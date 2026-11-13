//! A decrypted message's content: the parts of WhatsApp's `Message` protobuf the archive keeps.
//!
//! The archive was filled by WAHA and the Python ingester, so its columns follow their rules,
//! and these follow those rules rather than the protobuf's own structure:
//!
//! - `body` is WAHA's `extractBody`: wrappers (disappearing, view-once, edit, …) unwrapped,
//!   then the text, a caption, or an edit's new text.
//! - `kind` and the media columns are the Python ingester's `to_message`: the first *top-level*
//!   field that names a kind. A wrapped image is therefore `kind = text` with `has_media = true`,
//!   because that is what the archive already says about every such message.
//! - `reply_to` is WAHA's `extractReplyTo`: the outer message's context info only.
//!
//! Field numbers are `WAWebProtobufsE2E.proto` (via whatsmeow).

use prost::Message as _;

use crate::media::MediaRef;

/// `ProtocolMessage.Type`.
const REVOKE: i32 = 0;
const MESSAGE_EDIT: i32 = 14;
/// `SecretEncryptedMessage.SecretEncType.MESSAGE_EDIT`.
const SECRET_MESSAGE_EDIT: i32 = 2;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Message {
    #[prost(string, optional, tag = "1")]
    pub conversation: Option<String>,
    #[prost(message, optional, tag = "2")]
    pub sender_key_distribution_message: Option<SenderKeyDistributionMessage>,
    #[prost(message, optional, boxed, tag = "3")]
    pub image_message: Option<Box<ImageMessage>>,
    #[prost(message, optional, tag = "4")]
    pub contact_message: Option<Named>,
    #[prost(message, optional, tag = "5")]
    pub location_message: Option<Location>,
    #[prost(message, optional, boxed, tag = "6")]
    pub extended_text_message: Option<Box<ExtendedText>>,
    #[prost(message, optional, boxed, tag = "7")]
    pub document_message: Option<Box<DocumentMessage>>,
    #[prost(message, optional, boxed, tag = "8")]
    pub audio_message: Option<Box<AudioMessage>>,
    #[prost(message, optional, boxed, tag = "9")]
    pub video_message: Option<Box<VideoMessage>>,
    #[prost(message, optional, boxed, tag = "12")]
    pub protocol_message: Option<Box<ProtocolMessage>>,
    #[prost(message, optional, tag = "18")]
    pub live_location_message: Option<Location>,
    #[prost(message, optional, boxed, tag = "26")]
    pub sticker_message: Option<Box<StickerMessage>>,
    #[prost(message, optional, boxed, tag = "31")]
    pub device_sent_message: Option<Box<DeviceSent>>,
    #[prost(message, optional, boxed, tag = "37")]
    pub view_once_message: Option<Box<FutureProof>>,
    #[prost(message, optional, boxed, tag = "40")]
    pub ephemeral_message: Option<Box<FutureProof>>,
    #[prost(message, optional, tag = "46")]
    pub reaction_message: Option<Reaction>,
    #[prost(message, optional, boxed, tag = "53")]
    pub document_with_caption_message: Option<Box<FutureProof>>,
    #[prost(message, optional, boxed, tag = "55")]
    pub view_once_message_v2: Option<Box<FutureProof>>,
    #[prost(message, optional, boxed, tag = "58")]
    pub edited_message: Option<Box<FutureProof>>,
    #[prost(message, optional, boxed, tag = "59")]
    pub view_once_message_v2_extension: Option<Box<FutureProof>>,
    #[prost(message, optional, boxed, tag = "66")]
    pub ptv_message: Option<Box<VideoMessage>>,
    #[prost(message, optional, tag = "83")]
    pub album_message: Option<Album>,
    #[prost(message, optional, boxed, tag = "91")]
    pub associated_child_message: Option<Box<FutureProof>>,
    #[prost(message, optional, tag = "35")]
    pub message_context_info: Option<MessageContextInfo>,
    #[prost(message, optional, tag = "82")]
    pub secret_encrypted_message: Option<SecretEncrypted>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MessageContextInfo {
    /// The key a later edit of this message is encrypted under (see `secret.rs`).
    #[prost(bytes = "vec", optional, tag = "3")]
    pub message_secret: Option<Vec<u8>>,
}

/// A change to an earlier message, encrypted under that message's secret: how current phones
/// send edits.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SecretEncrypted {
    #[prost(message, optional, tag = "1")]
    pub target_message_key: Option<MessageKey>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub enc_payload: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub enc_iv: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub secret_enc_type: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SenderKeyDistributionMessage {
    #[prost(string, optional, tag = "1")]
    pub group_id: Option<String>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub axolotl_sender_key_distribution_message: Option<Vec<u8>>,
}

/// Each media message type, exactly: their field numbers disagree (a sticker's mimetype is
/// field 5, a string, where video keeps `seconds`, a number), and one wire-type mismatch fails
/// the whole decode.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ImageMessage {
    #[prost(string, optional, tag = "2")]
    pub mimetype: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub caption: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoMessage {
    #[prost(string, optional, tag = "2")]
    pub mimetype: Option<String>,
    #[prost(uint32, optional, tag = "5")]
    pub seconds: Option<u32>,
    #[prost(string, optional, tag = "7")]
    pub caption: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AudioMessage {
    #[prost(string, optional, tag = "1")]
    pub url: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub mimetype: Option<String>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub file_sha256: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "5")]
    pub seconds: Option<u32>,
    #[prost(bool, optional, tag = "6")]
    pub ptt: Option<bool>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub media_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub file_enc_sha256: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub direct_path: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DocumentMessage {
    #[prost(string, optional, tag = "2")]
    pub mimetype: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub title: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub file_name: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
    #[prost(string, optional, tag = "20")]
    pub caption: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StickerMessage {
    #[prost(string, optional, tag = "5")]
    pub mimetype: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
}

/// One media message seen through the columns the archive keeps.
#[derive(Clone, Copy)]
struct MediaView<'a> {
    mimetype: Option<&'a String>,
    caption: Option<&'a String>,
    file_name: Option<&'a String>,
    seconds: Option<u32>,
}

impl ImageMessage {
    fn view(&self) -> MediaView<'_> {
        MediaView {
            mimetype: self.mimetype.as_ref(),
            caption: self.caption.as_ref(),
            file_name: None,
            seconds: None,
        }
    }
}

impl VideoMessage {
    fn view(&self) -> MediaView<'_> {
        MediaView {
            mimetype: self.mimetype.as_ref(),
            caption: self.caption.as_ref(),
            file_name: None,
            seconds: self.seconds,
        }
    }
}

impl AudioMessage {
    fn view(&self) -> MediaView<'_> {
        MediaView {
            mimetype: self.mimetype.as_ref(),
            caption: None,
            file_name: None,
            seconds: self.seconds,
        }
    }
}

impl DocumentMessage {
    fn view(&self) -> MediaView<'_> {
        let file_name = self.file_name.as_ref().or(self.title.as_ref());
        MediaView {
            mimetype: self.mimetype.as_ref(),
            caption: self.caption.as_ref(),
            file_name,
            seconds: None,
        }
    }
}

impl StickerMessage {
    fn view(&self) -> MediaView<'_> {
        MediaView {
            mimetype: self.mimetype.as_ref(),
            caption: None,
            file_name: None,
            seconds: None,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExtendedText {
    #[prost(string, optional, tag = "1")]
    pub text: Option<String>,
    #[prost(message, optional, boxed, tag = "17")]
    pub context_info: Option<Box<ContextInfo>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ContextInfo {
    #[prost(string, optional, tag = "1")]
    pub stanza_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub participant: Option<String>,
    #[prost(message, optional, boxed, tag = "3")]
    pub quoted_message: Option<Box<Message>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ProtocolMessage {
    #[prost(message, optional, tag = "1")]
    pub key: Option<MessageKey>,
    #[prost(int32, optional, tag = "2")]
    pub r#type: Option<i32>,
    #[prost(message, optional, boxed, tag = "14")]
    pub edited_message: Option<Box<Message>>,
    #[prost(int64, optional, tag = "15")]
    pub timestamp_ms: Option<i64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MessageKey {
    #[prost(string, optional, tag = "1")]
    pub remote_jid: Option<String>,
    #[prost(bool, optional, tag = "2")]
    pub from_me: Option<bool>,
    #[prost(string, optional, tag = "3")]
    pub id: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub participant: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Reaction {
    #[prost(message, optional, tag = "1")]
    pub key: Option<MessageKey>,
    #[prost(string, optional, tag = "2")]
    pub text: Option<String>,
    #[prost(int64, optional, tag = "4")]
    pub sender_timestamp_ms: Option<i64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FutureProof {
    #[prost(message, optional, boxed, tag = "1")]
    pub message: Option<Box<Message>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeviceSent {
    #[prost(string, optional, tag = "1")]
    pub destination_jid: Option<String>,
    #[prost(message, optional, boxed, tag = "2")]
    pub message: Option<Box<Message>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Named {
    #[prost(string, optional, tag = "1")]
    pub display_name: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Location {
    #[prost(double, optional, tag = "1")]
    pub degrees_latitude: Option<f64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Album {
    #[prost(uint32, optional, tag = "2")]
    pub expected_image_count: Option<u32>,
}

/// Strips the random padding WhatsApp adds before encrypting (Baileys' `unpadRandomMax16`):
/// the last byte says how many bytes to drop.
pub fn unpad(padded: &[u8]) -> Option<&[u8]> {
    let (&pad, _) = padded.split_last()?;
    padded
        .len()
        .checked_sub(usize::from(pad))
        .map(|end| &padded[..end])
}

/// The top-level field numbers of an encoded message, in wire order: what to log about a
/// message this client keeps nothing of, without logging any of it.
pub fn field_numbers(mut bytes: &[u8]) -> Vec<u64> {
    fn varint(bytes: &mut &[u8]) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = bytes.split_first()?;
            *bytes = rest;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    let mut fields = Vec::new();
    while let Some(key) = varint(&mut bytes) {
        fields.push(key >> 3);
        let skip = match key & 7 {
            0 => varint(&mut bytes).map(|_| 0),
            1 => Some(8),
            2 => varint(&mut bytes).and_then(|n| usize::try_from(n).ok()),
            5 => Some(4),
            _ => None,
        };
        match skip.filter(|&n| n <= bytes.len()) {
            Some(n) => bytes = &bytes[n..],
            None => break,
        }
    }
    fields
}

/// Decodes an unpadded plaintext, with Baileys' one structural step: a message our own phone
/// sent arrives inside `deviceSentMessage`, which is unwrapped.
pub fn decode(plaintext: &[u8]) -> Option<Message> {
    let message = Message::decode(plaintext).ok()?;
    Some(
        match message
            .device_sent_message
            .as_ref()
            .and_then(|d| d.message.as_deref())
        {
            Some(inner) => {
                let mut inner = inner.clone();
                inner.sender_key_distribution_message = inner
                    .sender_key_distribution_message
                    .or(message.sender_key_distribution_message);
                // The secret lives on the outer envelope (Baileys `decode-wa-message.js`).
                inner.message_context_info =
                    inner.message_context_info.or(message.message_context_info);
                inner
            }
            None => message,
        },
    )
}

/// What the archive's message columns are made of.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Columns {
    pub body: Option<String>,
    pub kind: &'static str,
    pub reply_to_id: Option<String>,
    pub has_media: bool,
    pub media_mime: Option<String>,
    pub media_filename: Option<String>,
    pub media_seconds: Option<i32>,
    /// Where a voice note is and how to open it, for the transcriber.
    pub audio: Option<MediaRef>,
    /// What a later edit of this message is encrypted under.
    pub secret: Option<Vec<u8>>,
}

/// What a message does besides being one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A message to archive.
    Message(Columns),
    /// `target` now reads `body`.
    Edit {
        target: Target,
        body: Option<String>,
    },
    /// An edit of `target`, still encrypted under that message's secret.
    SecretEdit {
        target: Target,
        iv: Vec<u8>,
        payload: Vec<u8>,
    },
    /// `target` was deleted for everyone.
    Revoke { target: Target },
    /// A reaction to `target`; an empty emoji takes the reaction back.
    Reaction {
        target: Target,
        emoji: String,
        ts_ms: Option<i64>,
    },
    /// Protocol traffic (key shares, history sync notices, …), or nothing the archive keeps.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub remote_jid: Option<String>,
    pub from_me: bool,
    pub id: String,
    pub participant: Option<String>,
}

fn target(key: &MessageKey) -> Option<Target> {
    Some(Target {
        remote_jid: key.remote_jid.clone(),
        from_me: key.from_me.unwrap_or(false),
        id: key.id.clone()?,
        participant: key.participant.clone(),
    })
}

pub fn event(message: &Message) -> Event {
    let content = unwrap(message);
    if let Some(protocol) = &content.protocol_message {
        let Some(key) = protocol.key.as_ref().and_then(target) else {
            return Event::Other;
        };
        // A missing type is REVOKE: 0 is the default a sender may leave off the wire, and
        // Baileys' decoded `type` reads 0 for it.
        return match protocol.r#type.or(Some(REVOKE)) {
            Some(REVOKE) => Event::Revoke { target: key },
            Some(MESSAGE_EDIT) => Event::Edit {
                target: key,
                body: protocol.edited_message.as_deref().and_then(body),
            },
            _ => Event::Other,
        };
    }
    if let Some(secret) = &content.secret_encrypted_message {
        return match (
            secret.target_message_key.as_ref().and_then(target),
            &secret.enc_iv,
            &secret.enc_payload,
        ) {
            (Some(target), Some(iv), Some(payload))
                if secret.secret_enc_type == Some(SECRET_MESSAGE_EDIT) =>
            {
                Event::SecretEdit {
                    target,
                    iv: iv.clone(),
                    payload: payload.clone(),
                }
            }
            _ => Event::Other,
        };
    }
    if let Some(reaction) = &content.reaction_message {
        return match reaction.key.as_ref().and_then(target) {
            Some(key) => Event::Reaction {
                target: key,
                emoji: reaction.text.clone().unwrap_or_default(),
                ts_ms: reaction.sender_timestamp_ms,
            },
            None => Event::Other,
        };
    }
    if is_only_protocol(message) {
        return Event::Other;
    }
    Event::Message(columns(message))
}

/// A message with nothing but a sender key or context info is key distribution, not content.
pub fn is_only_protocol(message: &Message) -> bool {
    let empty = Message {
        sender_key_distribution_message: None,
        message_context_info: None,
        ..message.clone()
    };
    empty == Message::default()
}

fn columns(message: &Message) -> Columns {
    let (kind, media) = top_level(message);
    Columns {
        body: body(message).or_else(|| {
            media
                .and_then(|m| m.caption.cloned())
                .filter(|s| !s.is_empty())
        }),
        kind,
        reply_to_id: reply_to(message),
        has_media: media_content(&unwrap(message)).is_some()
            || matches!(kind, "image" | "video" | "audio" | "document" | "sticker"),
        media_mime: media.and_then(|m| m.mimetype.cloned()),
        media_filename: media.and_then(|m| m.file_name.cloned()),
        media_seconds: media.and_then(|m| m.seconds).map(|s| s as i32),
        // Only a top-level voice note: those are what the ingester marks for transcription.
        audio: message
            .audio_message
            .as_deref()
            .filter(|_| kind == "audio")
            .and_then(|a| {
                Some(MediaRef {
                    direct_path: a.direct_path.clone(),
                    url: a.url.clone(),
                    media_key: a.media_key.clone()?,
                    file_enc_sha256: a.file_enc_sha256.clone(),
                    file_sha256: a.file_sha256.clone(),
                })
            }),
        secret: message
            .message_context_info
            .as_ref()
            .and_then(|c| c.message_secret.clone()),
    }
}

/// The ingester's `kind`: the first top-level field it knows, in the payload's order, which is
/// the wire order, the lowest field number. Anything else is text.
fn top_level(m: &Message) -> (&'static str, Option<MediaView<'_>>) {
    let candidates: [(u32, &'static str, bool, Option<MediaView<'_>>); 9] = [
        (
            3,
            "image",
            m.image_message.is_some(),
            m.image_message.as_deref().map(ImageMessage::view),
        ),
        (4, "contact", m.contact_message.is_some(), None),
        (5, "location", m.location_message.is_some(), None),
        (
            7,
            "document",
            m.document_message.is_some(),
            m.document_message.as_deref().map(DocumentMessage::view),
        ),
        (
            8,
            "audio",
            m.audio_message.is_some(),
            m.audio_message.as_deref().map(AudioMessage::view),
        ),
        (
            9,
            "video",
            m.video_message.is_some(),
            m.video_message.as_deref().map(VideoMessage::view),
        ),
        (18, "location", m.live_location_message.is_some(), None),
        (
            26,
            "sticker",
            m.sticker_message.is_some(),
            m.sticker_message.as_deref().map(StickerMessage::view),
        ),
        (83, "album", m.album_message.is_some(), None),
    ];
    candidates
        .into_iter()
        .filter(|(_, _, here, _)| *here)
        .min_by_key(|(number, ..)| *number)
        .map_or(("text", None), |(_, kind, _, media)| (kind, media))
}

/// Baileys' `normalizeMessageContent`: up to five layers of wrapper removed.
fn unwrap(message: &Message) -> Message {
    let mut current = message.clone();
    for _ in 0..5 {
        let inner = [
            &current.ephemeral_message,
            &current.view_once_message,
            &current.document_with_caption_message,
            &current.view_once_message_v2,
            &current.view_once_message_v2_extension,
            &current.edited_message,
            &current.associated_child_message,
        ]
        .into_iter()
        .find_map(|w| w.as_deref().and_then(|f| f.message.as_deref()).cloned());
        match inner {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// WAHA's `extractMediaContent` after unwrapping: the first media field in its order.
fn media_content(m: &Message) -> Option<MediaView<'_>> {
    m.document_message
        .as_deref()
        .map(DocumentMessage::view)
        .or_else(|| m.image_message.as_deref().map(ImageMessage::view))
        .or_else(|| m.video_message.as_deref().map(VideoMessage::view))
        .or_else(|| m.audio_message.as_deref().map(AudioMessage::view))
        .or_else(|| m.ptv_message.as_deref().map(VideoMessage::view))
        .or_else(|| m.sticker_message.as_deref().map(StickerMessage::view))
}

/// WAHA's `extractBody`.
pub fn body(message: &Message) -> Option<String> {
    let content = unwrap(message);
    let found = content
        .conversation
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            content
                .extended_text_message
                .as_ref()
                .and_then(|e| e.text.clone())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            media_content(&content)
                .and_then(|m| m.caption.cloned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            content
                .protocol_message
                .as_ref()
                .and_then(|p| p.edited_message.as_deref())
                .and_then(body)
        })
        .or_else(|| {
            content
                .associated_child_message
                .as_ref()
                .and_then(|a| a.message.as_deref())
                .and_then(body)
        });
    found.filter(|s| !s.is_empty())
}

/// WAHA's `extractReplyTo`: the context info of Baileys' `getContentType(message)`, the first
/// top-level content field in wire order, wrapped or not. A wrapper has no context info.
fn reply_to(m: &Message) -> Option<String> {
    if m.conversation.is_some() {
        return None;
    }
    let first = m
        .image_message
        .as_deref()
        .map(|x| x.context_info.as_deref())
        .or_else(|| (m.contact_message.is_some() || m.location_message.is_some()).then_some(None))
        .or_else(|| {
            m.extended_text_message
                .as_deref()
                .map(|x| x.context_info.as_deref())
        })
        .or_else(|| {
            m.document_message
                .as_deref()
                .map(|x| x.context_info.as_deref())
        })
        .or_else(|| {
            m.audio_message
                .as_deref()
                .map(|x| x.context_info.as_deref())
        })
        .or_else(|| {
            m.video_message
                .as_deref()
                .map(|x| x.context_info.as_deref())
        })
        .or_else(|| {
            m.sticker_message
                .as_deref()
                .map(|x| x.context_info.as_deref())
        })
        .flatten()?;
    first.quoted_message.as_ref()?;
    first.stanza_id.clone()
}
