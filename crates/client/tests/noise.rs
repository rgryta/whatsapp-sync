//! The handshake against WAHA's own Baileys.
//!
//! `fixtures/noise.json` comes from `scripts/baileys-noise-fixture.mjs`: Baileys' client completed
//! a handshake there, certificate check included, and its frames were recorded with every key
//! used. Given the same keys and the same server reply, this client must send the same bytes:
//! AES-GCM is deterministic, so one wrong hash input anywhere changes every byte after it.

use prost::Message;
use serde::Deserialize;
use whatsapp_client::binary;
use whatsapp_client::login::{self, LoginSettings};
use whatsapp_client::noise::{Frames, Handshake, NoiseError};
use whatsapp_client::signal::curve::{KeyPair, PrivateKey};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    client_ephemeral: Pair,
    noise_key: Pair,
    root: String,
    now: u64,
    routing_info: String,
    client_payload: String,
    login: Login,
    hello_frame: String,
    server_hello: String,
    finish_frame: String,
    client_frames: Vec<ClientFrame>,
    server_frame: String,
    server_plaintext: String,
    /// A server hello per broken rule, keyed by the refusal it must produce.
    bad_server_hellos: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Login {
    user: String,
    device: u32,
    version: [u32; 3],
    country: String,
}

#[derive(Deserialize)]
struct Pair {
    private: String,
}

#[derive(Deserialize)]
struct ClientFrame {
    plaintext: String,
    frame: String,
}

fn fixture() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/noise.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn key_pair(pair: &Pair) -> KeyPair {
    let private = PrivateKey::deserialize(&unhex(&pair.private)).unwrap();
    KeyPair {
        public: private.public_key(),
        private,
    }
}

fn root(f: &Fixture) -> [u8; 32] {
    unhex(&f.root).try_into().unwrap()
}

#[test]
fn the_handshake_and_transport_match_baileys_byte_for_byte() {
    let f = fixture();
    let handshake = Handshake::with_ephemeral(key_pair(&f.client_ephemeral));
    let mut frames = Frames::new(Some(&unhex(&f.routing_info)));
    assert_eq!(
        frames.encode(&handshake.client_hello()).unwrap(),
        unhex(&f.hello_frame),
        "hello"
    );

    let (finish, mut transport) = handshake
        .client_finish(
            &unhex(&f.server_hello),
            &key_pair(&f.noise_key),
            &unhex(&f.client_payload),
            &root(&f),
            f.now,
        )
        .unwrap();
    assert_eq!(
        frames.encode(&finish).unwrap(),
        unhex(&f.finish_frame),
        "finish"
    );

    for (i, frame) in f.client_frames.iter().enumerate() {
        let sealed = transport.encrypt(&unhex(&frame.plaintext));
        assert_eq!(
            frames.encode(&sealed).unwrap(),
            unhex(&frame.frame),
            "client frame {i}"
        );
    }

    // Delivered a byte at a time, as a websocket is free to.
    for byte in unhex(&f.server_frame) {
        frames.push(&[byte]);
    }
    let sealed = frames.next_frame().expect("a whole frame");
    assert!(frames.next_frame().is_none());
    let plaintext = transport.decrypt(&sealed).unwrap();
    assert_eq!(plaintext, unhex(&f.server_plaintext));
    assert_eq!(binary::unmarshal(&plaintext).unwrap().tag, "success");
}

fn finish_with(
    f: &Fixture,
    server_hello: &[u8],
    root: &[u8; 32],
    now: u64,
) -> Result<(), NoiseError> {
    let handshake = Handshake::with_ephemeral(key_pair(&f.client_ephemeral));
    handshake
        .client_finish(server_hello, &key_pair(&f.noise_key), b"payload", root, now)
        .map(|_| ())
}

#[test]
fn a_certificate_not_chaining_to_the_root_is_refused() {
    let f = fixture();
    let mut wrong_root = root(&f);
    wrong_root[0] ^= 1;
    assert_eq!(
        finish_with(&f, &unhex(&f.server_hello), &wrong_root, f.now),
        Err(NoiseError::Certificate(
            "intermediate is not signed by the root"
        ))
    );
    // WhatsApp's real root did not sign this test chain either.
    assert!(
        finish_with(
            &f,
            &unhex(&f.server_hello),
            &whatsapp_client::noise::WA_CERT_ROOT,
            f.now
        )
        .is_err()
    );
}

#[test]
fn every_broken_certificate_rule_is_refused_for_that_reason() {
    let f = fixture();
    assert_eq!(f.bad_server_hellos.len(), 4);
    for (reason, hello) in &f.bad_server_hellos {
        let refusal = finish_with(&f, &unhex(hello), &root(&f), f.now).unwrap_err();
        assert_eq!(refusal.to_string(), format!("server certificate: {reason}"));
    }
}

#[test]
fn an_expired_or_premature_certificate_is_refused() {
    let f = fixture();
    let hello = unhex(&f.server_hello);
    assert_eq!(
        finish_with(&f, &hello, &root(&f), f.now + 7 * 86400),
        Err(NoiseError::Certificate("expired"))
    );
    assert_eq!(
        finish_with(&f, &hello, &root(&f), f.now - 7 * 86400),
        Err(NoiseError::Certificate("not valid yet"))
    );
    assert!(finish_with(&f, &hello, &root(&f), f.now).is_ok());
}

#[test]
fn a_tampered_server_hello_does_not_authenticate() {
    let f = fixture();
    let mut message = whatsapp_client_proto_roundtrip(&unhex(&f.server_hello));
    let last = message.len() - 1;
    message[last] ^= 1; // inside the encrypted certificate
    assert_eq!(
        finish_with(&f, &message, &root(&f), f.now),
        Err(NoiseError::Decrypt)
    );
}

/// The server hello re-encoded unchanged, to show the tamper test flips a payload byte and not
/// the protobuf framing.
fn whatsapp_client_proto_roundtrip(bytes: &[u8]) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Opaque {
        #[prost(bytes = "vec", optional, tag = "3")]
        server_hello: Option<Vec<u8>>,
    }
    let message = Opaque::decode(bytes).unwrap();
    assert_eq!(message.encode_to_vec(), bytes);
    message.encode_to_vec()
}

#[test]
fn frames_split_and_joined_arbitrarily_come_out_whole() {
    let mut writer = Frames::new(None);
    let a = writer.encode(b"first").unwrap();
    assert_eq!(
        &a[..4],
        b"WA\x06\x03",
        "the first frame carries the prologue"
    );
    let b = writer.encode(&[7u8; 300]).unwrap();
    assert_eq!(&b[..3], &[0, 1, 44], "later frames do not");
    let mut reader = Frames::new(None);
    let stream = [&a[4..], &b[..]].concat();
    reader.push(&stream[..2]);
    assert!(reader.next_frame().is_none());
    reader.push(&stream[2..10]);
    reader.push(&stream[10..]);
    assert_eq!(reader.next_frame().unwrap(), b"first");
    assert_eq!(reader.next_frame().unwrap(), vec![7u8; 300]);
    assert!(reader.next_frame().is_none());
}

#[test]
fn the_login_payload_is_the_one_waha_sends() {
    let f = fixture();
    let settings = LoginSettings {
        version: f.login.version,
        country: f.login.country.clone(),
    };
    let payload = login::client_payload(f.login.user.parse().unwrap(), f.login.device, &settings);
    assert_eq!(payload, unhex(&f.client_payload));
    let defaults = LoginSettings::default();
    assert_eq!(
        (defaults.version, defaults.country),
        (f.login.version, f.login.country)
    );
}
