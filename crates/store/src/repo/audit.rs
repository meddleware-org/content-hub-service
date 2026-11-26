//! The append-only audit log: every admin change records who, what and the before/after state.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use super::{Page, PageRequest};
use crate::Result;

/// One audit log entry: who changed what, with the value before and after.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AuditEntry {
    /// The entry's id.
    pub id: i64,
    /// When the change was made.
    pub at: DateTime<Utc>,
    /// The subject of the person who made it.
    pub actor_subject: String,
    /// What was done.
    pub action: String,
    /// The kind of thing that changed.
    pub entity_type: String,
    /// The thing's id, when it has one.
    pub entity_id: Option<String>,
    /// Its value before.
    pub before: Option<Value>,
    /// Its value after.
    pub after: Option<Value>,
}

/// Record one change (in the caller's transaction).
pub async fn record(
    tx: &mut PgConnection,
    actor: &str,
    action: &str,
    entity_type: &str,
    entity_id: Option<String>,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO hub.audit_log (actor_subject, action, entity_type, entity_id, before, after) VALUES ($1, $2, $3, $4, $5, $6)",
        actor,
        action,
        entity_type,
        entity_id,
        before,
        after
    )
    // The entry is written in the caller's transaction, so a change and its audit row commit or fail together.
    .execute(tx)
    .await?;
    // Nothing to return: the row is its own record.
    Ok(())
}

/// Newest first; optionally one entity's history.
pub async fn list(
    conn: &mut PgConnection,
    // Optionally only this kind of entity.
    entity_type: Option<&str>,
    // Optionally only this entity.
    entity_id: Option<&str>,
    page: PageRequest,
) -> Result<Page<AuditEntry>> {
    let rows = sqlx::query_as!(
        AuditEntry,
        r#"SELECT id, at, actor_subject, action, entity_type, entity_id, before, after
             FROM hub.audit_log
            WHERE id < $1 AND ($2::text IS NULL OR entity_type = $2) AND ($3::text IS NULL OR entity_id = $3)
            ORDER BY id DESC LIMIT $4"#,
        page.bound(),
        entity_type,
        entity_id,
        page.fetch()
    )
    .fetch_all(conn)
    .await?;
    Ok(page.page(rows, |r| r.id))
}
