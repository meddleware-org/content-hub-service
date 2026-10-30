//! The signed roadmap snapshots (ADR-0007): one row per (network, SHA-256). Renewing the storage of an unchanged
//! snapshot updates its row (new blob, new expiry); a changed roadmap is a new row.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub id: i64,
    pub network: String,
    pub generated_at: DateTime<Utc>,
    pub schema_version: i32,
    pub sha256: String,
    pub byte_size: i32,
    pub signature: String,
    pub signer_address: String,
    pub blob_id: Option<String>,
    pub object_id: Option<String>,
    pub epochs: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// What to record.
#[derive(Debug, Clone)]
pub struct NewSnapshot<'a> {
    pub network: &'a str,
    pub generated_at: DateTime<Utc>,
    pub schema_version: i32,
    pub sha256: &'a str,
    pub byte_size: i32,
    pub signature: &'a str,
    pub signer_address: &'a str,
    pub blob_id: Option<&'a str>,
    pub object_id: Option<&'a str>,
    pub epochs: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// The most recently generated snapshot of a network.
pub async fn latest(conn: &mut PgConnection, network: &str) -> Result<Option<Snapshot>> {
    Ok(sqlx::query_as!(
        Snapshot,
        r#"SELECT id, network, generated_at, schema_version, sha256, byte_size, signature, signer_address, blob_id, object_id,
                  epochs, expires_at
             FROM hub.snapshots WHERE network = $1 ORDER BY generated_at DESC, id DESC LIMIT 1"#,
        network
    )
    .fetch_optional(conn)
    .await?)
}

/// Insert a snapshot, or (the same bytes stored again) replace its storage details and signature.
pub async fn upsert(conn: &mut PgConnection, n: &NewSnapshot<'_>) -> Result<Snapshot> {
    Ok(sqlx::query_as!(
        Snapshot,
        r#"INSERT INTO hub.snapshots (network, generated_at, schema_version, sha256, byte_size, signature, signer_address,
                                      blob_id, object_id, epochs, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           ON CONFLICT (network, sha256) DO UPDATE SET generated_at = EXCLUDED.generated_at, signature = EXCLUDED.signature,
                  signer_address = EXCLUDED.signer_address, blob_id = EXCLUDED.blob_id, object_id = EXCLUDED.object_id,
                  epochs = EXCLUDED.epochs, expires_at = EXCLUDED.expires_at
           RETURNING id, network, generated_at, schema_version, sha256, byte_size, signature, signer_address, blob_id, object_id,
                     epochs, expires_at"#,
        n.network,
        n.generated_at,
        n.schema_version,
        n.sha256,
        n.byte_size,
        n.signature,
        n.signer_address,
        n.blob_id,
        n.object_id,
        n.epochs,
        n.expires_at
    )
    .fetch_one(conn)
    .await?)
}

/// Try to take the export's session lock (released with [`unlock`] or when the connection closes). Two exports at once
/// would store the same snapshot twice.
pub async fn try_lock(conn: &mut PgConnection) -> Result<bool> {
    Ok(sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock(0x6368626578706f72) AS "locked!""#).fetch_one(conn).await?)
}

pub async fn unlock(conn: &mut PgConnection) -> Result<()> {
    sqlx::query_scalar!(r#"SELECT pg_advisory_unlock(0x6368626578706f72) AS "unlocked!""#).fetch_one(conn).await?;
    Ok(())
}
