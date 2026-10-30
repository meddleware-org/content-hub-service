//! The numbers behind the worker's `/metrics` (ARCHITECTURE.md §10 "Alerting"), read from the database at scrape
//! time so every replica reports the same values.

use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCount {
    pub status: String,
    pub n: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptCount {
    pub platform: String,
    pub outcome: String,
    pub n: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformSpend {
    pub platform_definition_id: i64,
    pub platform: String,
    pub used_usd_micros: i64,
}

/// Instances per status (every status, zero included).
pub async fn instances_by_status(conn: &mut PgConnection) -> Result<Vec<StatusCount>> {
    Ok(sqlx::query_as!(
        StatusCount,
        r#"SELECT s.status AS "status!", count(i.id)::bigint AS "n!"
             FROM unnest(ARRAY['planned','queued','sending','posted','failed','unknown','manual_required','cancelled']) AS s(status)
             LEFT JOIN hub.platform_instances i ON i.status = s.status
            GROUP BY s.status ORDER BY s.status"#
    )
    .fetch_all(conn)
    .await?)
}

/// Finished attempts per platform and outcome since the start (a counter: attempts are never deleted).
pub async fn attempts(conn: &mut PgConnection) -> Result<Vec<AttemptCount>> {
    Ok(sqlx::query_as!(
        AttemptCount,
        r#"SELECT d.code AS platform, a.outcome AS "outcome!", count(*)::bigint AS "n!"
             FROM hub.delivery_attempts a
             JOIN hub.platform_instances i ON i.id = a.platform_instance_id
             JOIN hub.platform_targets t ON t.id = i.platform_target_id
             JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
            WHERE a.outcome IS NOT NULL
            GROUP BY d.code, a.outcome ORDER BY d.code, a.outcome"#
    )
    .fetch_all(conn)
    .await?)
}

/// This month's (UTC) usage per platform.
pub async fn spend(conn: &mut PgConnection) -> Result<Vec<PlatformSpend>> {
    Ok(sqlx::query_as!(
        PlatformSpend,
        r#"SELECT d.id AS platform_definition_id, d.code AS platform,
                  COALESCE(sum(CASE WHEN l.kind = 'release' THEN -l.amount_usd_micros ELSE l.amount_usd_micros END), 0)::bigint AS "used_usd_micros!"
             FROM hub.spend_ledger l JOIN hub.platform_definitions d ON d.id = l.platform_definition_id
            WHERE l.period_month = date_trunc('month', now() AT TIME ZONE 'UTC')::date
            GROUP BY d.id, d.code ORDER BY d.code"#
    )
    .fetch_all(conn)
    .await?)
}

/// Instances that became `failed` or `unknown` after `since` (for the alert summary).
pub async fn problems_since(conn: &mut PgConnection, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<StatusCount>> {
    Ok(sqlx::query_as!(
        StatusCount,
        r#"SELECT status AS "status!", count(*)::bigint AS "n!" FROM hub.platform_instances
            WHERE status IN ('failed', 'unknown') AND updated_at > $1
            GROUP BY status ORDER BY status"#,
        since
    )
    .fetch_all(conn)
    .await?)
}
