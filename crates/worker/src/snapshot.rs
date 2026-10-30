//! `export-snapshot` (ADR-0006 §5, ADR-0007): build the canonical snapshot of the public roadmaps, sign it as a Sui
//! personal message, store it on Walrus through the publisher, and record the result.
//!
//! The document is `{"schema", "human", "bot"}`: the two roadmaps exactly as the public routes serve them (the same
//! function builds them), canonicalised, so unchanged data gives identical bytes and an identical SHA-256.
//!
//! The caps (ADR-0007 §3): at most one **new** snapshot per `min_interval`; an unchanged snapshot is stored again only
//! when its storage is within `renew_margin` of ending; never more than 1 MiB; epochs fixed by configuration; mainnet
//! only when explicitly allowed. A run that stores nothing records nothing it could not store.

use std::time::Duration;

use chrono::{DateTime, Utc};
use content_hub_api::public::build_roadmap;
use content_hub_core::snapshot::{SCHEMA, Signer, SnapshotError, canonical, sha256_hex};
use content_hub_publish::blob::{BlobError, BlobStore};
use content_hub_store::repo::public::Audience;
use content_hub_store::repo::{audit, snapshots};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

/// The audit-log actor of an export.
pub const EXPORT_ACTOR: &str = "system:snapshot";

#[derive(Debug, Clone)]
pub struct ExportConfig {
    /// `localnet`, `testnet` or `mainnet`.
    pub network: String,
    /// Epochs to store for (1 to 53).
    pub epochs: u32,
    /// The least time between two snapshots of different content.
    pub min_interval: Duration,
    /// Store an unchanged snapshot again when its storage ends within this long.
    pub renew_margin: Duration,
    /// The length of an epoch on the network (testnet 1 day, mainnet 14 days).
    pub epoch_len: Duration,
}

impl ExportConfig {
    /// Defaults per network: testnet stores 30 epochs (about 30 days), mainnet 2 (28 days, renewed).
    pub fn for_network(network: &str) -> Result<ExportConfig, String> {
        let (epochs, epoch_days) = match network {
            "localnet" | "testnet" => (30, 1),
            "mainnet" => (2, 14),
            other => return Err(format!("SNAPSHOT_NETWORK {other:?} must be localnet, testnet or mainnet")),
        };
        Ok(ExportConfig {
            network: network.to_owned(),
            epochs,
            min_interval: Duration::from_secs(24 * 3600),
            renew_margin: Duration::from_secs(7 * 24 * 3600),
            epoch_len: Duration::from_secs(epoch_days * 24 * 3600),
        })
    }

    /// When storage bought now ends, conservatively: storage runs to the start of the end epoch, which is at least
    /// `epochs − 1` whole epochs away.
    pub fn estimated_expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let epochs = i64::from(self.epochs.saturating_sub(1));
        now + chrono::Duration::from_std(self.epoch_len).unwrap_or_default() * i32::try_from(epochs).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Nothing changed and the storage is not near its end.
    Unchanged { sha256: String, expires_at: Option<DateTime<Utc>> },
    /// The content changed, but the last snapshot is younger than the minimum interval.
    TooSoon { sha256: String, next_after: DateTime<Utc> },
    /// Another export is running.
    Busy,
    /// Signed and recorded without storage (no publisher configured).
    Signed { sha256: String, signer: String },
    /// Signed, stored and recorded.
    Stored { sha256: String, signer: String, blob_id: String, expires_at: DateTime<Utc>, renewed: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Store(#[from] content_hub_store::StoreError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("the roadmap could not be read: {0}")]
    Read(String),
}

impl From<sqlx::Error> for ExportError {
    fn from(e: sqlx::Error) -> Self {
        ExportError::Store(e.into())
    }
}

/// The document that is signed, from the live data (the public views).
pub async fn build_document(conn: &mut sqlx::PgConnection) -> Result<serde_json::Value, ExportError> {
    let human = build_roadmap(conn, Audience::Human).await.map_err(|e| ExportError::Read(e.to_string()))?;
    let bot = build_roadmap(conn, Audience::Bot).await.map_err(|e| ExportError::Read(e.to_string()))?;
    Ok(json!({
        "schema": SCHEMA,
        "human": serde_json::to_value(&human).map_err(|e| ExportError::Read(e.to_string()))?,
        "bot": serde_json::to_value(&bot).map_err(|e| ExportError::Read(e.to_string()))?,
    }))
}

/// Run one export. `store` is `None` for a signing-only run (nothing is stored).
pub async fn export<S: BlobStore>(
    db: &PgPool,
    signer: &Signer,
    store: Option<&S>,
    cfg: &ExportConfig,
    now: DateTime<Utc>,
) -> Result<Outcome, ExportError> {
    let mut conn = db.acquire().await?;
    if !snapshots::try_lock(&mut conn).await? {
        return Ok(Outcome::Busy);
    }
    let result = export_locked(&mut conn, signer, store, cfg, now).await;
    // Released explicitly: the connection goes back to the pool, which would keep a session-level lock.
    let _ = snapshots::unlock(&mut conn).await;
    result
}

async fn export_locked<S: BlobStore>(
    conn: &mut sqlx::PgConnection,
    signer: &Signer,
    store: Option<&S>,
    cfg: &ExportConfig,
    now: DateTime<Utc>,
) -> Result<Outcome, ExportError> {
    let doc = build_document(conn).await?;
    let bytes = canonical(&doc)?;
    let sha256 = sha256_hex(&bytes);
    let latest = snapshots::latest(conn, &cfg.network).await?;

    let mut renewed = false;
    if let Some(last) = &latest {
        if last.sha256 == sha256 {
            let healthy = last.blob_id.is_some()
                && last.expires_at.is_some_and(|e| (e - now).to_std().is_ok_and(|left| left > cfg.renew_margin));
            // Nothing to do: the same bytes, stored for long enough (or no store to renew with).
            if healthy || store.is_none() {
                return Ok(Outcome::Unchanged { sha256, expires_at: last.expires_at });
            }
            renewed = true;
        } else {
            let next_after = last.generated_at + chrono::Duration::from_std(cfg.min_interval).unwrap_or_default();
            if now < next_after {
                return Ok(Outcome::TooSoon { sha256, next_after });
            }
        }
    }

    let signature = signer.sign_personal_message(&bytes);
    let address = signer.address();
    let stored = match store {
        Some(s) => Some(s.put(&bytes, cfg.epochs).await?),
        None => None,
    };
    let expires_at = stored.as_ref().map(|_| cfg.estimated_expiry(now));
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = snapshots::upsert(
        &mut tx,
        &snapshots::NewSnapshot {
            network: &cfg.network,
            generated_at: now,
            schema_version: 1,
            sha256: &sha256,
            byte_size: i32::try_from(bytes.len()).unwrap_or(i32::MAX),
            signature: &signature,
            signer_address: &address,
            blob_id: stored.as_ref().map(|b| b.blob_id.as_str()),
            object_id: stored.as_ref().and_then(|b| b.object_id.as_deref()),
            epochs: stored.as_ref().map(|_| i32::try_from(cfg.epochs).unwrap_or(i32::MAX)),
            expires_at,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        EXPORT_ACTOR,
        if renewed { "snapshot.renew" } else { "snapshot.export" },
        "snapshot",
        Some(row.id.to_string()),
        None,
        Some(
            json!({ "network": row.network, "sha256": row.sha256, "signer": row.signer_address, "blob_id": row.blob_id,
                      "expires_at": row.expires_at }),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(match (stored, expires_at) {
        (Some(b), Some(expires_at)) => {
            Outcome::Stored { sha256, signer: address, blob_id: b.blob_id, expires_at, renewed }
        }
        _ => Outcome::Signed { sha256, signer: address },
    })
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
