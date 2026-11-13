//! The isolation guard.
//!
//! The MCP process must not be able to reach WhatsApp, and in Cargo that is a link-time
//! fact: the binary's package does not depend on `whatsapp-client`, so the code that holds
//! the device's keys is not in the process at all. What is left to check is that the
//! manifests still say so, and that the tools layer still holds no SQL of its own.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn manifest(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One manifest section, ending where the next one starts.
///
/// `[dependencies]` is what reaches a binary target; `[dev-dependencies]` does not, and
/// reading to the end of the file would conflate the two.
fn section(path: &str, name: &str) -> String {
    let manifest = manifest(path);
    let after = manifest
        .split_once(&format!("\n[{name}]"))
        .unwrap_or_else(|| panic!("{path} has no [{name}] section"))
        .1;
    after
        .split_once("\n[")
        .map_or(after, |(section, _)| section)
        .to_owned()
}

#[test]
fn the_mcp_binary_does_not_depend_on_the_whatsapp_client() {
    // The root package is the one that carries `src/bin/whatsapp-mcp.rs`.
    let dependencies = section("Cargo.toml", "dependencies");
    // `whatsapp-client` is the WhatsApp device itself, and AGPL besides: the MIT binary
    // that serves the archive links neither.
    assert!(
        !dependencies.contains("whatsapp-client"),
        "the MCP binary must not link whatsapp-client:\n{dependencies}"
    );
}

#[test]
fn neither_the_core_nor_the_mcp_crate_depends_on_the_whatsapp_client() {
    // A transitive edge would put the code back in the binary just as surely.
    for path in ["crates/core/Cargo.toml", "crates/mcp/Cargo.toml"] {
        assert!(
            !manifest(path).contains("whatsapp-client"),
            "{path} must not depend on whatsapp-client"
        );
    }
}

#[test]
fn the_tools_layer_holds_no_sql_of_its_own() {
    // `AGENTS.md`: all SQL lives in the store. `set-alias` is the only write in the
    // whole read surface, and the MCP role is granted nothing else.
    for file in ["crates/mcp/src/tools.rs", "crates/mcp/src/server.rs"] {
        let source = std::fs::read_to_string(repo_root().join(file))
            .expect(file)
            .to_lowercase();
        for statement in ["insert into", "update ", "delete from", "select "] {
            assert!(!source.contains(statement), "raw {statement:?} in {file}");
        }
    }
}
