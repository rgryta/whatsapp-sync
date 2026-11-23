//! The device's state in Postgres: its keys, and its Signal sessions with every contact.
//!
//! Its own schema, `device`, with its own migrations, applied by this crate and nothing else.
//! The archive's migrations (`db-migrations/pg`) are checked by the MCP on startup, so a device
//! table there would tie the MCP's deploys to this client's; and `public` grants the MCP role
//! read access to every new table, which must never include these private keys.
//!
//! [`save`] writes a whole [`Device`] in one transaction: the one-time import from WAHA. After
//! that, decryption changes a session, an identity or a sender key at a time, and each has its
//! own small write.

use std::collections::{BTreeMap, HashMap};

use sqlx::postgres::PgPool;
use sqlx::{Postgres, Row, Transaction};

use crate::baileys::{Account, Device, ImportStats, Me, SignedPreKey};
use crate::group::SenderKeyRecord;
use crate::signal::curve::{KeyPair, PrivateKey, PublicKey};
use crate::signal::{Address, SessionRecord};

pub const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_device.sql",
    include_str!("../migrations/0001_device.sql"),
)];

const BOOTSTRAP: &str = "CREATE SCHEMA IF NOT EXISTS device; \
    CREATE TABLE IF NOT EXISTS device.schema_migration ( \
        filename text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("stored {0} does not decode")]
    Corrupt(&'static str),
}

/// Applies every not-yet-applied device migration, returning their names. One transaction,
/// like `whatsapp_core::db::ensure_schema`: the migration table's primary key makes a second
/// process starting at the same moment skip rather than apply twice.
pub async fn migrate(pool: &PgPool) -> Result<Vec<&'static str>, StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(BOOTSTRAP).execute(&mut *tx).await?;
    let mut applied = Vec::new();
    for (filename, sql) in MIGRATIONS {
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO device.schema_migration (filename) VALUES ($1) \
             ON CONFLICT (filename) DO NOTHING RETURNING filename",
        )
        .bind(filename)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_some() {
            sqlx::raw_sql(*sql).execute(&mut *tx).await?;
            applied.push(*filename);
        }
    }
    tx.commit().await?;
    Ok(applied)
}

/// Replaces everything stored with `device`.
pub async fn save(pool: &PgPool, device: &Device) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "TRUNCATE device.account, device.pre_key, device.session, device.identity, \
         device.sender_key, device.app_state_key, device.lid_mapping",
    )
    .execute(&mut *tx)
    .await?;
    let account = device.account.as_ref();
    let me = device.me.as_ref();
    sqlx::query(
        "INSERT INTO device.account (id, registration_id, noise_private, identity_private, \
         signed_pre_key_id, signed_pre_key_private, signed_pre_key_signature, adv_secret, \
         adv_details, adv_account_signature_key, adv_account_signature, adv_device_signature, \
         me_id, me_lid, me_name, platform, routing_info, next_pre_key_id, first_unuploaded_pre_key_id) \
         VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(device.registration_id as i32)
    .bind(device.noise.private.serialize().to_vec())
    .bind(device.identity.private.serialize().to_vec())
    .bind(device.signed_pre_key.id as i32)
    .bind(device.signed_pre_key.key_pair.private.serialize().to_vec())
    .bind(&device.signed_pre_key.signature)
    .bind(&device.adv_secret)
    .bind(account.map(|a| &a.details))
    .bind(account.map(|a| &a.account_signature_key))
    .bind(account.map(|a| &a.account_signature))
    .bind(account.map(|a| &a.device_signature))
    .bind(me.map(|m| &m.id))
    .bind(me.and_then(|m| m.lid.as_ref()))
    .bind(me.and_then(|m| m.name.as_ref()))
    .bind(&device.platform)
    .bind(&device.routing_info)
    .bind(device.next_pre_key_id as i32)
    .bind(device.first_unuploaded_pre_key_id as i32)
    .execute(&mut *tx)
    .await?;
    for (id, key_pair) in &device.pre_keys {
        sqlx::query("INSERT INTO device.pre_key (id, private) VALUES ($1, $2)")
            .bind(*id as i32)
            .bind(key_pair.private.serialize().to_vec())
            .execute(&mut *tx)
            .await?;
    }
    for (address, record) in &device.sessions {
        upsert_session(&mut tx, address, record).await?;
    }
    for (address, key) in &device.identities {
        upsert_identity(&mut tx, address, key).await?;
    }
    for ((group, address), record) in &device.sender_keys {
        upsert_sender_key(&mut tx, group, address, record).await?;
    }
    for (key_id, data) in &device.app_state_keys {
        sqlx::query("INSERT INTO device.app_state_key (key_id, data) VALUES ($1, $2)")
            .bind(key_id)
            .bind(data)
            .execute(&mut *tx)
            .await?;
    }
    for (pn, lid) in &device.lid_mappings {
        sqlx::query("INSERT INTO device.lid_mapping (pn, lid) VALUES ($1, $2)")
            .bind(pn)
            .bind(lid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The stored device, or `None` before anything was saved.
pub async fn load(pool: &PgPool) -> Result<Option<Device>, StoreError> {
    let Some(row) = sqlx::query("SELECT * FROM device.account WHERE id = 1")
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let key_pair = |column: &str| -> Result<KeyPair, StoreError> {
        let private = PrivateKey::deserialize(&row.try_get::<Vec<u8>, _>(column)?)
            .map_err(|_| StoreError::Corrupt("private key"))?;
        Ok(KeyPair {
            public: private.public_key(),
            private,
        })
    };
    let account = match row.try_get::<Option<Vec<u8>>, _>("adv_details")? {
        Some(details) => Some(Account {
            details,
            account_signature_key: row.try_get("adv_account_signature_key")?,
            account_signature: row.try_get("adv_account_signature")?,
            device_signature: row.try_get("adv_device_signature")?,
        }),
        None => None,
    };
    let me = row
        .try_get::<Option<String>, _>("me_id")?
        .map(|id| -> Result<Me, sqlx::Error> {
            Ok(Me {
                id,
                lid: row.try_get("me_lid")?,
                name: row.try_get("me_name")?,
            })
        });
    let mut device = Device {
        noise: key_pair("noise_private")?,
        identity: key_pair("identity_private")?,
        registration_id: row.try_get::<i32, _>("registration_id")? as u32,
        signed_pre_key: SignedPreKey {
            id: row.try_get::<i32, _>("signed_pre_key_id")? as u32,
            key_pair: key_pair("signed_pre_key_private")?,
            signature: row.try_get("signed_pre_key_signature")?,
        },
        adv_secret: row.try_get("adv_secret")?,
        account,
        me: me.transpose()?,
        platform: row.try_get("platform")?,
        routing_info: row.try_get("routing_info")?,
        next_pre_key_id: row.try_get::<i32, _>("next_pre_key_id")? as u32,
        first_unuploaded_pre_key_id: row.try_get::<i32, _>("first_unuploaded_pre_key_id")? as u32,
        pre_keys: Vec::new(),
        sessions: Vec::new(),
        identities: Vec::new(),
        sender_keys: HashMap::new(),
        app_state_keys: BTreeMap::new(),
        lid_mappings: BTreeMap::new(),
        stats: ImportStats::default(),
    };
    for row in sqlx::query("SELECT id, private FROM device.pre_key ORDER BY id")
        .fetch_all(pool)
        .await?
    {
        let private = PrivateKey::deserialize(&row.try_get::<Vec<u8>, _>("private")?)
            .map_err(|_| StoreError::Corrupt("pre-key"))?;
        device.pre_keys.push((
            row.try_get::<i32, _>("id")? as u32,
            KeyPair {
                public: private.public_key(),
                private,
            },
        ));
    }
    for row in sqlx::query("SELECT name, device, record FROM device.session ORDER BY name, device")
        .fetch_all(pool)
        .await?
    {
        let record = SessionRecord::deserialize(&row.try_get::<Vec<u8>, _>("record")?)
            .map_err(|_| StoreError::Corrupt("session"))?;
        device.sessions.push((address(&row)?, record));
    }
    for row in sqlx::query("SELECT name, device, key FROM device.identity ORDER BY name, device")
        .fetch_all(pool)
        .await?
    {
        let key = PublicKey::deserialize(&row.try_get::<Vec<u8>, _>("key")?)
            .map_err(|_| StoreError::Corrupt("identity"))?;
        device.identities.push((address(&row)?, key));
    }
    for row in sqlx::query("SELECT group_id, name, device, record FROM device.sender_key")
        .fetch_all(pool)
        .await?
    {
        let record = SenderKeyRecord::deserialize(&row.try_get::<Vec<u8>, _>("record")?)
            .map_err(|_| StoreError::Corrupt("sender key"))?;
        device
            .sender_keys
            .insert((row.try_get("group_id")?, address(&row)?), record);
    }
    for row in sqlx::query("SELECT key_id, data FROM device.app_state_key")
        .fetch_all(pool)
        .await?
    {
        device
            .app_state_keys
            .insert(row.try_get("key_id")?, row.try_get("data")?);
    }
    for row in sqlx::query("SELECT pn, lid FROM device.lid_mapping")
        .fetch_all(pool)
        .await?
    {
        device
            .lid_mappings
            .insert(row.try_get("pn")?, row.try_get("lid")?);
    }
    Ok(Some(device))
}

pub async fn save_session(
    pool: &PgPool,
    address: &Address,
    record: &SessionRecord,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    upsert_session(&mut tx, address, record).await?;
    Ok(tx.commit().await?)
}

pub async fn save_identity(
    pool: &PgPool,
    address: &Address,
    key: &PublicKey,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    upsert_identity(&mut tx, address, key).await?;
    Ok(tx.commit().await?)
}

pub async fn save_sender_key(
    pool: &PgPool,
    group: &str,
    address: &Address,
    record: &SenderKeyRecord,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    upsert_sender_key(&mut tx, group, address, record).await?;
    Ok(tx.commit().await?)
}

/// Freshly generated pre-keys, saved before they are uploaded: a key the server hands out
/// that this device no longer has would make every session started with it fail.
pub async fn save_pre_keys(pool: &PgPool, keys: &[(u32, KeyPair)]) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    for (id, key_pair) in keys {
        sqlx::query(
            "INSERT INTO device.pre_key (id, private) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        )
        .bind(*id as i32)
        .bind(key_pair.private.serialize().to_vec())
        .execute(&mut *tx)
        .await?;
    }
    Ok(tx.commit().await?)
}

/// The pre-key counters, Baileys' `nextPreKeyId` and `firstUnuploadedPreKeyId`.
pub async fn set_pre_key_counters(
    pool: &PgPool,
    next: u32,
    first_unuploaded: u32,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE device.account SET next_pre_key_id = $1, first_unuploaded_pre_key_id = $2 WHERE id = 1")
        .bind(next as i32)
        .bind(first_unuploaded as i32)
        .execute(pool)
        .await?;
    Ok(())
}

/// The server's routing hint, sent again on every connection.
pub async fn set_routing_info(pool: &PgPool, routing_info: &[u8]) -> Result<(), StoreError> {
    sqlx::query("UPDATE device.account SET routing_info = $1 WHERE id = 1")
        .bind(routing_info)
        .execute(pool)
        .await?;
    Ok(())
}

/// A phone number's LID, learned from a stanza.
pub async fn save_lid_mapping(pool: &PgPool, pn: &str, lid: &str) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO device.lid_mapping (pn, lid) VALUES ($1, $2) ON CONFLICT (pn) DO UPDATE SET lid = EXCLUDED.lid")
        .bind(pn)
        .bind(lid)
        .execute(pool)
        .await?;
    Ok(())
}

/// A one-time pre-key is used exactly once; deleting it is what makes that so.
pub async fn remove_pre_key(pool: &PgPool, id: u32) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM device.pre_key WHERE id = $1")
        .bind(id as i32)
        .execute(pool)
        .await?;
    Ok(())
}

async fn upsert_session(
    tx: &mut Transaction<'_, Postgres>,
    address: &Address,
    record: &SessionRecord,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO device.session (name, device, record) VALUES ($1, $2, $3) \
         ON CONFLICT (name, device) DO UPDATE SET record = EXCLUDED.record",
    )
    .bind(&address.name)
    .bind(address.device as i32)
    .bind(record.serialize())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_identity(
    tx: &mut Transaction<'_, Postgres>,
    address: &Address,
    key: &PublicKey,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO device.identity (name, device, key) VALUES ($1, $2, $3) \
         ON CONFLICT (name, device) DO UPDATE SET key = EXCLUDED.key",
    )
    .bind(&address.name)
    .bind(address.device as i32)
    .bind(key.serialize().to_vec())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_sender_key(
    tx: &mut Transaction<'_, Postgres>,
    group: &str,
    address: &Address,
    record: &SenderKeyRecord,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO device.sender_key (group_id, name, device, record) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (group_id, name, device) DO UPDATE SET record = EXCLUDED.record",
    )
    .bind(group)
    .bind(&address.name)
    .bind(address.device as i32)
    .bind(record.serialize())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn address(row: &sqlx::postgres::PgRow) -> Result<Address, sqlx::Error> {
    Ok(Address {
        name: row.try_get("name")?,
        device: row.try_get::<i32, _>("device")? as u32,
    })
}
