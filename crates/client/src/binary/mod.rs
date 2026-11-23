// SPDX-License-Identifier: MPL-2.0
//
// Ported from whatsmeow (https://github.com/tulir/whatsmeow, c88388f), binary/encoder.go,
// decoder.go and unpack.go, Copyright (c) Tulir Asokan, MPL-2.0. Modified for whatsapp-sync:
// attributes keep their order, a JID remembers which of the four wire forms it came in, and
// errors are values rather than panics.

//! WhatsApp's binary XML: every frame after the handshake is one [`Node`] in this encoding.
//!
//! A node is a list: its tag, its attributes as key/value pairs, then optionally content (child
//! nodes, bytes, or a string). Strings are shrunk on the wire three ways: a token from
//! [`tokens`], a run of digits or upper-case hex packed two to a byte, or a JID in one of four
//! compact forms. Anything else is length-prefixed raw bytes.

pub mod tokens;

use std::fmt;
use std::io::Read;

const LIST_EMPTY: u8 = 0;
const DICTIONARY_0: u8 = 236;
const DICTIONARY_3: u8 = 239;
const INTEROP_JID: u8 = 245;
const FB_JID: u8 = 246;
const AD_JID: u8 = 247;
const LIST_8: u8 = 248;
const LIST_16: u8 = 249;
const JID_PAIR: u8 = 250;
const HEX_8: u8 = 251;
const BINARY_8: u8 = 252;
const BINARY_20: u8 = 253;
const BINARY_32: u8 = 254;
const NIBBLE_8: u8 = 255;
const PACKED_MAX: usize = 127;

/// The flag on a frame's first byte that says the rest is zlib-compressed.
const COMPRESSED: u8 = 2;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BinaryError {
    #[error("frame ended early")]
    Eof,
    #[error("invalid token {0} at byte {1}")]
    InvalidToken(u8, usize),
    #[error("invalid node at byte {0}")]
    InvalidNode(usize),
    #[error("attribute key is not a string at byte {0}")]
    NonStringKey(usize),
    #[error("invalid JID: {0}")]
    InvalidJid(&'static str),
    #[error("invalid packed byte {0}")]
    InvalidPacked(u8),
    #[error("{0} bytes after the node")]
    Trailing(usize),
    #[error("compressed frame does not inflate")]
    Inflate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub tag: String,
    /// In wire order; a `Vec` rather than a map so a decoded node encodes back to the same bytes.
    pub attrs: Vec<(String, Value)>,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    None,
    Nodes(Vec<Node>),
    Bytes(Vec<u8>),
    /// Content written as a string: a token, packed digits or a raw string.
    Text(String),
    Jid(Jid),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Jid(Jid),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jid {
    pub user: String,
    pub server: String,
    pub form: JidForm,
}

/// How a JID travels. The device-qualified forms are chosen by whoever encodes, not implied by
/// the address, so a decoded JID keeps the form it arrived in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JidForm {
    /// `user@server`, both as strings.
    Pair,
    /// A device of a WhatsApp, LID or hosted account: domain byte, device byte, user.
    Ad { domain: u8, device: u8 },
    /// A Messenger account.
    Fb { device: u16 },
    /// An interop (third-party network) account.
    Interop { device: u16, integrator: u16 },
}

impl Node {
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            attrs: Vec::new(),
            content: Content::None,
        }
    }

    pub fn attr(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.attrs.push((key.into(), value.into()));
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn children(&self) -> &[Node] {
        match &self.content {
            Content::Nodes(nodes) => nodes,
            _ => &[],
        }
    }

    pub fn child(&self, tag: &str) -> Option<&Node> {
        self.children().iter().find(|n| n.tag == tag)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<Jid> for Value {
    fn from(j: Jid) -> Self {
        Value::Jid(j)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => f.write_str(s),
            Value::Jid(j) => j.fmt(f),
        }
    }
}

/// The server each AD-JID domain byte stands for.
const DOMAINS: [(u8, &str); 4] = [
    (0, "s.whatsapp.net"),
    (1, "lid"),
    (128, "hosted"),
    (129, "hosted.lid"),
];

impl Jid {
    pub fn pair(user: impl Into<String>, server: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            server: server.into(),
            form: JidForm::Pair,
        }
    }

    /// A device of a WhatsApp (`s.whatsapp.net`), LID or hosted account.
    pub fn device(user: impl Into<String>, server: &str, device: u8) -> Option<Self> {
        let domain = DOMAINS.iter().find(|(_, s)| *s == server)?.0;
        Some(Self {
            user: user.into(),
            server: server.to_owned(),
            form: JidForm::Ad { domain, device },
        })
    }

    pub fn device_number(&self) -> u16 {
        match self.form {
            JidForm::Pair => 0,
            JidForm::Ad { device, .. } => device.into(),
            JidForm::Fb { device } | JidForm::Interop { device, .. } => device,
        }
    }
}

/// As Baileys writes a JID, `user:device@server`, the device left out when it is 0.
impl fmt::Display for Jid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.device_number() {
            0 => write!(f, "{}@{}", self.user, self.server),
            device => write!(f, "{}:{device}@{}", self.user, self.server),
        }
    }
}

// --- decoding -------------------------------------------------------------------------------

/// One frame's payload: a flags byte, then the node, zlib-compressed if the flag says so.
pub fn unmarshal(frame: &[u8]) -> Result<Node, BinaryError> {
    let (&flags, body) = frame.split_first().ok_or(BinaryError::Eof)?;
    if flags & COMPRESSED != 0 {
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(body)
            .read_to_end(&mut inflated)
            .map_err(|_| BinaryError::Inflate)?;
        return decode(&inflated);
    }
    decode(body)
}

/// One node, with nothing after it.
pub fn decode(data: &[u8]) -> Result<Node, BinaryError> {
    let mut decoder = Decoder { data, index: 0 };
    let node = decoder.read_node()?;
    match data.len() - decoder.index {
        0 => Ok(node),
        rest => Err(BinaryError::Trailing(rest)),
    }
}

/// A string-shaped read: what a position that is not content can hold.
enum Item {
    Empty,
    Str(String),
    Jid(Jid),
    Bytes(Vec<u8>),
    Nodes(Vec<Node>),
}

struct Decoder<'a> {
    data: &'a [u8],
    index: usize,
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], BinaryError> {
        let end = self
            .index
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(BinaryError::Eof)?;
        let bytes = &self.data[self.index..end];
        self.index = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, BinaryError> {
        Ok(self.take(1)?[0])
    }

    fn int_be(&mut self, n: usize) -> Result<usize, BinaryError> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0, |acc, &b| (acc << 8) | usize::from(b)))
    }

    fn int20(&mut self) -> Result<usize, BinaryError> {
        let b = self.take(3)?;
        Ok((usize::from(b[0] & 0x0F) << 16) | (usize::from(b[1]) << 8) | usize::from(b[2]))
    }

    fn list_size(&mut self, tag: u8) -> Result<usize, BinaryError> {
        match tag {
            LIST_EMPTY => Ok(0),
            LIST_8 => self.int_be(1),
            LIST_16 => self.int_be(2),
            other => Err(BinaryError::InvalidToken(other, self.index)),
        }
    }

    fn read(&mut self, as_string: bool) -> Result<Item, BinaryError> {
        let tag = self.byte()?;
        Ok(match tag {
            LIST_EMPTY => Item::Empty,
            LIST_8 | LIST_16 => {
                let size = self.list_size(tag)?;
                Item::Nodes(
                    (0..size)
                        .map(|_| self.read_node())
                        .collect::<Result<_, _>>()?,
                )
            }
            BINARY_8 | BINARY_20 | BINARY_32 => {
                let size = match tag {
                    BINARY_8 => self.int_be(1)?,
                    BINARY_20 => self.int20()?,
                    _ => self.int_be(4)?,
                };
                let bytes = self.take(size)?.to_vec();
                if as_string {
                    Item::Str(String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    Item::Bytes(bytes)
                }
            }
            DICTIONARY_0..=DICTIONARY_3 => {
                let index = self.byte()?;
                Item::Str(
                    tokens::DOUBLE_BYTE[usize::from(tag - DICTIONARY_0)][usize::from(index)]
                        .to_owned(),
                )
            }
            JID_PAIR => {
                let user = self.read_string()?;
                let server = self
                    .read_string()?
                    .ok_or(BinaryError::InvalidJid("pair without a server"))?;
                Item::Jid(Jid::pair(user.unwrap_or_default(), server))
            }
            AD_JID => {
                let domain = self.byte()?;
                let device = self.byte()?;
                let user = self
                    .read_string()?
                    .ok_or(BinaryError::InvalidJid("AD JID without a user"))?;
                let server = DOMAINS
                    .iter()
                    .find(|(d, _)| *d == domain)
                    .map_or("s.whatsapp.net", |(_, s)| s);
                Item::Jid(Jid {
                    user,
                    server: server.to_owned(),
                    form: JidForm::Ad { domain, device },
                })
            }
            FB_JID => {
                let user = self
                    .read_string()?
                    .ok_or(BinaryError::InvalidJid("FB JID without a user"))?;
                let device = self.int_be(2)? as u16;
                let server = self
                    .read_string()?
                    .filter(|s| s == "msgr")
                    .ok_or(BinaryError::InvalidJid("FB JID not on msgr"))?;
                Item::Jid(Jid {
                    user,
                    server,
                    form: JidForm::Fb { device },
                })
            }
            INTEROP_JID => {
                let user = self
                    .read_string()?
                    .ok_or(BinaryError::InvalidJid("interop JID without a user"))?;
                let device = self.int_be(2)? as u16;
                let integrator = self.int_be(2)? as u16;
                let server = self
                    .read_string()?
                    .filter(|s| s == "interop")
                    .ok_or(BinaryError::InvalidJid("interop JID not on interop"))?;
                Item::Jid(Jid {
                    user,
                    server,
                    form: JidForm::Interop { device, integrator },
                })
            }
            NIBBLE_8 | HEX_8 => Item::Str(self.read_packed(tag)?),
            index => match tokens::SINGLE_BYTE.get(usize::from(index)) {
                Some(token) if index > 0 => Item::Str((*token).to_owned()),
                _ => return Err(BinaryError::InvalidToken(tag, self.index)),
            },
        })
    }

    /// A position that must hold a string, or nothing.
    fn read_string(&mut self) -> Result<Option<String>, BinaryError> {
        let at = self.index;
        match self.read(true)? {
            Item::Empty => Ok(None),
            Item::Str(s) => Ok(Some(s)),
            _ => Err(BinaryError::InvalidNode(at)),
        }
    }

    fn read_packed(&mut self, tag: u8) -> Result<String, BinaryError> {
        let start = self.byte()?;
        let mut out = String::with_capacity(usize::from(start & 0x7F) * 2);
        for _ in 0..(start & 0x7F) {
            let b = self.byte()?;
            out.push(unpack(tag, b >> 4)?);
            out.push(unpack(tag, b & 0x0F)?);
        }
        if start >> 7 != 0 {
            out.pop();
        }
        Ok(out)
    }

    fn read_node(&mut self) -> Result<Node, BinaryError> {
        let at = self.index;
        let list_tag = self.byte()?;
        let size = self.list_size(list_tag)?;
        let tag = match self.read(true)? {
            Item::Str(tag) if size > 0 && !tag.is_empty() => tag,
            _ => return Err(BinaryError::InvalidNode(at)),
        };
        let mut attrs = Vec::with_capacity((size - 1) / 2);
        for _ in 0..(size - 1) / 2 {
            let key_at = self.index;
            let key = match self.read(true)? {
                Item::Str(key) => key,
                _ => return Err(BinaryError::NonStringKey(key_at)),
            };
            let value = match self.read(true)? {
                Item::Str(s) => Value::Str(s),
                Item::Jid(j) => Value::Jid(j),
                _ => return Err(BinaryError::InvalidNode(key_at)),
            };
            attrs.push((key, value));
        }
        let content = if size % 2 == 1 {
            Content::None
        } else {
            match self.read(false)? {
                Item::Empty => Content::None,
                Item::Str(s) => Content::Text(s),
                Item::Jid(j) => Content::Jid(j),
                Item::Bytes(b) => Content::Bytes(b),
                Item::Nodes(n) => Content::Nodes(n),
            }
        };
        Ok(Node {
            tag,
            attrs,
            content,
        })
    }
}

fn unpack(tag: u8, value: u8) -> Result<char, BinaryError> {
    match (tag, value) {
        (_, 0..=9) => Ok(char::from(b'0' + value)),
        (NIBBLE_8, 10) => Ok('-'),
        (NIBBLE_8, 11) => Ok('.'),
        (NIBBLE_8, 15) => Ok('\0'),
        (HEX_8, 10..=15) => Ok(char::from(b'A' + value - 10)),
        _ => Err(BinaryError::InvalidPacked(value)),
    }
}

// --- encoding -------------------------------------------------------------------------------

/// A frame payload: the uncompressed flags byte, then the node.
pub fn marshal(node: &Node) -> Vec<u8> {
    let mut out = vec![0];
    encode_node(&mut out, node);
    out
}

fn encode_node(out: &mut Vec<u8>, node: &Node) {
    let has_content = !matches!(node.content, Content::None);
    write_list_start(out, 2 * node.attrs.len() + 1 + usize::from(has_content));
    write_string(out, &node.tag);
    for (key, value) in &node.attrs {
        write_string(out, key);
        match value {
            Value::Str(s) => write_string(out, s),
            Value::Jid(j) => write_jid(out, j),
        }
    }
    match &node.content {
        Content::None => {}
        Content::Nodes(nodes) => {
            write_list_start(out, nodes.len());
            for n in nodes {
                encode_node(out, n);
            }
        }
        Content::Bytes(bytes) => write_bytes(out, bytes),
        Content::Text(s) => write_string(out, s),
        Content::Jid(j) => write_jid(out, j),
    }
}

fn write_list_start(out: &mut Vec<u8>, size: usize) {
    match size {
        0 => out.push(LIST_EMPTY),
        1..=255 => out.extend([LIST_8, size as u8]),
        _ => {
            out.push(LIST_16);
            out.extend((size as u16).to_be_bytes());
        }
    }
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len < 256 {
        out.extend([BINARY_8, len as u8]);
    } else if len < 1 << 20 {
        out.extend([
            BINARY_20,
            (len >> 16) as u8 & 0x0F,
            (len >> 8) as u8,
            len as u8,
        ]);
    } else {
        out.push(BINARY_32);
        out.extend((len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    if let Some(index) = tokens::SINGLE_BYTE
        .iter()
        .position(|t| *t == s)
        .filter(|&i| i > 0)
    {
        out.push(index as u8);
    } else if let Some((dict, index)) = tokens::DOUBLE_BYTE
        .iter()
        .enumerate()
        .find_map(|(d, row)| row.iter().position(|t| *t == s).map(|i| (d, i)))
    {
        out.extend([DICTIONARY_0 + dict as u8, index as u8]);
    } else if is_packable(s, |c| c.is_ascii_digit() || c == '-' || c == '.') {
        write_packed(out, s, NIBBLE_8);
    } else if is_packable(s, |c| c.is_ascii_digit() || ('A'..='F').contains(&c)) {
        write_packed(out, s, HEX_8);
    } else {
        write_bytes(out, s.as_bytes());
    }
}

fn is_packable(s: &str, allowed: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.len() <= PACKED_MAX && s.chars().all(allowed)
}

fn write_packed(out: &mut Vec<u8>, s: &str, tag: u8) {
    let bytes = s.as_bytes();
    let pack = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        b'-' => 10,
        b'.' => 11,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 15, // the padding of an odd length
    };
    out.push(tag);
    out.push(bytes.len().div_ceil(2) as u8 | if bytes.len() % 2 == 1 { 0x80 } else { 0 });
    for pair in bytes.chunks(2) {
        out.push(pack(pair[0]) << 4 | pack(*pair.get(1).unwrap_or(&0)));
    }
}

fn write_jid(out: &mut Vec<u8>, jid: &Jid) {
    match jid.form {
        JidForm::Pair => {
            out.push(JID_PAIR);
            if jid.user.is_empty() {
                out.push(LIST_EMPTY);
            } else {
                write_string(out, &jid.user);
            }
            write_string(out, &jid.server);
        }
        JidForm::Ad { domain, device } => {
            out.extend([AD_JID, domain, device]);
            write_string(out, &jid.user);
        }
        JidForm::Fb { device } => {
            out.push(FB_JID);
            write_string(out, &jid.user);
            out.extend(device.to_be_bytes());
            write_string(out, &jid.server);
        }
        JidForm::Interop { device, integrator } => {
            out.push(INTEROP_JID);
            write_string(out, &jid.user);
            out.extend(device.to_be_bytes());
            out.extend(integrator.to_be_bytes());
            write_string(out, &jid.server);
        }
    }
}
