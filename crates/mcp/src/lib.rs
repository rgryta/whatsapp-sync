//! The read surface.
//!
//! This crate must never depend on `whatsapp-client`: the MCP process must be unable to
//! reach WhatsApp even by accident. The absence of the dependency edge is what the linker
//! enforces, and `tests/archive/isolation.rs` checks the manifests still say so.

pub mod render;
pub mod server;
pub mod tools;
