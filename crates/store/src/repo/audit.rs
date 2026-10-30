//! The append-only audit log: every admin change records who, what and the before/after state.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use super::{Page, PageRequest};
use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AuditEntry {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor_subject: String,
    pub action: String,
    pub entity_type: String,
    pub entity_id: Option<String>,
    pub before: Option<Value>,
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
    .execute(tx)
    .await?;
    Ok(())
}

/// Newest first; optionally one entity's history.
pub async fn list(
    conn: &mut PgConnection,
    entity_type: Option<&str>,
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
