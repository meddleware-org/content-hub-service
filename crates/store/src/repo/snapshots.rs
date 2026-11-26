//! The signed roadmap snapshots (ADR-0007): one row per (network, SHA-256). Renewing the storage of an unchanged
//! snapshot updates its row (new blob, new expiry); a changed roadmap is a new row.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

/// One stored roadmap snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// The snapshot's id.
    pub id: i64,
    /// The network it is for.
    pub network: String,
    /// When it was generated.
    pub generated_at: DateTime<Utc>,
    /// The version of the document's format.
    pub schema_version: i32,
    /// The SHA-256 of the document.
    pub sha256: String,
    /// The document's size in bytes.
    pub byte_size: i32,
    /// The Sui personal-message signature, base64.
    pub signature: String,
    /// The signer's Sui address.
    pub signer_address: String,
    /// The Walrus blob id, if stored.
    pub blob_id: Option<String>,
    /// The Sui object id of the blob, if stored.
    pub object_id: Option<String>,
    /// How many Walrus epochs it was stored for.
    pub epochs: Option<i32>,
    /// When the storage is estimated to run out.
    pub expires_at: Option<DateTime<Utc>>,
}

/// What to record.
#[derive(Debug, Clone)]
pub struct NewSnapshot<'a> {
    /// The network.
    pub network: &'a str,
    /// When it was generated.
    pub generated_at: DateTime<Utc>,
    /// The document format version.
    pub schema_version: i32,
    /// The SHA-256 of the document.
    pub sha256: &'a str,
    /// The size in bytes.
    pub byte_size: i32,
    /// The signature, base64.
    pub signature: &'a str,
    /// The signer's Sui address.
    pub signer_address: &'a str,
    /// The Walrus blob id, if stored.
    pub blob_id: Option<&'a str>,
    /// The Sui object id, if stored.
    pub object_id: Option<&'a str>,
    /// The Walrus epochs, if stored.
    pub epochs: Option<i32>,
    /// When the storage runs out, if stored.
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

/// Release the export's session lock.
pub async fn unlock(conn: &mut PgConnection) -> Result<()> {
    sqlx::query_scalar!(r#"SELECT pg_advisory_unlock(0x6368626578706f72) AS "unlocked!""#).fetch_one(conn).await?;
    Ok(())
}
