//! Connects to WhatsApp as WAHA's device, from a COPY of WAHA's session, and changes nothing.
//!
//!     whatsapp-connect-check <session copy> login       log in, report, disconnect
//!     whatsapp-connect-check <session copy> <seconds> [rows.jsonl]
//!         also run WAHA's post-login sequence and decrypt what arrives, in memory; with a path,
//!         write the archive row each message would become there, to compare with the rows WAHA
//!         and the ingester write for the same messages afterwards
//!
//! WAHA must be stopped first: WhatsApp keeps one connection per device and drops the other.
//!
//! What it deliberately does not do, so WAHA picks up exactly where it left off afterwards:
//! - acknowledge anything, so WhatsApp keeps every message queued for the next connection;
//! - upload pre-keys, which would exist only in the copy and leave WAHA unable to answer the
//!   contacts who used them;
//! - write the session anywhere.
//!
//! Prints counts, node tags and attribute names, never content, keys or numbers. The rows file
//! does hold message content: keep it on the machine the archive is on, and delete it after.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use whatsapp_client::baileys::{self, Device};
use whatsapp_client::binary::{Content, Jid, JidForm, Node};
use whatsapp_client::conn::{self, Connection};
use whatsapp_client::content::{self, Event};
use whatsapp_client::login::LoginSettings;
use whatsapp_client::rows::{self, PnByLid};
use whatsapp_client::signal::{Address, Store};
use whatsapp_client::stanza::{self, parse_jid};

/// WAHA's `keepAliveIntervalMs`.
const KEEPALIVE: Duration = Duration::from_secs(30);

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(mode)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: whatsapp-connect-check <session copy> login|<seconds>");
        return std::process::ExitCode::from(2);
    };
    let device = match baileys::import(dir.as_ref()) {
        Ok(device) => device,
        Err(e) => {
            eprintln!("import failed: {}", e.reason);
            return std::process::ExitCode::FAILURE;
        }
    };
    match run(device, mode, args.get(3).map(String::as_str)).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(device: Device, mode: &str, rows_path: Option<&str>) -> Result<(), conn::ConnError> {
    let (mut conn, first) = conn::connect(&device, &LoginSettings::default()).await?;
    println!(
        "login answer: <{}> attributes: {}",
        first.tag,
        names(&first)
    );
    if first.tag != "success" {
        if let Some(reason) = first.get("reason") {
            println!("failure reason: {reason}");
        }
        return conn.close().await;
    }
    if mode == "login" {
        println!("logged in; disconnecting without sending anything");
        return conn.close().await;
    }
    let seconds: u64 = mode.parse().unwrap_or(60);
    let mut session = Session::new(device, rows_path);
    session.post_login(&mut conn).await?;
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            () = &mut deadline => break,
            _ = keepalive.tick() => {
                let ping = iq(&mut conn, "w:p", "get").content(vec![Node::new("ping")]);
                conn.send(&ping).await?;
            }
            node = conn.recv() => match node? {
                Some(node) => session.handle(&mut conn, node).await?,
                None => { println!("server closed the connection"); break; }
            },
        }
    }
    session.report();
    conn.close().await
}

struct Session {
    device: Device,
    signal: Store,
    me: Jid,
    me_lid: Option<Jid>,
    pn_by_lid: PnByLid,
    rows: Option<std::fs::File>,
    tags: BTreeMap<String, usize>,
    decrypted: BTreeMap<String, usize>,
    failed: BTreeMap<String, usize>,
    events: BTreeMap<&'static str, usize>,
    rows_written: usize,
    sender_keys_received: usize,
}

impl Session {
    fn new(device: Device, rows_path: Option<&str>) -> Self {
        let signal = device.signal_store();
        let me_info = device.me.as_ref();
        let me = me_info
            .and_then(|m| parse_jid(&m.id))
            .unwrap_or_else(|| Jid::pair("", "s.whatsapp.net"));
        let me_lid = me_info.and_then(|m| m.lid.as_deref()).and_then(parse_jid);
        // The session keeps phone number -> LID; rows need the other direction.
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
        let rows = rows_path.map(|path| std::fs::File::create(path).expect("rows file"));
        Self {
            device,
            signal,
            me,
            me_lid,
            pn_by_lid,
            rows,
            tags: BTreeMap::new(),
            decrypted: BTreeMap::new(),
            failed: BTreeMap::new(),
            events: BTreeMap::new(),
            rows_written: 0,
            sender_keys_received: 0,
        }
    }

    /// WAHA's sequence after `<success>` (Baileys `socket.js` and `chats.js`), minus the upload.
    async fn post_login(&mut self, conn: &mut Connection) -> Result<(), conn::ConnError> {
        let count = iq(conn, "encrypt", "get").content(vec![Node::new("count")]);
        conn.send(&count).await?;
        let active = iq(conn, "passive", "set").content(vec![Node::new("active")]);
        conn.send(&active).await?;
        let digest = iq(conn, "encrypt", "get").content(vec![Node::new("digest")]);
        conn.send(&digest).await?;
        let unified = Node::new("ib").content(vec![
            Node::new("unified_session").attr("id", unified_session_id()),
        ]);
        conn.send(&unified).await?;
        let props = iq(conn, "abt", "get").content(vec![Node::new("props").attr("protocol", "1")]);
        conn.send(&props).await?;
        let blocklist = iq(conn, "blocklist", "get");
        conn.send(&blocklist).await?;
        let privacy = iq(conn, "privacy", "get").content(vec![Node::new("privacy")]);
        conn.send(&privacy).await?;
        if let Some(name) = self.device.me.as_ref().and_then(|m| m.name.as_ref()) {
            let presence = Node::new("presence")
                .attr("name", name.replace('@', ""))
                .attr("type", "unavailable");
            conn.send(&presence).await?;
        }
        println!("sent WAHA's post-login sequence (no pre-key upload)");
        Ok(())
    }

    async fn handle(&mut self, _conn: &mut Connection, node: Node) -> Result<(), conn::ConnError> {
        *self.tags.entry(node.tag.clone()).or_default() += 1;
        match node.tag.as_str() {
            "iq" => {
                let child = node
                    .children()
                    .first()
                    .map(|c| c.tag.as_str())
                    .unwrap_or("-");
                let detail = match child {
                    "count" => node.children()[0]
                        .get("value")
                        .map(|v| format!(" value={v}"))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                println!(
                    "iq {} <{child}>{detail}",
                    node.get("type")
                        .map(ToString::to_string)
                        .unwrap_or_default()
                );
            }
            "message" => self.message(&node),
            "stream:error" | "failure" => println!("<{}> {}", node.tag, names(&node)),
            _ => {}
        }
        Ok(())
    }

    fn message(&mut self, node: &Node) {
        let info = match stanza::message_info(node, &self.me, self.me_lid.as_ref()) {
            Ok(info) => info,
            Err(e) => {
                *self.failed.entry(format!("stanza: {e}")).or_default() += 1;
                return;
            }
        };
        let author = info.author.clone();
        let mut message_event = None;
        for enc in node.children().iter().filter(|c| c.tag == "enc") {
            let kind = enc.get("type").map(ToString::to_string).unwrap_or_default();
            let Content::Bytes(ciphertext) = &enc.content else {
                continue;
            };
            let result = match kind.as_str() {
                "pkmsg" => self
                    .signal
                    .decrypt_prekey(&self.address(&author), ciphertext)
                    .map_err(|e| e.to_string()),
                "msg" => self
                    .signal
                    .decrypt(&self.address(&author), ciphertext)
                    .map_err(|e| e.to_string()),
                "skmsg" => {
                    let key = (info.chat.to_string(), self.address(&author));
                    match self.device.sender_keys.get_mut(&key) {
                        Some(record) => record.decrypt(ciphertext).map_err(|e| e.to_string()),
                        None => Err("no sender key for this member".into()),
                    }
                }
                _ => Err(format!("unknown enc type {kind}")),
            };
            let padded = match result {
                Ok(padded) => padded,
                Err(reason) => {
                    *self.failed.entry(format!("{kind}: {reason}")).or_default() += 1;
                    continue;
                }
            };
            *self.decrypted.entry(kind).or_default() += 1;
            let Some(message) = content::unpad(&padded).and_then(content::decode) else {
                *self
                    .failed
                    .entry("content does not decode".into())
                    .or_default() += 1;
                continue;
            };
            if let Some(skdm) = &message.sender_key_distribution_message {
                self.take_sender_key(skdm, &author);
            }
            let event = content::event(&message);
            let label = match &event {
                Event::Message(_) => "message",
                Event::Edit { .. } | Event::SecretEdit { .. } => "edit",
                Event::Revoke { .. } => "revoke",
                Event::Reaction { .. } => "reaction",
                Event::Other => "other",
            };
            *self.events.entry(label).or_default() += 1;
            if let Event::Message(columns) = event {
                message_event.get_or_insert(columns);
            }
        }
        if let (Some(columns), Some(file)) = (message_event, self.rows.as_mut())
            && let Some(row) = rows::message_row(node, &info, &columns, &self.pn_by_lid)
        {
            serde_json::to_writer(&mut *file, &row).expect("rows file");
            file.write_all(b"\n").expect("rows file");
            self.rows_written += 1;
        }
    }

    /// A group member's sender key, which their later group messages need.
    fn take_sender_key(&mut self, skdm: &content::SenderKeyDistributionMessage, author: &Jid) {
        let (Some(group), Some(distribution)) = (
            &skdm.group_id,
            &skdm.axolotl_sender_key_distribution_message,
        ) else {
            return;
        };
        let key = (group.clone(), self.address(author));
        let record = self.device.sender_keys.entry(key).or_default();
        if record.process_distribution(distribution).is_ok() {
            self.sender_keys_received += 1;
        }
    }

    /// Baileys' `jidToSignalProtocolAddress`, with its PN-to-LID step: sessions live under LIDs.
    fn address(&self, jid: &Jid) -> Address {
        let device = u32::from(jid.device_number());
        let user = if jid.server == "s.whatsapp.net" {
            match self.device.lid_mappings.get(&jid.user) {
                Some(lid) => {
                    return Address {
                        name: format!("{}_1", lid.split(['@', ':']).next().unwrap_or(lid)),
                        device,
                    };
                }
                None => jid.user.clone(),
            }
        } else {
            let domain = match jid.form {
                JidForm::Ad { domain, .. } => domain,
                _ if jid.server == "lid" => 1,
                _ => 0,
            };
            if domain == 0 {
                jid.user.clone()
            } else {
                format!("{}_{domain}", jid.user)
            }
        };
        Address { name: user, device }
    }

    fn report(&self) {
        println!("nodes received by tag: {:?}", self.tags);
        println!("decrypted: {:?}", self.decrypted);
        println!("failed: {:?}", self.failed);
        println!("events: {:?}", self.events);
        println!("group sender keys received: {}", self.sender_keys_received);
        if self.rows.is_some() {
            println!("archive rows written: {}", self.rows_written);
        }
    }
}

fn iq(conn: &mut Connection, xmlns: &str, kind: &str) -> Node {
    Node::new("iq")
        .attr("id", conn.next_tag())
        .attr("to", stanza::server())
        .attr("type", kind)
        .attr("xmlns", xmlns)
}

/// Baileys' `getUnifiedSessionId`: three days ahead, modulo a week, in milliseconds.
fn unified_session_id() -> String {
    const DAY: u64 = 86_400_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    ((now + 3 * DAY) % (7 * DAY)).to_string()
}

/// Attribute names only: values can be phone numbers or LIDs.
fn names(node: &Node) -> String {
    node.attrs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

trait WithContent {
    fn content(self, children: Vec<Node>) -> Self;
}

impl WithContent for Node {
    fn content(mut self, children: Vec<Node>) -> Self {
        self.content = Content::Nodes(children);
        self
    }
}
