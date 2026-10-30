//! Spend caps (runtime layers 3 and 4) and this month's usage from the spend ledger.
//!
//! Usage counts reservations and settlements and subtracts releases: the publishing worker settles the actual
//! cost and releases the reservation it replaces (ARCHITECTURE.md §2 "Cost caps").

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SpendCap {
    pub id: i64,
    pub scope: String,
    pub platform_definition_id: Option<i64>,
    pub platform_target_id: Option<i64>,
    pub monthly_limit_usd_micros: i64,
    pub daily_post_limit: Option<i32>,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Usage {
    pub platform_definition_id: i64,
    pub used_usd_micros: i64,
}

pub async fn caps(conn: &mut PgConnection) -> Result<Vec<SpendCap>> {
    Ok(sqlx::query_as!(
        SpendCap,
        "SELECT * FROM hub.spend_caps ORDER BY scope, platform_definition_id, platform_target_id"
    )
    .fetch_all(conn)
    .await?)
}

/// Set one cap (insert or replace by its scope and subject).
pub async fn set_cap(
    conn: &mut PgConnection,
    scope: &str,
    platform_definition_id: Option<i64>,
    platform_target_id: Option<i64>,
    monthly_limit_usd_micros: i64,
    daily_post_limit: Option<i32>,
    by: &str,
) -> Result<SpendCap> {
    Ok(sqlx::query_as!(
        SpendCap,
        r#"INSERT INTO hub.spend_caps (scope, platform_definition_id, platform_target_id, monthly_limit_usd_micros, daily_post_limit, updated_by)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (scope, platform_definition_id, platform_target_id) DO UPDATE
              SET monthly_limit_usd_micros = EXCLUDED.monthly_limit_usd_micros, daily_post_limit = EXCLUDED.daily_post_limit,
                  updated_by = EXCLUDED.updated_by, updated_at = now()
           RETURNING *"#,
        scope,
        platform_definition_id,
        platform_target_id,
        monthly_limit_usd_micros,
        daily_post_limit,
        by
    )
    .fetch_one(conn)
    .await?)
}

/// This calendar month's (UTC) usage per platform.
pub async fn usage_this_month(conn: &mut PgConnection) -> Result<Vec<Usage>> {
    Ok(sqlx::query_as!(
        Usage,
        r#"SELECT platform_definition_id,
                  COALESCE(sum(CASE WHEN kind = 'release' THEN -amount_usd_micros ELSE amount_usd_micros END), 0)::bigint AS "used_usd_micros!"
             FROM hub.spend_ledger
            WHERE period_month = date_trunc('month', now() AT TIME ZONE 'UTC')::date
            GROUP BY platform_definition_id ORDER BY platform_definition_id"#
    )
    .fetch_all(conn)
    .await?)
}
