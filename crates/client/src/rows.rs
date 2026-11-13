//! A received message as the archive row WAHA and the Python ingester would have made of it.
//!
//! The ids have to be theirs exactly, or the archive splits: a message seen by both would be
//! stored twice, and a chat written to under two ids would show as two. So:
//!
//! - `message_id` is WAHA's `buildMessageId` over the key Baileys' `cleanMessage` leaves:
//!   `{fromMe}_{chat}_{id}`, plus `_{participant}` in a group, each JID in WAHA's `toCusFormat`.
//! - `chat_id` is the id WAHA's chat list gives a conversation, which the ingester keyed rows by:
//!   a group's JID, and for a direct chat the counterpart's *phone number* JID, even when the
//!   message itself is addressed by LID.
//! - `author_id` is the ingester's `author_of`: the participant in a group, otherwise WAHA's
//!   `from`, which in a direct chat is the counterpart, also for this account's own messages.
//!
//! `tests/rows.rs` checks ids and authors against WAHA's own `buildMessageId` and `toCusFormat`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use whatsapp_core::models::MessageRow;

use crate::binary::{Jid, Node, Value};
use crate::content::{Columns, Target};
use crate::media::MediaRef;
use crate::stanza::{MessageInfo, MessageKind, parse_jid};

/// Phone-number users by LID user, from the session's LID mappings.
pub type PnByLid = HashMap<String, String>;

/// Baileys' `cleanMessage`: device dropped, hosted servers folded into the plain ones.
fn cleaned(jid: &Jid) -> (String, String) {
    let server = match jid.server.as_str() {
        "hosted" => "s.whatsapp.net",
        "hosted.lid" => "lid",
        "c.us" => "s.whatsapp.net",
        other => other,
    };
    (jid.user.clone(), server.to_owned())
}

/// WAHA's `toCusFormat` on a cleaned JID.
fn cus((user, server): &(String, String)) -> String {
    match server.as_str() {
        "s.whatsapp.net" => format!("{user}@c.us"),
        _ => format!("{user}@{server}"),
    }
}

/// WAHA's `buildMessageId`.
pub fn waha_message_id(info: &MessageInfo) -> String {
    let chat = cus(&cleaned(&info.chat));
    let mut id = format!("{}_{chat}_{}", info.from_me, info.id);
    if let Some(participant) = &info.participant {
        id.push('_');
        id.push_str(&cus(&cleaned(participant)));
    }
    id
}

/// The archive id of the message an edit or deletion is about. Baileys files both under the
/// *outer* message's key with the target's id (`messages.update` in `process-message.js`), so
/// a contact's edit keeps the chat, `fromMe` and participant of the edit itself.
pub fn edit_target_id(info: &MessageInfo, target_id: &str) -> String {
    waha_message_id(&MessageInfo {
        id: target_id.to_owned(),
        ..info.clone()
    })
}

/// The archive id of the message a reaction is to: Baileys' `normaliseKey` (the key turned to
/// this account's perspective when someone else reacted), then WAHA's `buildMessageId`.
pub fn reaction_target_id(
    info: &MessageInfo,
    target: &Target,
    me: &Jid,
    me_lid: Option<&Jid>,
) -> String {
    let parse = |text: &Option<String>| text.as_deref().and_then(parse_jid);
    let (chat, from_me, participant) = if info.from_me {
        // Our own reaction from the phone: the key is already ours.
        (
            parse(&target.remote_jid).unwrap_or_else(|| info.chat.clone()),
            target.from_me,
            parse(&target.participant),
        )
    } else {
        let who = parse(&target.participant).or_else(|| parse(&target.remote_jid));
        let is_me =
            who.is_some_and(|j| j.user == me.user || me_lid.is_some_and(|l| j.user == l.user));
        let from_me = !target.from_me && is_me;
        (
            info.chat.clone(),
            from_me,
            parse(&target.participant).or_else(|| info.participant.clone()),
        )
    };
    let mut id = format!("{from_me}_{}_{}", cus(&cleaned(&chat)), target.id);
    if let Some(p) = participant {
        id.push('_');
        id.push_str(&cus(&cleaned(&p)));
    }
    id
}

/// Whoever sent `info`, in the archive's form: the actor of a reaction, the author of a row.
pub fn sender_id(info: &MessageInfo) -> String {
    cus(&cleaned(&info.author))
}

/// The ingester's `author_of` over WAHA's payload.
pub fn waha_author(info: &MessageInfo) -> Option<String> {
    match (&info.participant, info.kind) {
        (Some(participant), _) => Some(cus(&cleaned(participant))),
        (None, MessageKind::Group) => None,
        (None, _) => Some(cus(&cleaned(&info.chat))),
    }
}

/// The archive's chat for a message, or `None` for what the archive does not keep: the
/// ingester only walks WAHA's chat list, which has no status updates, broadcasts or channels.
pub fn chat_id(node: &Node, info: &MessageInfo, pn_by_lid: &PnByLid) -> Option<String> {
    let (user, server) = cleaned(&info.chat);
    match server.as_str() {
        "g.us" => Some(format!("{user}@g.us")),
        "s.whatsapp.net" => Some(format!("{user}@s.whatsapp.net")),
        "lid" => {
            // The stanza names the counterpart's number when it can; the session's LID map
            // covers the rest. Without either there is no phone-number chat to file it under.
            let alternate = if info.from_me {
                "recipient_pn"
            } else {
                "sender_pn"
            };
            let from_stanza = match node
                .get(alternate)
                .or_else(|| node.get("peer_recipient_pn"))
            {
                Some(Value::Jid(j)) => Some(j.user.clone()),
                Some(Value::Str(s)) => s.split(['@', ':']).next().map(str::to_owned),
                None => None,
            };
            let pn = from_stanza.or_else(|| pn_by_lid.get(&user).cloned());
            Some(match pn {
                Some(pn) => format!("{pn}@s.whatsapp.net"),
                None => format!("{user}@lid"),
            })
        }
        _ => None,
    }
}

/// The row, or `None` when the archive does not keep this chat.
pub fn message_row(
    node: &Node,
    info: &MessageInfo,
    columns: &Columns,
    pn_by_lid: &PnByLid,
) -> Option<MessageRow> {
    let chat_id = chat_id(node, info, pn_by_lid)?;
    let t: i64 = node.get("t").and_then(|t| t.to_string().parse().ok())?;
    let ts = DateTime::<Utc>::from_timestamp(t, 0)?;
    Some(MessageRow {
        chat_id,
        message_id: waha_message_id(info),
        ts,
        from_me: info.from_me,
        author_id: waha_author(info),
        body: columns.body.clone(),
        kind: columns.kind.to_owned(),
        reply_to_id: columns.reply_to_id.clone(),
        // Baileys' status less one: delivered for what arrives, server-acknowledged for our
        // own; receipts move both to read later.
        ack: Some(if info.from_me { 1 } else { 2 }),
        has_media: columns.has_media,
        media_mime: columns.media_mime.clone(),
        media_filename: columns.media_filename.clone(),
        media_seconds: columns.media_seconds,
        transcript_state: if columns.kind == "audio" {
            "pending"
        } else {
            "n/a"
        }
        .to_owned(),
        raw: serde_json::json!({
            "source": "whatsapp-client",
            "id": info.id,
            "chat": info.chat.to_string(),
            "participant": info.participant.as_ref().map(ToString::to_string),
            "fromMe": info.from_me,
            "t": t,
            "notify": node.get("notify").map(ToString::to_string),
            "category": info.category,
            "media": columns.audio.as_ref().map(MediaRef::to_json),
            // What a later edit is encrypted under, and who sent it: the edit's key names both.
            "messageSecret": columns.secret.as_ref().map(|b| B64.encode(b)),
            "sender": crate::secret::account(&info.author),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(chat: &str, from_me: bool) -> MessageInfo {
        let chat = parse_jid(chat).unwrap();
        MessageInfo {
            id: "X".into(),
            author: chat.clone(),
            chat,
            from_me,
            participant: None,
            category: None,
            kind: MessageKind::Chat,
        }
    }

    #[test]
    fn a_lid_chat_is_filed_under_the_counterparts_phone_number() {
        let map: PnByLid = [("222".to_owned(), "48222".to_owned())].into();
        let bare = Node::new("message");
        // From the stanza first, then the session's map, else the LID itself.
        let incoming = Node::new("message").attr("sender_pn", Jid::pair("48111", "s.whatsapp.net"));
        assert_eq!(
            chat_id(&incoming, &info("111@lid", false), &map).as_deref(),
            Some("48111@s.whatsapp.net")
        );
        let own = Node::new("message").attr("recipient_pn", Jid::pair("48333", "s.whatsapp.net"));
        assert_eq!(
            chat_id(&own, &info("333@lid", true), &map).as_deref(),
            Some("48333@s.whatsapp.net")
        );
        assert_eq!(
            chat_id(&bare, &info("222@lid", false), &map).as_deref(),
            Some("48222@s.whatsapp.net")
        );
        assert_eq!(
            chat_id(&bare, &info("999@lid", false), &map).as_deref(),
            Some("999@lid")
        );
        assert_eq!(
            chat_id(&bare, &info("48444:3@s.whatsapp.net", false), &map).as_deref(),
            Some("48444@s.whatsapp.net")
        );
        assert_eq!(
            chat_id(&bare, &info("1203@g.us", false), &map).as_deref(),
            Some("1203@g.us")
        );
        assert_eq!(
            chat_id(&bare, &info("status@broadcast", false), &map),
            None,
            "status updates are not archived"
        );
    }
}
