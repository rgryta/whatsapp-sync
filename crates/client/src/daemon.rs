//! The long-running client: WAHA's device, archiving straight into Postgres.
//!
//! One loop: connect and log in, run WAHA's post-login sequence, then handle what arrives until
//! the connection ends, and connect again. What it answers is what WAHA's Baileys answers, from
//! [`stanza`] and [`prekeys`]; what it archives is what the Python ingester archived, from
//! [`rows`]. The order of work for a message is what makes a crash harmless:
//!
//! 1. archive it (idempotent: a re-delivery upserts the same row);
//! 2. save the Signal state the decryption advanced;
//! 3. send the receipt, after which WhatsApp stops re-delivering.
//!
//! A crash before 3 means WhatsApp sends the message again; before 2, it decrypts again too.
//!
//! Not handled yet, each named where it would go: read-self receipts (the archive's `ack` for
//! incoming messages read on the phone), group subject and contact name changes, app-state
//! sync, history sync, and the placeholder resend Baileys asks its own phone for.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use whatsapp_core::store::writes;

use crate::baileys::Device;
use crate::binary::{Content, Jid, Node, Value};
use crate::conn::{self, ConnError, Outbox};
use crate::content::{self, Event, Target};
use crate::login::LoginSettings;
use crate::rows::{self, PnByLid};
use crate::signal::curve::KeyPair;
use crate::signal::{Address, SignalError, Store};
use crate::stanza::{self, MessageInfo, NACK_UNHANDLED_ERROR, RetryKeys, parse_jid};
use crate::{prekeys, secret, store};

/// WAHA's `keepAliveIntervalMs`; Baileys calls the connection lost 5 s after a missed answer.
const KEEPALIVE: Duration = Duration::from_secs(30);
const LOST_AFTER: Duration = Duration::from_secs(35);
/// How often a live connection tells `status` it is still running: the Python sweep's cadence,
/// so a connection open for days does not read as a loop that stopped at login.
const HEARTBEAT: Duration = Duration::from_secs(120);
/// Baileys' `maxMsgRetryCount`.
const MAX_RETRIES: u32 = 5;
/// Baileys' `NACK_REASONS.MissingMessageSecret`, for message types it ignores.
const NACK_MISSING_SECRET: u16 = 495;
/// The `sync_state` loop name the MCP's `status` reports.
const LOOP: &str = "sync";

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Conn(#[from] ConnError),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Store(#[from] store::StoreError),
}

/// Why a connection ended, when it was not an error.
pub enum Ended {
    /// Lost or closed; connect again.
    Closed,
    /// This device was logged out from the phone. Connecting again cannot help.
    LoggedOut,
    /// Another client took over this device (WAHA started again?). Fighting it would only
    /// make the two knock each other off in turn.
    Replaced,
}

/// What an iq this client sent was for, so its answer can be read.
enum Pending {
    PreKeyCount,
    Other,
}

pub struct Client {
    pool: PgPool,
    device: Device,
    signal: Store,
    me: Jid,
    me_lid: Option<Jid>,
    pn_by_lid: PnByLid,
    settings: LoginSettings,
    retries: HashMap<String, u32>,
    pending: HashMap<String, Pending>,
}

impl Client {
    pub fn new(pool: PgPool, device: Device, settings: LoginSettings) -> Self {
        let signal = device.signal_store();
        let me_info = device.me.as_ref();
        let me = me_info
            .and_then(|m| parse_jid(&m.id))
            .unwrap_or_else(|| Jid::pair("", "s.whatsapp.net"));
        let me_lid = me_info.and_then(|m| m.lid.as_deref()).and_then(parse_jid);
        let pn_by_lid = device
            .lid_mappings
            .iter()
            .map(|(pn, lid)| {
                (
                    lid.split(['@', ':']).next().unwrap_or(lid).to_owned(),
                    pn.clone(),
                )
            })
            .collect();
        Self {
            pool,
            device,
            signal,
            me,
            me_lid,
            pn_by_lid,
            settings,
            retries: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    /// Runs until this device is logged out or replaced. Reconnects after anything else,
    /// backing off up to a minute.
    pub async fn run(mut self) {
        let mut backoff = Duration::from_secs(2);
        loop {
            let started = Instant::now();
            match self.serve().await {
                Ok(Ended::LoggedOut) => {
                    return self.park("this device was logged out from the phone").await;
                }
                Ok(Ended::Replaced) => {
                    return self.park("another client took over this device").await;
                }
                Ok(Ended::Closed) => tracing::warn!("connection closed"),
                Err(e) => {
                    tracing::error!("connection failed: {e}");
                    let _ = writes::set_sync_state(&self.pool, LOOP, Some(&e.to_string())).await;
                }
            }
            // A connection that lasted is not a failing one: start the backoff over.
            if started.elapsed() > Duration::from_secs(300) {
                backoff = Duration::from_secs(2);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// Records why, then waits forever: a restart loop would only repeat the refusal.
    async fn park(&self, why: &str) {
        tracing::error!("{why}; not reconnecting");
        let _ = writes::set_sync_state(&self.pool, LOOP, Some(why)).await;
        std::future::pending::<()>().await;
    }

    async fn serve(&mut self) -> Result<Ended, DaemonError> {
        let (mut conn, first) = conn::connect(&self.device, &self.settings).await?;
        match first.tag.as_str() {
            "success" => tracing::info!("logged in"),
            "failure" if first.get("reason").map(ToString::to_string).as_deref() == Some("401") => {
                return Ok(Ended::LoggedOut);
            }
            other => {
                tracing::error!(
                    "login refused: <{other}> reason {:?}",
                    first.get("reason").map(ToString::to_string)
                );
                return Ok(Ended::Closed);
            }
        }
        writes::set_sync_state(&self.pool, LOOP, None).await?;
        self.post_login(&mut conn).await?;
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.tick().await;
        let mut last_heard = Instant::now();
        let mut last_beat = Instant::now();
        loop {
            tokio::select! {
                _ = keepalive.tick() => {
                    if last_heard.elapsed() > LOST_AFTER {
                        return Ok(Ended::Closed);
                    }
                    let ping = self.iq(&mut conn, "w:p", "get", vec![Node::new("ping")], Pending::Other);
                    conn.send(&ping).await?;
                    if last_beat.elapsed() >= HEARTBEAT {
                        writes::set_sync_state(&self.pool, LOOP, None).await?;
                        last_beat = Instant::now();
                    }
                }
                node = conn.recv() => {
                    let Some(node) = node? else { return Ok(Ended::Closed) };
                    last_heard = Instant::now();
                    if let Some(ended) = self.handle(&mut conn, node).await? {
                        return Ok(ended);
                    }
                }
            }
        }
    }

    /// WAHA's sequence after `<success>` (Baileys `socket.js`, `chats.js`).
    pub async fn post_login(&mut self, conn: &mut impl Outbox) -> Result<(), DaemonError> {
        let requests = [
            (
                "encrypt",
                "get",
                vec![Node::new("count")],
                Pending::PreKeyCount,
            ),
            ("passive", "set", vec![Node::new("active")], Pending::Other),
            ("encrypt", "get", vec![Node::new("digest")], Pending::Other),
        ];
        for (xmlns, kind, content, pending) in requests {
            let node = self.iq(conn, xmlns, kind, content, pending);
            conn.send(&node).await?;
        }
        let unified = Node {
            content: Content::Nodes(vec![
                Node::new("unified_session").attr("id", unified_session_id()),
            ]),
            ..Node::new("ib")
        };
        conn.send(&unified).await?;
        let props = self.iq(
            conn,
            "abt",
            "get",
            vec![Node::new("props").attr("protocol", "1")],
            Pending::Other,
        );
        conn.send(&props).await?;
        let blocklist = self.iq(conn, "blocklist", "get", Vec::new(), Pending::Other);
        conn.send(&blocklist).await?;
        let privacy = self.iq(
            conn,
            "privacy",
            "get",
            vec![Node::new("privacy")],
            Pending::Other,
        );
        conn.send(&privacy).await?;
        if let Some(name) = self.device.me.as_ref().and_then(|m| m.name.as_ref()) {
            conn.send(
                &Node::new("presence")
                    .attr("name", name.replace('@', ""))
                    .attr("type", "unavailable"),
            )
            .await?;
        }
        Ok(())
    }

    fn iq(
        &mut self,
        conn: &mut impl Outbox,
        xmlns: &str,
        kind: &str,
        content: Vec<Node>,
        pending: Pending,
    ) -> Node {
        let id = conn.next_tag();
        self.pending.insert(id.clone(), pending);
        let mut node = Node::new("iq")
            .attr("id", id)
            .attr("to", stanza::server())
            .attr("type", kind)
            .attr("xmlns", xmlns);
        if !content.is_empty() {
            node.content = Content::Nodes(content);
        }
        node
    }

    /// One node from the server. Public so the tests can drive it with a recorder.
    pub async fn handle(
        &mut self,
        conn: &mut impl Outbox,
        node: Node,
    ) -> Result<Option<Ended>, DaemonError> {
        match node.tag.as_str() {
            "message" => self.message(conn, &node).await?,
            "receipt" => {
                self.receipt(&node).await?;
                conn.send(&stanza::ack(&node, None, &self.me)).await?;
            }
            "notification" => {
                let low = node.get("type").map(ToString::to_string).as_deref() == Some("encrypt")
                    && node.get("from").is_some_and(|f| {
                        f.to_string() == "s.whatsapp.net" || f.to_string() == "@s.whatsapp.net"
                    })
                    && node
                        .child("count")
                        .and_then(|c| c.get("value"))
                        .and_then(|v| v.to_string().parse::<u32>().ok())
                        .is_some_and(|n| n < prekeys::MIN_PRE_KEYS);
                if low {
                    self.upload_pre_keys(conn, prekeys::MIN_PRE_KEYS).await?;
                }
                conn.send(&stanza::ack(&node, None, &self.me)).await?;
            }
            "call" => conn.send(&stanza::ack(&node, None, &self.me)).await?,
            "iq" => self.iq_answer(conn, &node).await?,
            "ib" => self.ib(conn, &node).await?,
            "stream:error" => {
                let replaced = node
                    .child("conflict")
                    .and_then(|c| c.get("type"))
                    .is_some_and(|t| t.to_string() == "replaced");
                tracing::warn!(
                    "stream error: {:?}",
                    node.children().first().map(|c| c.tag.clone())
                );
                return Ok(Some(if replaced {
                    Ended::Replaced
                } else {
                    Ended::Closed
                }));
            }
            "failure" => {
                let reason = node.get("reason").map(ToString::to_string);
                return Ok(Some(if reason.as_deref() == Some("401") {
                    Ended::LoggedOut
                } else {
                    Ended::Closed
                }));
            }
            "xmlstreamend" => return Ok(Some(Ended::Closed)),
            _ => {}
        }
        Ok(None)
    }

    async fn iq_answer(&mut self, conn: &mut impl Outbox, node: &Node) -> Result<(), DaemonError> {
        let Some(pending) = node
            .get("id")
            .and_then(|id| self.pending.remove(&id.to_string()))
        else {
            return Ok(());
        };
        if let Pending::PreKeyCount = pending {
            let count: u32 = node
                .child("count")
                .and_then(|c| c.get("value"))
                .and_then(|v| v.to_string().parse().ok())
                .unwrap_or(0);
            tracing::info!("{count} one-time pre-keys on the server");
            if count <= prekeys::MIN_PRE_KEYS {
                let want = if count == 0 {
                    prekeys::INITIAL_PRE_KEYS
                } else {
                    prekeys::MIN_PRE_KEYS
                };
                self.upload_pre_keys(conn, want).await?;
            }
        }
        Ok(())
    }

    async fn ib(&mut self, conn: &mut impl Outbox, node: &Node) -> Result<(), DaemonError> {
        if node.child("offline_preview").is_some() {
            let batch = Node::new("offline_batch").attr("count", "100");
            conn.send(&Node {
                content: Content::Nodes(vec![batch]),
                ..Node::new("ib")
            })
            .await?;
        }
        if let Some(Content::Bytes(info)) = node
            .child("edge_routing")
            .and_then(|e| e.child("routing_info"))
            .map(|r| &r.content)
        {
            store::set_routing_info(&self.pool, info).await?;
            self.device.routing_info = Some(info.clone());
        }
        if let Some(count) = node.child("offline").and_then(|o| o.get("count")) {
            tracing::info!("offline queue delivered: {count}");
        }
        Ok(())
    }

    /// Baileys' `uploadPreKeys`: generate the shortfall, save it, then send. Saved first because
    /// a key the server hands out that this device lost would fail every session started on it.
    async fn upload_pre_keys(
        &mut self,
        conn: &mut impl Outbox,
        count: u32,
    ) -> Result<(), DaemonError> {
        let plan = prekeys::plan(
            self.device.next_pre_key_id,
            self.device.first_unuploaded_pre_key_id,
            count,
            KeyPair::generate,
        );
        store::save_pre_keys(&self.pool, &plan.new_keys).await?;
        for (id, key) in &plan.new_keys {
            self.signal.pre_keys.insert(*id, key.clone());
        }
        store::set_pre_key_counters(
            &self.pool,
            plan.next_pre_key_id,
            plan.first_unuploaded_pre_key_id,
        )
        .await?;
        self.device.next_pre_key_id = plan.next_pre_key_id;
        self.device.first_unuploaded_pre_key_id = plan.first_unuploaded_pre_key_id;
        let keys: Vec<(u32, &KeyPair)> = plan
            .upload
            .filter_map(|id| self.signal.pre_keys.get(&id).map(|k| (id, k)))
            .collect();
        let mut upload = prekeys::upload_node(
            self.device.registration_id,
            &self.device.identity.public,
            &keys,
            &self.device.signed_pre_key,
        );
        let id = conn.next_tag();
        upload.attrs.push(("id".into(), Value::Str(id.clone())));
        self.pending.insert(id, Pending::Other);
        tracing::info!("uploading {} one-time pre-keys", keys.len());
        conn.send(&upload).await?;
        Ok(())
    }

    async fn message(&mut self, conn: &mut impl Outbox, node: &Node) -> Result<(), DaemonError> {
        let info = match stanza::message_info(node, &self.me, self.me_lid.as_ref()) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("unreadable message stanza: {e}");
                conn.send(&stanza::ack(node, Some(NACK_UNHANDLED_ERROR), &self.me))
                    .await?;
                return Ok(());
            }
        };
        let encs: Vec<&Node> = node.children().iter().filter(|c| c.tag == "enc").collect();
        if encs
            .iter()
            .any(|e| e.get("type").is_some_and(|t| t.to_string() == "msmsg"))
        {
            conn.send(&stanza::ack(node, Some(NACK_MISSING_SECRET), &self.me))
                .await?;
            return Ok(());
        }
        self.learn_lid(node, &info).await?;
        let address = self.address(&info.author);
        let group_key = (info.chat.to_string(), address.clone());
        let (mut events, mut decrypted, mut failed) = (Vec::new(), false, None);
        for enc in encs {
            let Content::Bytes(ciphertext) = &enc.content else {
                continue;
            };
            let result = match enc.get("type").map(ToString::to_string).as_deref() {
                Some("pkmsg") => self.signal.decrypt_prekey(&address, ciphertext),
                Some("msg") => self.signal.decrypt(&address, ciphertext),
                Some("skmsg") => match self.device.sender_keys.get_mut(&group_key) {
                    Some(record) => record.decrypt(ciphertext).map_err(|e| match e {
                        crate::group::GroupError::Duplicate(counter) => {
                            SignalError::Duplicate { counter }
                        }
                        other => SignalError::InvalidMessage(group_error(&other)),
                    }),
                    None => Err(SignalError::InvalidMessage("no sender key for this member")),
                },
                _ => Err(SignalError::InvalidMessage("unknown enc type")),
            };
            match result {
                Ok(padded) => {
                    decrypted = true;
                    let Some(message) = content::unpad(&padded).and_then(content::decode) else {
                        continue;
                    };
                    if let Some(skdm) = &message.sender_key_distribution_message
                        && let (Some(group), Some(bytes)) = (
                            &skdm.group_id,
                            &skdm.axolotl_sender_key_distribution_message,
                        )
                    {
                        let key = (group.clone(), address.clone());
                        if self
                            .device
                            .sender_keys
                            .entry(key.clone())
                            .or_default()
                            .process_distribution(bytes)
                            .is_ok()
                        {
                            store::save_sender_key(
                                &self.pool,
                                &key.0,
                                &key.1,
                                &self.device.sender_keys[&key],
                            )
                            .await?;
                        }
                    }
                    let event = content::event(&message);
                    if event == Event::Other {
                        // What this client does not read yet: named, never shown.
                        if let Some(protocol) = &message.protocol_message {
                            tracing::info!(
                                "ignored a protocol message of type {:?}",
                                protocol.r#type
                            );
                        } else if !content::is_only_protocol(&message) {
                            tracing::info!(
                                "kept nothing from a message with fields {:?}",
                                content::unpad(&padded).map(content::field_numbers)
                            );
                        }
                    }
                    events.push(event);
                }
                // Already decrypted, before a crash took the receipt with it: answer as if new.
                Err(SignalError::Duplicate { .. }) => decrypted = true,
                Err(e) => failed = Some(e),
            }
        }
        if !decrypted {
            if let Some(e) = failed {
                tracing::warn!("could not decrypt a message: {e}");
            }
            return self.retry(conn, node).await;
        }
        for event in events {
            self.archive(node, &info, event).await?;
        }
        self.save_signal_state(&address, &group_key).await?;
        self.retries.remove(&info.id);
        conn.send(&stanza::after_decrypt(node, &info, &self.me))
            .await?;
        Ok(())
    }

    /// What decryption advanced, made durable before the receipt goes out.
    async fn save_signal_state(
        &mut self,
        address: &Address,
        group_key: &(String, Address),
    ) -> Result<(), DaemonError> {
        if let Some(record) = self.signal.sessions.get(address) {
            store::save_session(&self.pool, address, record).await?;
        }
        if let Some(identity) = self.signal.identities.get(address) {
            store::save_identity(&self.pool, address, identity).await?;
        }
        for id in std::mem::take(&mut self.signal.used_pre_keys) {
            store::remove_pre_key(&self.pool, id).await?;
        }
        if let Some(record) = self.device.sender_keys.get(group_key) {
            store::save_sender_key(&self.pool, &group_key.0, &group_key.1, record).await?;
        }
        Ok(())
    }

    /// Baileys' retry receipt, then a nack. From the second attempt the receipt carries a
    /// fresh pre-key and this device's identity, so the sender can start a new session.
    async fn retry(&mut self, conn: &mut impl Outbox, node: &Node) -> Result<(), DaemonError> {
        let id = node.get("id").map(ToString::to_string).unwrap_or_default();
        let count = {
            let n = self.retries.entry(id.clone()).or_insert(0);
            *n += 1;
            *n
        };
        if count > MAX_RETRIES {
            self.retries.remove(&id);
        } else {
            let keys = if count > 1 {
                self.retry_pre_key().await?
            } else {
                None
            };
            let receipt = match (&keys, &self.device.account) {
                (Some((pre_key_id, pre_key)), Some(account)) => stanza::retry_receipt(
                    node,
                    count,
                    self.device.registration_id,
                    Some(RetryKeys {
                        identity: &self.device.identity.public,
                        pre_key: (*pre_key_id, pre_key),
                        signed_pre_key: &self.device.signed_pre_key,
                        account,
                    }),
                ),
                _ => stanza::retry_receipt(node, count, self.device.registration_id, None),
            };
            conn.send(&receipt).await?;
        }
        conn.send(&stanza::ack(node, Some(NACK_UNHANDLED_ERROR), &self.me))
            .await?;
        Ok(())
    }

    /// Baileys' `getNextPreKeys(authState, 1)` for a retry receipt.
    async fn retry_pre_key(&mut self) -> Result<Option<(u32, KeyPair)>, DaemonError> {
        let plan = prekeys::plan(
            self.device.next_pre_key_id,
            self.device.first_unuploaded_pre_key_id,
            1,
            KeyPair::generate,
        );
        store::save_pre_keys(&self.pool, &plan.new_keys).await?;
        for (id, key) in &plan.new_keys {
            self.signal.pre_keys.insert(*id, key.clone());
        }
        store::set_pre_key_counters(
            &self.pool,
            plan.next_pre_key_id,
            plan.first_unuploaded_pre_key_id,
        )
        .await?;
        self.device.next_pre_key_id = plan.next_pre_key_id;
        self.device.first_unuploaded_pre_key_id = plan.first_unuploaded_pre_key_id;
        Ok(plan
            .upload
            .clone()
            .find_map(|id| self.signal.pre_keys.get(&id).map(|k| (id, k.clone()))))
    }

    async fn archive(
        &mut self,
        node: &Node,
        info: &MessageInfo,
        event: Event,
    ) -> Result<(), DaemonError> {
        let Some(chat_id) = rows::chat_id(node, info, &self.pn_by_lid) else {
            return Ok(());
        };
        match event {
            Event::Message(columns) => {
                let Some(row) = rows::message_row(node, info, &columns, &self.pn_by_lid) else {
                    return Ok(());
                };
                let kind = if chat_id.ends_with("@g.us") {
                    "group"
                } else {
                    "dm"
                };
                writes::ensure_chat(&self.pool, &chat_id, kind).await?;
                writes::upsert_message(&self.pool, &row).await?;
                writes::set_watermark(&self.pool, &chat_id, row.ts).await?;
                // The ingester's `observed_participant`: a group author the address book may lack.
                if let Some(author) = row
                    .author_id
                    .as_deref()
                    .filter(|a| !info.from_me && *a != chat_id && info.participant.is_some())
                {
                    let lid = author.ends_with("@lid").then_some(author);
                    let push = node.get("notify").map(ToString::to_string);
                    writes::upsert_observed_participant(&self.pool, author, lid, push.as_deref())
                        .await?;
                }
            }
            Event::Edit { target, body } => {
                if let Some(id) = self.target(&chat_id, info, &target.id).await? {
                    writes::record_revision(&self.pool, &chat_id, &id, body.as_deref()).await?;
                }
            }
            Event::SecretEdit {
                target,
                iv,
                payload,
            } => {
                self.secret_edit(&chat_id, info, &target, &iv, &payload)
                    .await?
            }
            Event::Revoke { target } => {
                if let Some(id) = self.target(&chat_id, info, &target.id).await? {
                    writes::mark_revoked(&self.pool, &chat_id, &id).await?;
                }
            }
            Event::Reaction {
                target,
                emoji,
                ts_ms,
            } => {
                self.reaction(info, &chat_id, &target, &emoji, ts_ms)
                    .await?
            }
            Event::Other => {}
        }
        Ok(())
    }

    async fn reaction(
        &mut self,
        info: &MessageInfo,
        chat_id: &str,
        target: &Target,
        emoji: &str,
        ts_ms: Option<i64>,
    ) -> Result<(), DaemonError> {
        let message_id = rows::reaction_target_id(info, target, &self.me, self.me_lid.as_ref());
        let actor = rows::sender_id(info);
        if emoji.is_empty() {
            writes::remove_reaction(&self.pool, chat_id, &message_id, &actor).await?;
        } else {
            let ts = ts_ms.and_then(DateTime::<Utc>::from_timestamp_millis);
            writes::set_reaction(&self.pool, chat_id, &message_id, &actor, emoji, ts).await?;
        }
        Ok(())
    }

    /// An edit sealed with the original's secret: opened, then recorded like any edit.
    async fn secret_edit(
        &self,
        chat_id: &str,
        info: &MessageInfo,
        target: &Target,
        iv: &[u8],
        payload: &[u8],
    ) -> Result<(), DaemonError> {
        let Some(id) = self.target(chat_id, info, &target.id).await? else {
            return Ok(());
        };
        let raw = writes::message_raw(&self.pool, chat_id, &id)
            .await?
            .unwrap_or_default();
        let Some(secret) = secret::from_raw(&raw) else {
            tracing::warn!("an edit of a message archived without its secret; ignored");
            return Ok(());
        };
        let editor = secret::account(&info.author);
        let me: Vec<String> = [Some(&self.me), self.me_lid.as_ref()]
            .into_iter()
            .flatten()
            .map(secret::account)
            .collect();
        // whatsmeow's choice first (the editor for our own, else the key's sender), then every
        // other form the original sender may have been named in.
        let mut originals = Vec::new();
        if target.from_me {
            originals.push(editor.clone());
            originals.extend(me.iter().cloned());
        }
        for jid in [&target.participant, &target.remote_jid] {
            if let Some(jid) = jid.as_deref().and_then(parse_jid) {
                originals.push(secret::account(&jid));
            }
        }
        if let Some(sender) = raw.get("sender").and_then(serde_json::Value::as_str) {
            originals.push(sender.to_owned());
        }
        let mut editors = vec![editor];
        if info.from_me {
            editors.extend(me);
        }
        let Some(plain) = secret::open(&secret, &target.id, &originals, &editors, iv, payload)
        else {
            tracing::warn!("an edit did not decrypt under its message's secret; ignored");
            return Ok(());
        };
        let body = match content::decode(&plain).map(|m| content::event(&m)) {
            Some(Event::Edit { body, .. }) => body,
            Some(Event::Message(columns)) => columns.body,
            _ => {
                tracing::warn!("a decrypted edit held no message; ignored");
                return Ok(());
            }
        };
        writes::record_revision(&self.pool, chat_id, &id, body.as_deref()).await?;
        Ok(())
    }

    /// The archived message an edit or deletion is about, if it was archived.
    async fn target(
        &self,
        chat_id: &str,
        info: &MessageInfo,
        stanza_id: &str,
    ) -> Result<Option<String>, DaemonError> {
        let expected = rows::edit_target_id(info, stanza_id);
        let found = writes::resolve_message_id(&self.pool, chat_id, &expected, stanza_id).await?;
        if found.is_none() {
            tracing::warn!("an edit or deletion of a message that is not archived; ignored");
        }
        Ok(found)
    }

    /// Receipts move `ack` on this account's own messages: delivered (2), read (3), played (4).
    async fn receipt(&mut self, node: &Node) -> Result<(), DaemonError> {
        let ack = match node.get("type").map(ToString::to_string).as_deref() {
            None | Some("inactive") => 2,
            Some("read") => 3,
            Some("played") => 4,
            // shortcut: read-self (incoming messages read on the phone) is not applied yet.
            _ => return Ok(()),
        };
        let Some(Value::Jid(from)) = node.get("from") else {
            return Ok(());
        };
        let mut ids = vec![node.get("id").map(ToString::to_string).unwrap_or_default()];
        if let Some(list) = node.child("list") {
            ids.extend(
                list.children()
                    .iter()
                    .filter_map(|i| i.get("id").map(ToString::to_string)),
            );
        }
        let group = from.server == "g.us";
        // Our own message, in whichever form the archive holds it.
        let participants: Vec<Option<Jid>> = if group {
            vec![self.me_lid.clone(), Some(self.me.clone())]
        } else {
            vec![None]
        };
        let probe = MessageInfo {
            id: String::new(),
            chat: stanza::normalized(from),
            from_me: true,
            participant: None,
            author: self.me.clone(),
            category: None,
            kind: if group {
                stanza::MessageKind::Group
            } else {
                stanza::MessageKind::Chat
            },
        };
        let Some(chat_id) = rows::chat_id(node, &probe, &self.pn_by_lid) else {
            return Ok(());
        };
        for id in ids.iter().filter(|id| !id.is_empty()) {
            for participant in &participants {
                let info = MessageInfo {
                    id: id.clone(),
                    participant: participant.clone(),
                    ..probe.clone()
                };
                writes::raise_ack(&self.pool, &chat_id, &rows::waha_message_id(&info), ack).await?;
            }
        }
        Ok(())
    }

    /// A LID-addressed stanza names the sender's phone number too: remember the pair, as
    /// Baileys' `storeLIDPNMappings` does, so later direct chats file under the number.
    async fn learn_lid(&mut self, node: &Node, info: &MessageInfo) -> Result<(), DaemonError> {
        let user = |key| match node.get(key) {
            Some(Value::Jid(j)) => Some(j.user.clone()),
            Some(Value::Str(s)) => parse_jid(s).map(|j| j.user),
            None => None,
        };
        if info.author.server != "lid" {
            return Ok(());
        }
        let Some(pn) = user("participant_pn").or_else(|| user("sender_pn")) else {
            return Ok(());
        };
        if self.pn_by_lid.get(&info.author.user) != Some(&pn) {
            store::save_lid_mapping(&self.pool, &pn, &info.author.user).await?;
            self.pn_by_lid.insert(info.author.user.clone(), pn.clone());
            self.device
                .lid_mappings
                .insert(pn, info.author.user.clone());
        }
        Ok(())
    }

    /// Baileys' `jidToSignalProtocolAddress` with its PN-to-LID step: sessions live under LIDs.
    fn address(&self, jid: &Jid) -> Address {
        let device = u32::from(jid.device_number());
        match jid.server.as_str() {
            "s.whatsapp.net" => match self.device.lid_mappings.get(&jid.user) {
                Some(lid) => Address {
                    name: format!("{}_1", lid.split(['@', ':']).next().unwrap_or(lid)),
                    device,
                },
                None => Address {
                    name: jid.user.clone(),
                    device,
                },
            },
            "lid" => Address {
                name: format!("{}_1", jid.user),
                device,
            },
            "hosted" => Address {
                name: format!("{}_128", jid.user),
                device,
            },
            "hosted.lid" => Address {
                name: format!("{}_129", jid.user),
                device,
            },
            _ => Address {
                name: jid.user.clone(),
                device,
            },
        }
    }
}

fn group_error(e: &crate::group::GroupError) -> &'static str {
    match e {
        crate::group::GroupError::Signature => "group signature does not verify",
        crate::group::GroupError::NoState => "no sender key with this id",
        _ => "group message does not decrypt",
    }
}

/// Baileys' `getUnifiedSessionId`: three days ahead, modulo a week, in milliseconds.
fn unified_session_id() -> String {
    const DAY: u64 = 86_400_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    ((now + 3 * DAY) % (7 * DAY)).to_string()
}
