//! One test binary, so one testcontainer is started for the whole suite.
//!
//! Rust has no session-scoped fixture, and a separate `tests/*.rs` file would mean a
//! separate process and a separate Postgres container each. The modules below share
//! one, taking turns through a mutex because they share its database.
//!
//! The module comments name the `tests/test_*.py` file each was ported from. Those
//! files are still in the tree and still run: this phase adds the Rust beside the
//! Python rather than replacing it.

mod isolation;
mod schema;
mod server;
mod store;
mod support;
mod tools;
mod writes;
