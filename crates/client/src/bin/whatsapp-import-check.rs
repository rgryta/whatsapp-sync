//! Reads a copy of WAHA's session and reports whether this client could take it over.
//!
//! Prints counts and verdicts only, never a key or an address, so its output is safe to
//! paste. Nothing connects to WhatsApp.
//!
//!     whatsapp-import-check /path/to/a/copy/of/session_<id>

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: whatsapp-import-check <baileys session directory>");
        return ExitCode::from(2);
    };
    let device = match whatsapp_client::baileys::import(dir.as_ref()) {
        Ok(device) => device,
        Err(e) => {
            // The file name can carry a phone number or LID; the kind of file is enough.
            let kind = e
                .file
                .split(|c: char| c.is_ascii_digit())
                .next()
                .unwrap_or("");
            eprintln!("import failed in a {kind}… file: {}", e.reason);
            return ExitCode::FAILURE;
        }
    };
    let stats = &device.stats;
    println!("keys: noise, identity and signed pre-key match their private keys");
    println!("signed pre-key: signed by the identity key");
    match &device.account {
        Some(_) => println!("account: the phone's and this device's signatures verify"),
        None => println!("account: none (this device was never paired)"),
    }
    println!(
        "me: {}",
        if device.me.is_some() {
            "present"
        } else {
            "absent"
        }
    );
    println!(
        "pre-keys: {} (each matches its private key)",
        device.pre_keys.len()
    );
    println!(
        "sessions: {} addresses, {} current, {} archived, {} archived dropped over the limit, {} closed chains",
        device.sessions.len(),
        stats.current_sessions,
        stats.archived_sessions,
        stats.dropped_sessions,
        stats.closed_chains
    );
    println!("identities: {}", device.identities.len());
    println!("group sender keys: {}", device.sender_keys.len());
    println!("app-state keys: {}", device.app_state_keys.len());
    println!("LID mappings: {}", device.lid_mappings.len());
    for (kind, count) in &stats.not_imported {
        println!("not imported: {kind} x{count}");
    }
    ExitCode::SUCCESS
}
