//! Runtime settings (`hub.settings`): small JSON values the application validates per key.
//!
//! - `publishing.paused` (bool): while true the worker claims nothing (the stop switch).
//! - `alerts.state` (object): when the worker last sent an alert summary, and the spend levels already reported,
//!   so restarts and replicas do not repeat them.

use serde_json::Value;
use sqlx::PgConnection;

use crate::Result;

pub const PUBLISHING_PAUSED: &str = "publishing.paused";
pub const ALERTS_STATE: &str = "alerts.state";

pub async fn get(conn: &mut PgConnection, key: &str) -> Result<Option<Value>> {
    Ok(sqlx::query_scalar!("SELECT value FROM hub.settings WHERE key = $1", key).fetch_optional(conn).await?)
}

pub async fn set(conn: &mut PgConnection, key: &str, value: &Value, by: &str) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO hub.settings (key, value, updated_by) VALUES ($1, $2, $3)
           ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = now()"#,
        key,
        value,
        by
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Whether the worker is stopped (absent or not `true` means running).
pub async fn paused(conn: &mut PgConnection) -> Result<bool> {
    Ok(get(conn, PUBLISHING_PAUSED).await?.and_then(|v| v.as_bool()).unwrap_or(false))
}

/// Lock one setting's row for the rest of the transaction (inserting it first if needed), so two workers do not
/// both act on it.
pub async fn lock(conn: &mut PgConnection, key: &str, initial: &Value) -> Result<Value> {
    sqlx::query!(
        "INSERT INTO hub.settings (key, value, updated_by) VALUES ($1, $2, 'system:worker') ON CONFLICT (key) DO NOTHING",
        key,
        initial
    )
    .execute(&mut *conn)
    .await?;
    Ok(sqlx::query_scalar!("SELECT value FROM hub.settings WHERE key = $1 FOR UPDATE", key).fetch_one(conn).await?)
}
