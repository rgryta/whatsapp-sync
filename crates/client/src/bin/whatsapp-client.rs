//! WhatsApp's linked device, archiving into Postgres: what WAHA and the Python ingester did.
//!
//! Configuration, from the environment:
//!
//! - `WHATSAPP_DSN`: the ingest role's DSN. This process owns the archive schema, as the Python
//!   ingester did, and the device's own `device` schema.
//! - `WHATSAPP_IMPORT_FROM` (first start only): a Baileys session directory to adopt, WAHA's,
//!   when the database holds no device yet. Ignored once one is stored.
//! - `WHATSAPP_WHISPER_URL`: the whisper.cpp server voice notes are sent to,
//!   `http://whisper-cpp:8080` by default.
//! - `RUST_LOG`: log filter, `info` by default.

use std::process::ExitCode;

use whatsapp_client::daemon::Client;
use whatsapp_client::login::LoginSettings;
use whatsapp_client::{baileys, store, transcribe};
use whatsapp_core::db::{create_pool, ensure_schema};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Ok(dsn) = std::env::var("WHATSAPP_DSN") else {
        tracing::error!("WHATSAPP_DSN is not set");
        return ExitCode::from(2);
    };
    let pool = match create_pool(&dsn).await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!("cannot reach the database: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = ensure_schema(&pool).await {
        tracing::error!("archive migrations failed: {e}");
        return ExitCode::FAILURE;
    }
    match store::migrate(&pool).await {
        Ok(applied) if !applied.is_empty() => {
            tracing::info!("applied device migrations: {}", applied.join(", "))
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("device migrations failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    let device = match store::load(&pool).await {
        Ok(Some(device)) => device,
        Ok(None) => match std::env::var("WHATSAPP_IMPORT_FROM") {
            Ok(dir) => match baileys::import(dir.as_ref()) {
                Ok(device) => {
                    if let Err(e) = store::save(&pool, &device).await {
                        tracing::error!("saving the imported device failed: {e}");
                        return ExitCode::FAILURE;
                    }
                    tracing::info!("adopted the device from the Baileys session");
                    device
                }
                Err(e) => {
                    tracing::error!("import failed: {}", e.reason);
                    return ExitCode::FAILURE;
                }
            },
            Err(_) => {
                tracing::error!("no device stored and WHATSAPP_IMPORT_FROM not set");
                return ExitCode::from(2);
            }
        },
        Err(e) => {
            tracing::error!("loading the device failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let whisper =
        std::env::var("WHATSAPP_WHISPER_URL").unwrap_or_else(|_| "http://whisper-cpp:8080".into());
    tokio::spawn(transcribe::Worker::new(pool.clone(), &whisper).run(transcribe::DEFAULT_INTERVAL));
    Client::new(pool, device, LoginSettings::default())
        .run()
        .await;
    ExitCode::SUCCESS
}
