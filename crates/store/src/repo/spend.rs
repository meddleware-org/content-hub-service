//! The spend ledger's writes (ARCHITECTURE.md §2 "Cost caps"): a reservation before the action, under every cap
//! that applies; settlement when the post went out; release when it did not. The ledger is append-only, so a
//! release is a row too, written in the month of the reservation it cancels (that month's usage returns to what it
//! was), while a settlement belongs to the month the money was spent.
//!
//! Usage in a scope = reservations + settlements − releases of the current month (UTC). Reservations are
//! serialised by a transaction-scoped advisory lock, so two enqueues cannot both fit under a cap that only one
//! fits under.

use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

/// The advisory-lock key of the spend ledger (an arbitrary constant, unique to this use).
const SPEND_LOCK: i64 = 0x0063_6862_7370_6e64; // "chbspnd"

/// One cap that applies to a send, with this month's usage in its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applicable {
    /// `total`, `platform` or `target`.
    pub scope: &'static str,
    pub limit_usd_micros: i64,
    pub used_usd_micros: i64,
}

impl Applicable {
    pub fn fits(&self, amount: i64) -> bool {
        self.used_usd_micros.saturating_add(amount) <= self.limit_usd_micros
    }
}

/// Take the ledger's lock for the rest of the transaction.
pub async fn lock(conn: &mut PgConnection) -> Result<()> {
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", SPEND_LOCK).execute(conn).await?;
    Ok(())
}

/// The caps that apply to a target, each bounded by the instance's ceilings (configuration): the total cap
/// (the runtime row or the total ceiling), the platform cap (the runtime row, or the per-platform ceiling, never
/// above the total ceiling) and the target cap when a runtime row exists.
pub async fn applicable(
    conn: &mut PgConnection,
    platform_definition_id: i64,
    platform_target_id: i64,
    ceiling_total: i64,
    ceiling_per_platform: i64,
) -> Result<Vec<Applicable>> {
    let rows = sqlx::query!(
        r#"WITH month AS (SELECT date_trunc('month', now() AT TIME ZONE 'UTC')::date AS m),
                used AS (
                  SELECT COALESCE(sum(CASE WHEN kind = 'release' THEN -amount_usd_micros ELSE amount_usd_micros END), 0)::bigint AS total,
                         COALESCE(sum(CASE WHEN platform_definition_id = $1
                                           THEN CASE WHEN kind = 'release' THEN -amount_usd_micros ELSE amount_usd_micros END END), 0)::bigint AS platform,
                         COALESCE(sum(CASE WHEN platform_target_id = $2
                                           THEN CASE WHEN kind = 'release' THEN -amount_usd_micros ELSE amount_usd_micros END END), 0)::bigint AS target
                    FROM hub.spend_ledger, month WHERE period_month = month.m)
           SELECT (SELECT monthly_limit_usd_micros FROM hub.spend_caps WHERE scope = 'total') AS total_cap,
                  (SELECT monthly_limit_usd_micros FROM hub.spend_caps WHERE scope = 'platform' AND platform_definition_id = $1) AS platform_cap,
                  (SELECT monthly_limit_usd_micros FROM hub.spend_caps WHERE scope = 'target' AND platform_target_id = $2) AS target_cap,
                  used.total AS "used_total!", used.platform AS "used_platform!", used.target AS "used_target!"
             FROM used"#,
        platform_definition_id,
        platform_target_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let total = rows.total_cap.unwrap_or(ceiling_total).min(ceiling_total);
    let platform = rows.platform_cap.unwrap_or(ceiling_per_platform).min(ceiling_per_platform).min(ceiling_total);
    let mut out = vec![
        Applicable { scope: "total", limit_usd_micros: total, used_usd_micros: rows.used_total },
        Applicable { scope: "platform", limit_usd_micros: platform, used_usd_micros: rows.used_platform },
    ];
    if let Some(t) = rows.target_cap {
        out.push(Applicable { scope: "target", limit_usd_micros: t.min(platform), used_usd_micros: rows.used_target });
    }
    Ok(out)
}

/// Reserve `amount` for an instance in the current month.
pub async fn reserve(conn: &mut PgConnection, instance_id: i64, amount: i64, note: &str) -> Result<()> {
    entry(conn, instance_id, "reserve", amount, note).await
}

/// Record `amount` as spent this month, and release the instance's outstanding reservations.
pub async fn settle(conn: &mut PgConnection, instance_id: i64, amount: i64, note: &str) -> Result<()> {
    if amount > 0 {
        entry(conn, instance_id, "settle", amount, note).await?;
    }
    release(conn, instance_id, note).await?;
    Ok(())
}

async fn entry(conn: &mut PgConnection, instance_id: i64, kind: &str, amount: i64, note: &str) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO hub.spend_ledger (period_month, platform_definition_id, platform_target_id, platform_instance_id, kind,
                                         amount_usd_micros, note)
           SELECT date_trunc('month', now() AT TIME ZONE 'UTC')::date, t.platform_definition_id, t.id, i.id, $2, $3, $4
             FROM hub.platform_instances i JOIN hub.platform_targets t ON t.id = i.platform_target_id
            WHERE i.id = $1"#,
        instance_id,
        kind,
        amount,
        note
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Release whatever the instance still has reserved, in the month of each reservation. Returns the amount.
pub async fn release(conn: &mut PgConnection, instance_id: i64, note: &str) -> Result<i64> {
    let rows = sqlx::query_scalar!(
        r#"INSERT INTO hub.spend_ledger (period_month, platform_definition_id, platform_target_id, platform_instance_id, kind,
                                         amount_usd_micros, note)
           SELECT period_month, platform_definition_id, platform_target_id, platform_instance_id, 'release', outstanding, $2
             FROM (SELECT period_month, platform_definition_id, platform_target_id, platform_instance_id,
                          sum(CASE kind WHEN 'reserve' THEN amount_usd_micros WHEN 'release' THEN -amount_usd_micros ELSE 0 END)::bigint AS outstanding
                     FROM hub.spend_ledger WHERE platform_instance_id = $1
                    GROUP BY period_month, platform_definition_id, platform_target_id, platform_instance_id) o
            WHERE outstanding > 0
           RETURNING amount_usd_micros"#,
        instance_id,
        note
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().sum())
}

/// What the instance has reserved and not yet released.
pub async fn outstanding(conn: &mut PgConnection, instance_id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(CASE kind WHEN 'reserve' THEN amount_usd_micros WHEN 'release' THEN -amount_usd_micros ELSE 0 END), 0)::bigint AS "o!"
             FROM hub.spend_ledger WHERE platform_instance_id = $1"#,
        instance_id
    )
    .fetch_one(conn)
    .await?)
}

/// The posts-per-day limit of a target: the instance ceiling, lowered by any runtime row (total, platform, target).
pub async fn daily_post_limit(
    conn: &mut PgConnection,
    platform_definition_id: i64,
    platform_target_id: i64,
    ceiling: i32,
) -> Result<i32> {
    let lowest = sqlx::query_scalar!(
        r#"SELECT min(daily_post_limit) FROM hub.spend_caps
            WHERE daily_post_limit IS NOT NULL
              AND (scope = 'total' OR (scope = 'platform' AND platform_definition_id = $1)
                   OR (scope = 'target' AND platform_target_id = $2))"#,
        platform_definition_id,
        platform_target_id
    )
    .fetch_one(conn)
    .await?;
    Ok(lowest.map_or(ceiling, |l| l.min(ceiling)))
}
