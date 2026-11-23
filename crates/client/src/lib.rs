//! Our own WhatsApp linked device, receive-only.
//!
//! It replaces WAHA by becoming the device WAHA already is: [`baileys::import`] reads WAHA's
//! session, so nothing is paired again. Pairwise messages decrypt through [`signal`], the
//! version-3 path of Signal's libsignal v0.74.1 carried here because later releases removed it;
//! group messages through [`group`], because WhatsApp's group wire format is one libsignal no
//! longer reads.

pub mod baileys;
pub mod binary;
pub mod conn;
pub mod content;
pub mod daemon;
pub mod group;
pub mod login;
pub mod media;
pub mod noise;
pub mod prekeys;
mod proto;
pub mod rows;
pub mod secret;
pub mod signal;
pub mod stanza;
pub mod store;
pub mod transcribe;
