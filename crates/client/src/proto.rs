//! The protobuf shapes this crate writes or reads by hand.
//!
//! `SessionStructure` and `RecordStructure` are libsignal v0.74.1's `storage.proto`, the form
//! sessions are stored in, so a record from libsignal reads here unchanged. Only the fields a
//! version-3 session uses are present.
//!
//! `WireSignalMessage` and `WirePreKeySignalMessage` are libsignal's `wire.proto`, which
//! WhatsApp's `msg` and `pkmsg` carry unchanged; the Kyber and post-quantum fields are read
//! only so that a message using them can be refused.
//!
//! `HandshakeMessage` and `CertChain` are WhatsApp's Noise handshake (`WAWebProtobufsWa6`,
//! `WACert`), only the fields this client reads or sends.
//!
//! The two `Wa*` messages are WhatsApp's group wire format, which is the original libsignal
//! one. Signal has since renumbered it around a distribution UUID, so libsignal's own group
//! cipher cannot parse WhatsApp's messages.

#[derive(Clone, PartialEq, prost::Message)]
pub struct RecordStructure {
    #[prost(message, optional, tag = "1")]
    pub current_session: Option<SessionStructure>,
    /// Each entry is an encoded `SessionStructure`, most recently archived first.
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub previous_sessions: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SessionStructure {
    #[prost(uint32, tag = "1")]
    pub session_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub local_identity_public: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub remote_identity_public: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub root_key: Vec<u8>,
    #[prost(uint32, tag = "5")]
    pub previous_counter: u32,
    #[prost(message, optional, tag = "6")]
    pub sender_chain: Option<Chain>,
    #[prost(message, repeated, tag = "7")]
    pub receiver_chains: Vec<Chain>,
    #[prost(message, optional, tag = "9")]
    pub pending_pre_key: Option<PendingPreKey>,
    #[prost(uint32, tag = "10")]
    pub remote_registration_id: u32,
    #[prost(uint32, tag = "11")]
    pub local_registration_id: u32,
    #[prost(bytes = "vec", tag = "13")]
    pub alice_base_key: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Chain {
    #[prost(bytes = "vec", tag = "1")]
    pub sender_ratchet_key: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub sender_ratchet_key_private: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub chain_key: Option<ChainKey>,
    #[prost(message, repeated, tag = "4")]
    pub message_keys: Vec<MessageKey>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ChainKey {
    /// The index of the message this key derives next.
    #[prost(uint32, tag = "1")]
    pub index: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub key: Vec<u8>,
}

/// A skipped message's key, kept as the seed libsignal derives the AES and MAC keys from.
#[derive(Clone, PartialEq, prost::Message)]
pub struct MessageKey {
    #[prost(uint32, tag = "1")]
    pub index: u32,
    #[prost(bytes = "vec", tag = "5")]
    pub seed: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PendingPreKey {
    #[prost(uint32, optional, tag = "1")]
    pub pre_key_id: Option<u32>,
    #[prost(bytes = "vec", tag = "2")]
    pub base_key: Vec<u8>,
    #[prost(int32, tag = "3")]
    pub signed_pre_key_id: i32,
    #[prost(uint64, tag = "4")]
    pub timestamp: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WaSenderKeyMessage {
    #[prost(uint32, optional, tag = "1")]
    pub id: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub iteration: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub ciphertext: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WaSenderKeyDistributionMessage {
    #[prost(uint32, optional, tag = "1")]
    pub id: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub iteration: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub chain_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub signing_key: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireSignalMessage {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub ratchet_key: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "2")]
    pub counter: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub previous_counter: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub ciphertext: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub pq_ratchet: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WirePreKeySignalMessage {
    #[prost(uint32, optional, tag = "5")]
    pub registration_id: Option<u32>,
    #[prost(uint32, optional, tag = "1")]
    pub pre_key_id: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub signed_pre_key_id: Option<u32>,
    #[prost(uint32, optional, tag = "7")]
    pub kyber_pre_key_id: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub kyber_ciphertext: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub base_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub identity_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub message: Option<Vec<u8>>,
}

/// `WAWebProtobufsWa6.HandshakeMessage`: the three Noise handshake messages.
#[derive(Clone, PartialEq, prost::Message)]
pub struct HandshakeMessage {
    #[prost(message, optional, tag = "2")]
    pub client_hello: Option<HandshakeHello>,
    #[prost(message, optional, tag = "3")]
    pub server_hello: Option<HandshakeHello>,
    #[prost(message, optional, tag = "4")]
    pub client_finish: Option<ClientFinish>,
}

/// Client and server hello share these first three fields.
#[derive(Clone, PartialEq, prost::Message)]
pub struct HandshakeHello {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub ephemeral: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub r#static: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub payload: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientFinish {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub r#static: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub payload: Option<Vec<u8>>,
}

/// `WACert.CertChain`: the server's Noise certificate, leaf and intermediate.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CertChain {
    #[prost(message, optional, tag = "1")]
    pub leaf: Option<NoiseCertificate>,
    #[prost(message, optional, tag = "2")]
    pub intermediate: Option<NoiseCertificate>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NoiseCertificate {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub details: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub signature: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CertDetails {
    #[prost(uint32, optional, tag = "1")]
    pub serial: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub issuer_serial: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub key: Option<Vec<u8>>,
    #[prost(uint64, optional, tag = "4")]
    pub not_before: Option<u64>,
    #[prost(uint64, optional, tag = "5")]
    pub not_after: Option<u64>,
}

/// `WAWebProtobufsWa6.ClientPayload`: the login inside the handshake's last message. Only the
/// fields WAHA's Baileys sets when it resumes a paired device.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientPayload {
    #[prost(uint64, optional, tag = "1")]
    pub username: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub passive: Option<bool>,
    #[prost(message, optional, tag = "5")]
    pub user_agent: Option<UserAgent>,
    #[prost(message, optional, tag = "6")]
    pub web_info: Option<WebInfo>,
    #[prost(int32, optional, tag = "12")]
    pub connect_type: Option<i32>,
    #[prost(int32, optional, tag = "13")]
    pub connect_reason: Option<i32>,
    #[prost(uint32, optional, tag = "18")]
    pub device: Option<u32>,
    #[prost(int32, optional, tag = "24")]
    pub lc: Option<i32>,
    #[prost(bool, optional, tag = "33")]
    pub pull: Option<bool>,
    #[prost(bool, optional, tag = "41")]
    pub lid_db_migrated: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserAgent {
    #[prost(int32, optional, tag = "1")]
    pub platform: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub app_version: Option<AppVersion>,
    #[prost(string, optional, tag = "3")]
    pub mcc: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub mnc: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub os_version: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub device: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub os_build_number: Option<String>,
    #[prost(int32, optional, tag = "10")]
    pub release_channel: Option<i32>,
    #[prost(string, optional, tag = "11")]
    pub locale_language_iso6391: Option<String>,
    #[prost(string, optional, tag = "12")]
    pub locale_country_iso31661_alpha2: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AppVersion {
    #[prost(uint32, optional, tag = "1")]
    pub primary: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub secondary: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub tertiary: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WebInfo {
    #[prost(int32, optional, tag = "4")]
    pub web_sub_platform: Option<i32>,
}

/// libsignal's `SenderKeyRecordStructure`, the stored form of one member's group keys.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SenderKeyRecordStructure {
    #[prost(message, repeated, tag = "1")]
    pub states: Vec<SenderKeyStateStructure>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SenderKeyStateStructure {
    #[prost(uint32, tag = "1")]
    pub chain_id: u32,
    #[prost(message, optional, tag = "2")]
    pub chain_key: Option<SenderChainKey>,
    #[prost(message, optional, tag = "3")]
    pub signing_key: Option<SenderSigningKey>,
    #[prost(message, repeated, tag = "4")]
    pub message_keys: Vec<SenderChainKey>,
}

/// A chain position: its iteration and the seed or chain key at it.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SenderChainKey {
    #[prost(uint32, tag = "1")]
    pub iteration: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub seed: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SenderSigningKey {
    #[prost(bytes = "vec", tag = "1")]
    pub public: Vec<u8>,
}
