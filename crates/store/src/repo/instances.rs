//! Platform instances (one post on one target) and their delivery attempts: what the admin API needs. The
//! worker's claim, finish and janitor queries are in [`super::delivery`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Instance {
    pub id: i64,
    pub post_id: i64,
    pub platform_target_id: i64,
    pub override_title: Option<String>,
    pub override_body: Option<String>,
    pub status: String,
    pub external_status: Option<String>,
    pub url_or_external_id: Option<String>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub posted_at: Option<DateTime<Utc>>,
    pub sending_started_at: Option<DateTime<Utc>>,
    pub attempt_count: i32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub idempotency_key: Uuid,
    pub cost_estimate_usd_micros: i64,
    pub metrics: Value,
    /// The post revision the payload was rendered from (the post may have changed since).
    pub post_revision: Option<i32>,
    /// The payload exactly as validated and as the worker sends it.
    pub payload_title: Option<String>,
    pub payload_body: Option<String>,
    pub payload_sha256: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A rendered, validated payload (the enqueue step's output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub post_revision: i32,
    pub title: String,
    pub body: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Attempt {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    pub http_status: Option<i32>,
    pub response_excerpt: Option<String>,
    pub request_redacted: Option<Value>,
    pub dry_run: bool,
    pub rendered_payload_sha256: String,
}

#[derive(Debug, Clone, Default)]
pub struct InstanceFilter {
    pub status: Option<String>,
    pub platform_target_id: Option<i64>,
    pub post_id: Option<i64>,
}

pub async fn list(conn: &mut PgConnection, f: &InstanceFilter, page: PageRequest) -> Result<Page<Instance>> {
    let rows = sqlx::query_as!(
        Instance,
        r#"SELECT * FROM hub.platform_instances
            WHERE id < $1 AND ($2::text IS NULL OR status = $2) AND ($3::bigint IS NULL OR platform_target_id = $3)
              AND ($4::bigint IS NULL OR post_id = $4)
            ORDER BY id DESC LIMIT $5"#,
        page.bound(),
        f.status,
        f.platform_target_id,
        f.post_id,
        page.fetch()
    )
    .fetch_all(conn)
    .await?;
    Ok(page.page(rows, |r| r.id))
}

pub async fn for_post(conn: &mut PgConnection, post_id: i64) -> Result<Vec<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE post_id = $1 ORDER BY id", post_id)
        .fetch_all(conn)
        .await?)
}

pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE id = $1", id).fetch_optional(conn).await?)
}

pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

pub async fn attempts(conn: &mut PgConnection, id: i64) -> Result<Vec<Attempt>> {
    Ok(sqlx::query_as!(
        Attempt,
        r#"SELECT id, started_at, finished_at, outcome, http_status, response_excerpt, request_redacted, dry_run, rendered_payload_sha256
             FROM hub.delivery_attempts WHERE platform_instance_id = $1 ORDER BY id DESC"#,
        id
    )
    .fetch_all(conn)
    .await?)
}

/// A new instance (from the enqueue step). The database refuses a second live instance on the same target.
pub struct NewInstance<'a> {
    pub post_id: i64,
    pub platform_target_id: i64,
    pub override_title: Option<&'a str>,
    pub override_body: Option<&'a str>,
    pub status: &'a str,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub cost_estimate_usd_micros: i64,
    pub payload: &'a Payload,
    /// Why it is not queued (a cap reached at enqueue), shown to the operator.
    pub last_error: Option<&'a str>,
}

pub async fn insert(conn: &mut PgConnection, n: &NewInstance<'_>) -> Result<Instance> {
    Ok(sqlx::query_as!(
        Instance,
        r#"INSERT INTO hub.platform_instances (post_id, platform_target_id, override_title, override_body, status, scheduled_at,
                                               cost_estimate_usd_micros, post_revision, payload_title, payload_body,
                                               payload_sha256, last_error)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING *"#,
        n.post_id,
        n.platform_target_id,
        n.override_title,
        n.override_body,
        n.status,
        n.scheduled_at,
        n.cost_estimate_usd_micros,
        n.payload.post_revision,
        n.payload.title,
        n.payload.body,
        n.payload.sha256,
        n.last_error
    )
    .fetch_one(conn)
    .await?)
}

/// Replace the payload and cost estimate (an operator's re-queue re-renders and re-validates first).
pub async fn set_payload(conn: &mut PgConnection, id: i64, p: &Payload, cost_estimate_usd_micros: i64) -> Result<()> {
    sqlx::query!(
        r#"UPDATE hub.platform_instances SET post_revision = $2, payload_title = $3, payload_body = $4, payload_sha256 = $5,
                  cost_estimate_usd_micros = $6
            WHERE id = $1"#,
        id,
        p.post_revision,
        p.title,
        p.body,
        p.sha256,
        cost_estimate_usd_micros
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// An operator's move (the API checks the state machine first). `posted` records the time and the URL or id.
pub async fn operator_move(
    conn: &mut PgConnection,
    id: i64,
    to: &str,
    url_or_external_id: Option<&str>,
    scheduled_at: Option<DateTime<Utc>>,
) -> Result<Instance> {
    Ok(sqlx::query_as!(
        Instance,
        r#"UPDATE hub.platform_instances SET status = $2,
                  url_or_external_id = COALESCE($3, url_or_external_id),
                  posted_at = CASE WHEN $2 = 'posted' THEN now() ELSE posted_at END,
                  scheduled_at = CASE WHEN $2 = 'queued' THEN COALESCE($4, now()) ELSE scheduled_at END,
                  next_attempt_at = NULL,
                  sending_started_at = NULL,
                  attempt_count = CASE WHEN $2 = 'queued' THEN 0 ELSE attempt_count END,
                  last_error = CASE WHEN $2 = 'queued' THEN NULL ELSE last_error END
            WHERE id = $1 RETURNING *"#,
        id,
        to,
        url_or_external_id,
        scheduled_at
    )
    .fetch_one(conn)
    .await?)
}

/// Set or clear the third party's own status (a KOL's or a directory's progress).
pub async fn set_external_status(conn: &mut PgConnection, id: i64, external: Option<&str>) -> Result<Instance> {
    Ok(sqlx::query_as!(
        Instance,
        "UPDATE hub.platform_instances SET external_status = $2 WHERE id = $1 RETURNING *",
        id,
        external
    )
    .fetch_one(conn)
    .await?)
}

/// Upcoming queued instances (the schedule view).
pub async fn upcoming(conn: &mut PgConnection, until: DateTime<Utc>) -> Result<Vec<Instance>> {
    Ok(sqlx::query_as!(
        Instance,
        r#"SELECT * FROM hub.platform_instances WHERE status = 'queued' AND COALESCE(next_attempt_at, scheduled_at) <= $1
            ORDER BY COALESCE(next_attempt_at, scheduled_at), id LIMIT 500"#,
        until
    )
    .fetch_all(conn)
    .await?)
}
