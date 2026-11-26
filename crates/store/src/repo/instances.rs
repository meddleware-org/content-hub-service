//! Platform instances (one post on one target) and their delivery attempts: what the admin API needs. The
//! worker's claim, finish and janitor queries are in [`super::delivery`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

/// One platform instance: a post rendered for one target, and the queue row for it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Instance {
    /// The instance's id.
    pub id: i64,
    /// The post.
    pub post_id: i64,
    /// The target.
    pub platform_target_id: i64,
    /// A title that replaces the post's for this target.
    pub override_title: Option<String>,
    /// A body that replaces the post's for this target.
    pub override_body: Option<String>,
    /// Where the instance stands.
    pub status: String,
    /// Progress reported by a third party, for platforms that have one.
    pub external_status: Option<String>,
    /// The platform's URL or id for the post.
    pub url_or_external_id: Option<String>,
    /// When it is to be sent.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// When it was posted.
    pub posted_at: Option<DateTime<Utc>>,
    /// When the worker claimed it; set only while the status is `sending`.
    pub sending_started_at: Option<DateTime<Utc>>,
    /// How many sends were claimed.
    pub attempt_count: i32,
    /// When the next attempt is due.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The last error, if any.
    pub last_error: Option<String>,
    /// The key that makes a retried send safe where the platform honours it.
    pub idempotency_key: Uuid,
    /// The expected cost, in millionths of a US dollar.
    pub cost_estimate_usd_micros: i64,
    /// Counters the operator records (impressions and the like).
    pub metrics: Value,
    /// The post revision the payload was rendered from (the post may have changed since).
    pub post_revision: Option<i32>,
    /// The payload exactly as validated and as the worker sends it.
    pub payload_title: Option<String>,
    /// The body as validated when it was queued.
    pub payload_body: Option<String>,
    /// The SHA-256 of the payload.
    pub payload_sha256: Option<String>,
    /// When the instance was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A rendered, validated payload (the enqueue step's output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// The post revision the payload was rendered from.
    pub post_revision: i32,
    /// The rendered title.
    pub title: String,
    /// The rendered body.
    pub body: String,
    /// The SHA-256 of the title and body as they will be sent.
    pub sha256: String,
}

/// One send attempt on an instance.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Attempt {
    /// The attempt's id.
    pub id: i64,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished (empty while it is in flight).
    pub finished_at: Option<DateTime<Utc>>,
    /// How it ended.
    pub outcome: Option<String>,
    /// The HTTP status, if there was an answer.
    pub http_status: Option<i32>,
    /// The start of the answer, with credentials removed.
    pub response_excerpt: Option<String>,
    /// The request as sent, with credentials removed.
    pub request_redacted: Option<Value>,
    /// Whether nothing was actually sent.
    pub dry_run: bool,
    /// The SHA-256 of the payload that was (or would have been) sent.
    pub rendered_payload_sha256: String,
}

/// A filter for the list of instances.
#[derive(Debug, Clone, Default)]
pub struct InstanceFilter {
    /// Only this status.
    pub status: Option<String>,
    /// Only this target.
    pub platform_target_id: Option<i64>,
    /// Only this post.
    pub post_id: Option<i64>,
}

/// A page of instances, newest first, matching the filter.
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

/// Every instance of a post.
pub async fn for_post(conn: &mut PgConnection, post_id: i64) -> Result<Vec<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE post_id = $1 ORDER BY id", post_id)
        .fetch_all(conn)
        .await?)
}

/// One instance.
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE id = $1", id).fetch_optional(conn).await?)
}

/// One instance, locked for the rest of the transaction.
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Instance>> {
    Ok(sqlx::query_as!(Instance, "SELECT * FROM hub.platform_instances WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

/// The attempts on an instance, newest first.
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
    /// The post.
    pub post_id: i64,
    /// The target.
    pub platform_target_id: i64,
    /// A replacement title, if any.
    pub override_title: Option<&'a str>,
    /// A replacement body, if any.
    pub override_body: Option<&'a str>,
    /// The starting status.
    pub status: &'a str,
    /// When to send it.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// The expected cost.
    pub cost_estimate_usd_micros: i64,
    /// The validated payload.
    pub payload: &'a Payload,
    /// Why it is not queued (a cap reached at enqueue), shown to the operator.
    pub last_error: Option<&'a str>,
}

/// Insert the instance; the database refuses a second live instance of the same post on the same target.
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
