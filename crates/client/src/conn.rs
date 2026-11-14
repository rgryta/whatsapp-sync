//! One connection to WhatsApp: websocket, Noise handshake, login, then nodes both ways.
//!
//! Mirrors what WAHA's Baileys does for the same device (`Socket/socket.js`): the routing info
//! on the URL and in the intro header, the `Origin` header, the login payload, and message
//! tags shaped like Baileys' so the server sees the client it already knows.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::baileys::Device;
use crate::binary::{self, BinaryError, Node};
use crate::login::{self, LoginSettings};
use crate::noise::{self, Frames, Handshake, NoiseError, Transport};
use crate::signal::curve::KeyPair;

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("websocket: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Noise(#[from] NoiseError),
    #[error(transparent)]
    Binary(#[from] BinaryError),
    #[error("the device has no account id to log in as")]
    NotPaired,
    #[error("the account id {0:?} is not user:device@s.whatsapp.net")]
    BadAccountId(String),
    #[error("connection closed during the handshake")]
    ClosedEarly,
    #[error("a header value is not valid: {0}")]
    Header(#[from] tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue),
}

pub struct Connection {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    frames: Frames,
    transport: Transport,
    tags: Tags,
}

/// Where a server speaking this protocol listens. WhatsApp's is [`noise::URL`]; Messenger's
/// encrypted chat speaks the same protocol from its own host.
pub struct Endpoint<'a> {
    pub url: &'a str,
    pub origin: &'a str,
    pub user_agent: Option<&'a str>,
}

/// Connects and logs in as `device`. Returns the connection and the server's first node, which
/// is `<success>` when the login was accepted and `<failure reason=…>` when not.
pub async fn connect(
    device: &Device,
    settings: &LoginSettings,
) -> Result<(Connection, Node), ConnError> {
    let me = device.me.as_ref().ok_or(ConnError::NotPaired)?;
    let (user, device_number) =
        parse_account(&me.id).ok_or_else(|| ConnError::BadAccountId(me.id.clone()))?;

    // Baileys passes the routing info twice: as `ED` on the URL and in the intro header.
    let url = match &device.routing_info {
        Some(info) => format!("{}?ED={}", noise::URL, URL_SAFE_NO_PAD.encode(info)),
        None => noise::URL.to_owned(),
    };
    let endpoint = Endpoint {
        url: &url,
        origin: noise::ORIGIN,
        user_agent: None,
    };
    let payload = login::client_payload(user, device_number, settings);
    connect_to(
        &endpoint,
        device.routing_info.as_deref(),
        &device.noise,
        &payload,
    )
    .await
}

/// Connects to `endpoint` and logs in with `payload`, an encoded `ClientPayload`, as the device
/// whose Noise key is `noise_key`.
pub async fn connect_to(
    endpoint: &Endpoint<'_>,
    routing_info: Option<&[u8]>,
    noise_key: &KeyPair,
    payload: &[u8],
) -> Result<(Connection, Node), ConnError> {
    let mut request = endpoint.url.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Origin", HeaderValue::from_str(endpoint.origin)?);
    if let Some(user_agent) = endpoint.user_agent {
        headers.insert("User-Agent", HeaderValue::from_str(user_agent)?);
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;

    let mut frames = Frames::new(routing_info);
    let handshake = Handshake::new();
    ws.send(Message::Binary(Bytes::from(
        frames.encode(&handshake.client_hello())?,
    )))
    .await?;
    let server_hello = next_frame(&mut ws, &mut frames).await?;
    let (finish, mut transport) = handshake.client_finish(
        &server_hello,
        noise_key,
        payload,
        &noise::WA_CERT_ROOT,
        unix_now(),
    )?;
    ws.send(Message::Binary(Bytes::from(frames.encode(&finish)?)))
        .await?;
    let first = binary::unmarshal(&transport.decrypt(&next_frame(&mut ws, &mut frames).await?)?)?;
    Ok((
        Connection {
            ws,
            frames,
            transport,
            tags: Tags::new(),
        },
        first,
    ))
}

impl Connection {
    pub async fn send(&mut self, node: &Node) -> Result<(), ConnError> {
        let sealed = self.transport.encrypt(&binary::marshal(node));
        self.ws
            .send(Message::Binary(Bytes::from(self.frames.encode(&sealed)?)))
            .await?;
        Ok(())
    }

    /// The next node from the server, or `None` when it closed the connection.
    pub async fn recv(&mut self) -> Result<Option<Node>, ConnError> {
        match next_frame(&mut self.ws, &mut self.frames).await {
            Ok(frame) => Ok(Some(binary::unmarshal(&self.transport.decrypt(&frame)?)?)),
            Err(ConnError::ClosedEarly) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A fresh id for a node this client sends, shaped like Baileys' message tags.
    pub fn next_tag(&mut self) -> String {
        self.tags.next()
    }

    pub async fn close(mut self) -> Result<(), ConnError> {
        self.ws.close(None).await?;
        Ok(())
    }
}

/// Where replies go: the connection, or a recorder in tests, so the daemon's handling can be
/// exercised without WhatsApp on the other end.
#[allow(async_fn_in_trait)] // only ever used with concrete types, inside this crate's tests
pub trait Outbox {
    async fn send(&mut self, node: &Node) -> Result<(), ConnError>;
    fn next_tag(&mut self) -> String;
}

impl Outbox for Connection {
    async fn send(&mut self, node: &Node) -> Result<(), ConnError> {
        Connection::send(self, node).await
    }

    fn next_tag(&mut self) -> String {
        Connection::next_tag(self)
    }
}

async fn next_frame(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    frames: &mut Frames,
) -> Result<Vec<u8>, ConnError> {
    loop {
        if let Some(frame) = frames.next_frame() {
            return Ok(frame);
        }
        match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => frames.push(&bytes),
            Some(Ok(Message::Close(_))) | None => return Err(ConnError::ClosedEarly),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

/// `user:device@s.whatsapp.net`, as Baileys stores the account id.
fn parse_account(id: &str) -> Option<(u64, u32)> {
    let address = id.strip_suffix("@s.whatsapp.net")?;
    let (user, device) = address.split_once(':').unwrap_or((address, "0"));
    Some((user.parse().ok()?, device.parse().ok()?))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Baileys' `generateMessageTag`: a random `NNNNN.NNNNN-` prefix per connection, then a counter
/// from 1.
struct Tags {
    prefix: String,
    counter: u64,
}

impl Tags {
    fn new() -> Self {
        let mut bytes = [0u8; 4];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
        let prefix = format!(
            "{}.{}-",
            u16::from_be_bytes([bytes[0], bytes[1]]),
            u16::from_be_bytes([bytes[2], bytes[3]])
        );
        Self { prefix, counter: 1 }
    }

    fn next(&mut self) -> String {
        let tag = format!("{}{}", self.prefix, self.counter);
        self.counter += 1;
        tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_id_parses_as_baileys_writes_it() {
        assert_eq!(
            parse_account("48000000000:3@s.whatsapp.net"),
            Some((48_000_000_000, 3))
        );
        assert_eq!(
            parse_account("48000000000@s.whatsapp.net"),
            Some((48_000_000_000, 0))
        );
        assert_eq!(parse_account("111:3@lid"), None);
    }

    #[test]
    fn tags_count_up_behind_one_prefix() {
        let mut tags = Tags::new();
        let (a, b) = (tags.next(), tags.next());
        assert!(a.ends_with("-1") && b.ends_with("-2"));
        assert_eq!(a.trim_end_matches('1'), b.trim_end_matches('2'));
    }
}
