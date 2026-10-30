//! The publishing worker's queries (ARCHITECTURE.md §10): claim a due instance with `FOR UPDATE SKIP LOCKED`, commit
//! `sending` and an open attempt **before** the external call, finish it afterwards, and the janitor that turns a
//! stale `sending` into `unknown`. State moves are checked against the instance state machine by the caller; the
//! queries also guard on the current state, so a row the janitor already took is never overwritten.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Result;

/// A due instance with its target and platform, locked for the claiming transaction.
#[derive(Debug, Clone)]
pub struct Claim {
    pub id: i64,
    pub post_id: i64,
    pub post_public_id: Uuid,
    pub attempt_count: i32,
    pub idempotency_key: Uuid,
    pub cost_estimate_usd_micros: i64,
    pub payload_title: String,
    pub payload_body: String,
    pub payload_sha256: String,
    pub target_id: i64,
    pub target_name: String,
    pub target_external_ref: Option<String>,
    pub target_config: Value,
    pub target_secret_ref: Option<String>,
    pub target_is_sandbox: bool,
    pub target_enabled: bool,
    pub target_live: bool,
    pub platform_definition_id: i64,
    pub platform_code: String,
    pub platform_enabled: bool,
    pub posting_mode: String,
    pub base_url: Option<String>,
    pub counting_method: Option<String>,
    pub primary_limit: Option<i32>,
    pub secondary_limits: Value,
    pub markup: String,
}

/// The next due instance (`queued`, its time reached), locked; rows other workers hold are skipped.
pub async fn next_due(conn: &mut PgConnection) -> Result<Option<Claim>> {
    Ok(sqlx::query_as!(
        Claim,
        r#"SELECT i.id, i.post_id, p.public_id AS post_public_id, i.attempt_count, i.idempotency_key, i.cost_estimate_usd_micros,
                  i.payload_title AS "payload_title!", i.payload_body AS "payload_body!", i.payload_sha256 AS "payload_sha256!",
                  t.id AS target_id, t.name AS target_name, t.external_ref AS target_external_ref, t.config AS target_config,
                  t.secret_ref AS target_secret_ref, t.is_sandbox AS target_is_sandbox, t.enabled AS target_enabled,
                  t.live AS target_live,
                  d.id AS platform_definition_id, d.code AS platform_code, d.enabled AS platform_enabled, d.posting_mode,
                  d.base_url, d.counting_method, d.primary_limit, d.secondary_limits, d.markup
             FROM hub.platform_instances i
             JOIN hub.posts p ON p.id = i.post_id
             JOIN hub.platform_targets t ON t.id = i.platform_target_id
             JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
            WHERE i.status = 'queued' AND COALESCE(i.next_attempt_at, i.scheduled_at) <= now()
            ORDER BY COALESCE(i.next_attempt_at, i.scheduled_at), i.id
            LIMIT 1
              FOR UPDATE OF i SKIP LOCKED"#
    )
    .fetch_optional(conn)
    .await?)
}

/// Sends to a target in the current UTC day (posted today, or in flight).
pub async fn sent_today(conn: &mut PgConnection, target_id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*)::bigint AS "n!" FROM hub.platform_instances
            WHERE platform_target_id = $1
              AND (status = 'sending' OR (status = 'posted' AND posted_at >= date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'))"#,
        target_id
    )
    .fetch_one(conn)
    .await?)
}

/// Leave a queued instance for later (a daily limit): it stays `queued`.
pub async fn defer(conn: &mut PgConnection, id: i64, until: DateTime<Utc>, why: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE hub.platform_instances SET next_attempt_at = $2, last_error = $3 WHERE id = $1 AND status = 'queued'",
        id,
        until,
        why
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// `queued → sending` and an open attempt row, in the claiming transaction (which the caller commits before the
/// external call). Returns the attempt's id.
pub async fn begin_send(conn: &mut PgConnection, id: i64, payload_sha256: &str) -> Result<i64> {
    sqlx::query!(
        r#"UPDATE hub.platform_instances SET status = 'sending', sending_started_at = now(), attempt_count = attempt_count + 1,
                  next_attempt_at = NULL
            WHERE id = $1 AND status = 'queued'"#,
        id
    )
    .execute(&mut *conn)
    .await?;
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO hub.delivery_attempts (platform_instance_id, dry_run, rendered_payload_sha256)
           VALUES ($1, false, $2) RETURNING id"#,
        id,
        payload_sha256
    )
    .fetch_one(conn)
    .await?)
}

/// When the instance's first real attempt started (Mastodon's idempotency window; Bluesky's record key).
pub async fn first_attempt_at(conn: &mut PgConnection, id: i64) -> Result<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar!(
        "SELECT min(started_at) FROM hub.delivery_attempts WHERE platform_instance_id = $1 AND NOT dry_run",
        id
    )
    .fetch_one(conn)
    .await?)
}

/// The record of one finished send.
#[derive(Debug, Clone)]
pub struct Finish<'a> {
    pub to: &'a str,
    pub outcome: &'a str,
    pub url_or_external_id: Option<&'a str>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub last_error: Option<&'a str>,
    pub http_status: Option<i32>,
    pub response_excerpt: Option<&'a str>,
    pub request_redacted: &'a Value,
}

/// Record a send's result. Returns `false` when the instance was no longer this attempt's (the janitor moved it to
/// `unknown` first): the instance is then left for an operator and the caller records the late result.
pub async fn finish(conn: &mut PgConnection, id: i64, attempt_id: i64, f: &Finish<'_>) -> Result<bool> {
    let moved = sqlx::query!(
        r#"UPDATE hub.platform_instances SET status = $2, sending_started_at = NULL,
                  posted_at = CASE WHEN $2 = 'posted' THEN now() ELSE posted_at END,
                  url_or_external_id = COALESCE($3, url_or_external_id),
                  next_attempt_at = $4,
                  last_error = $5
            WHERE id = $1 AND status = 'sending'
              AND EXISTS (SELECT 1 FROM hub.delivery_attempts a
                           WHERE a.id = $6 AND a.platform_instance_id = $1 AND a.finished_at IS NULL)"#,
        id,
        f.to,
        f.url_or_external_id,
        f.next_attempt_at,
        f.last_error,
        attempt_id
    )
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    if moved {
        sqlx::query!(
            r#"UPDATE hub.delivery_attempts SET finished_at = now(), outcome = $2, http_status = $3, response_excerpt = $4,
                      request_redacted = $5
                WHERE id = $1 AND finished_at IS NULL"#,
            attempt_id,
            f.outcome,
            f.http_status,
            f.response_excerpt,
            f.request_redacted
        )
        .execute(conn)
        .await?;
    }
    Ok(moved)
}

/// `queued → manual_required` at claim time (a dry run, or a stop), optionally with a finished dry-run attempt.
pub async fn stop(conn: &mut PgConnection, id: i64, why: &str, dry_run: Option<(&Value, &str)>) -> Result<()> {
    sqlx::query!(
        r#"UPDATE hub.platform_instances SET status = 'manual_required', next_attempt_at = NULL, last_error = $2
            WHERE id = $1 AND status = 'queued'"#,
        id,
        why
    )
    .execute(&mut *conn)
    .await?;
    if let Some((request, sha)) = dry_run {
        sqlx::query!(
            r#"INSERT INTO hub.delivery_attempts (platform_instance_id, finished_at, outcome, request_redacted, dry_run,
                                                  rendered_payload_sha256)
               VALUES ($1, now(), 'dry_run', $2, true, $3)"#,
            id,
            request,
            sha
        )
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// Every `sending` row older than the lease becomes `unknown` (never `queued`), and its open attempt is closed as
/// `unknown`. Returns the instances moved.
pub async fn janitor(conn: &mut PgConnection, lease_seconds: f64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar!(
        r#"WITH stale AS (
             UPDATE hub.platform_instances SET status = 'unknown', sending_started_at = NULL,
                    last_error = 'no result within the claim lease (the worker stopped mid-send?); check the platform, then resolve'
              WHERE status = 'sending' AND sending_started_at < now() - make_interval(secs => $1::double precision)
              RETURNING id),
           closed AS (
             UPDATE hub.delivery_attempts a SET finished_at = now(), outcome = 'unknown',
                    response_excerpt = 'closed by the janitor: no result within the claim lease'
               FROM stale WHERE a.platform_instance_id = stale.id AND a.finished_at IS NULL
             RETURNING a.id)
           SELECT id AS "id!" FROM stale"#,
        lease_seconds
    )
    .fetch_all(conn)
    .await?)
}
