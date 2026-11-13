//! The codec against WAHA's own Baileys.
//!
//! `fixtures/binary.json` comes from `scripts/baileys-binary-fixture.mjs`: Baileys' token
//! tables, and frames Baileys encoded together with what Baileys decodes them to. Each frame
//! must decode to that structure and encode back to the same bytes.

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use whatsapp_client::binary::{self, Content, Node, tokens};

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "singleByteTokens")]
    single: Vec<String>,
    #[serde(rename = "doubleByteTokens")]
    double: Vec<Vec<String>>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    bytes: String,
    node: Json,
}

fn fixture() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/binary.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The shape the fixture records Baileys' decode in.
fn canonical(node: &Node) -> Json {
    json!({
        "tag": node.tag,
        "attrs": node.attrs.iter().map(|(k, v)| json!([k, v.to_string()])).collect::<Vec<_>>(),
        "content": match &node.content {
            Content::None => Json::Null,
            Content::Nodes(nodes) => Json::Array(nodes.iter().map(canonical).collect()),
            Content::Bytes(bytes) => json!({ "bytes": B64.encode(bytes) }),
            Content::Text(text) => json!({ "text": text }),
            Content::Jid(jid) => json!({ "text": jid.to_string() }),
        },
    })
}

#[test]
fn the_token_tables_are_baileys_tables() {
    let fixture = fixture();
    assert_eq!(tokens::SINGLE_BYTE.to_vec(), fixture.single);
    for (ours, theirs) in tokens::DOUBLE_BYTE.iter().zip(&fixture.double) {
        assert_eq!(ours.to_vec(), *theirs);
    }
    assert_eq!(tokens::DOUBLE_BYTE.len(), fixture.double.len());
}

#[test]
fn every_baileys_frame_decodes_to_baileys_structure_and_encodes_back_byte_for_byte() {
    let fixture = fixture();
    assert!(fixture.cases.len() > 300);
    for case in &fixture.cases {
        let bytes = B64.decode(&case.bytes).unwrap();
        let node = binary::unmarshal(&bytes).unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(
            canonical(&node),
            case.node,
            "{}: decoded structure",
            case.name
        );
        assert_eq!(
            binary::marshal(&node),
            bytes,
            "{}: re-encoded bytes",
            case.name
        );
    }
}

#[test]
fn a_compressed_frame_inflates_to_the_same_node() {
    let fixture = fixture();
    let case = fixture
        .cases
        .iter()
        .find(|c| c.name.starts_with("300 children"))
        .unwrap();
    let bytes = B64.decode(&case.bytes).unwrap();
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(&bytes[1..]).unwrap();
    let compressed = [&[2u8][..], &zlib.finish().unwrap()].concat();
    assert_eq!(
        binary::unmarshal(&compressed).unwrap(),
        binary::unmarshal(&bytes).unwrap()
    );
}

#[test]
fn a_cut_short_or_padded_frame_is_an_error_not_a_panic() {
    let fixture = fixture();
    for case in fixture.cases.iter().take(60) {
        let bytes = B64.decode(&case.bytes).unwrap();
        for end in 0..bytes.len() {
            assert!(
                binary::unmarshal(&bytes[..end]).is_err(),
                "{}: cut at {end}",
                case.name
            );
        }
        let padded = [&bytes[..], &[0u8]].concat();
        assert!(
            binary::unmarshal(&padded).is_err(),
            "{}: trailing byte",
            case.name
        );
    }
}

#[test]
fn list_length_switches_to_sixteen_bits_at_256_as_baileys_does() {
    let list = |n: usize| Node {
        content: Content::Nodes(vec![Node::new("a"); n]),
        ..Node::new("iq")
    };
    // Flags byte, the node's 2-item list header, the `iq` token, then the child list header.
    assert_eq!(&binary::marshal(&list(255))[4..6], &[248, 255]);
    assert_eq!(&binary::marshal(&list(256))[4..7], &[249, 1, 0]);
    for n in [255, 256] {
        assert_eq!(
            binary::unmarshal(&binary::marshal(&list(n))).unwrap(),
            list(n)
        );
    }
}
