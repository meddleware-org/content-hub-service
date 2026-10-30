//! Budgets (runtime caps, bounded by the instance ceilings), the publishing stop switch, the schedule, stats and the
//! audit log.

use axum::extract::State;
use axum::{Extension, Json};
use chrono::{Duration, Utc};
use content_hub_core::model::SpendScope;
use content_hub_store::repo::audit::AuditEntry;
use content_hub_store::repo::budgets::{SpendCap, Usage};
use content_hub_store::repo::instances::Instance;
use content_hub_store::repo::public::Audience;
use content_hub_store::repo::stats::Stats;
use content_hub_store::repo::tasks::{Task, TaskFilter};
use content_hub_store::repo::{Page, PageRequest, audit, budgets, instances, settings, stats as stats_repo, tasks};
use serde::{Deserialize, Serialize};

use super::{AdminState, Ceilings, json, parse_enum};
use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Body, Query};

#[derive(Serialize, utoipa::ToSchema)]
pub struct Budgets {
    /// The instance's ceilings (configuration; the runtime caps cannot exceed them).
    pub ceilings: Ceilings,
    pub caps: Vec<SpendCap>,
    /// This calendar month's (UTC) usage per platform.
    pub usage: Vec<Usage>,
}

/// The worker's stop switch: while `paused`, nothing is claimed (in-flight sends finish).
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Publishing {
    pub paused: bool,
}

pub async fn get_publishing(State(s): State<AdminState>) -> ApiResult<Json<Publishing>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(Publishing { paused: settings::paused(&mut c).await? }))
}

pub async fn put_publishing(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<Publishing>,
) -> ApiResult<Json<Publishing>> {
    let mut tx = s.db.begin().await?;
    let before = settings::paused(&mut tx).await?;
    settings::set(&mut tx, settings::PUBLISHING_PAUSED, &serde_json::json!(b.paused), &who.subject).await?;
    audit::record(
        &mut tx,
        &who.subject,
        "publishing.update",
        "setting",
        Some(settings::PUBLISHING_PAUSED.to_owned()),
        Some(serde_json::json!({ "paused": before })),
        Some(serde_json::json!({ "paused": b.paused })),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(b))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CapInput {
    pub scope: String,
    #[serde(default)]
    pub platform_definition_id: Option<i64>,
    #[serde(default)]
    pub platform_target_id: Option<i64>,
    pub monthly_limit_usd_micros: i64,
    #[serde(default)]
    pub daily_post_limit: Option<i32>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetsInput {
    pub caps: Vec<CapInput>,
}

pub async fn get_budgets(State(s): State<AdminState>) -> ApiResult<Json<Budgets>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(Budgets {
        ceilings: s.ceilings,
        caps: budgets::caps(&mut c).await?,
        usage: budgets::usage_this_month(&mut c).await?,
    }))
}

/// Set caps. Each must be at or below the ceiling of its layer; a target's cap at or below its platform's.
pub async fn put_budgets(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<BudgetsInput>,
) -> ApiResult<Json<Budgets>> {
    if b.caps.len() > 200 {
        return Err(ApiError::Invalid("at most 200 caps per request".into()));
    }
    let mut tx = s.db.begin().await?;
    let before = budgets::caps(&mut tx).await?;
    for cap in &b.caps {
        let scope: SpendScope = parse_enum("scope", &cap.scope)?;
        if cap.monthly_limit_usd_micros < 0 || cap.daily_post_limit.is_some_and(|d| d <= 0) {
            return Err(ApiError::Invalid("limits must be non-negative (daily_post_limit positive)".into()));
        }
        let ceiling = match scope {
            SpendScope::Total => s.ceilings.total_monthly_usd_micros,
            SpendScope::Platform | SpendScope::Target => {
                s.ceilings.per_platform_monthly_usd_micros.min(s.ceilings.total_monthly_usd_micros)
            }
        };
        if cap.monthly_limit_usd_micros > ceiling {
            return Err(ApiError::Invalid(format!(
                "{} cap {} exceeds the instance ceiling {} (raise it in the deployment's configuration first)",
                scope, cap.monthly_limit_usd_micros, ceiling
            )));
        }
        let (def, target) = match scope {
            SpendScope::Total => (None, None),
            SpendScope::Platform => (
                Some(
                    cap.platform_definition_id
                        .ok_or_else(|| ApiError::Invalid("platform caps need platform_definition_id".into()))?,
                ),
                None,
            ),
            SpendScope::Target => (
                None,
                Some(
                    cap.platform_target_id
                        .ok_or_else(|| ApiError::Invalid("target caps need platform_target_id".into()))?,
                ),
            ),
        };
        budgets::set_cap(
            &mut tx,
            scope.as_str(),
            def,
            target,
            cap.monthly_limit_usd_micros,
            cap.daily_post_limit,
            &who.subject,
        )
        .await?;
    }
    let after = budgets::caps(&mut tx).await?;
    audit::record(&mut tx, &who.subject, "budgets.update", "budgets", None, json(&before), json(&after)).await?;
    let usage = budgets::usage_this_month(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(Budgets { ceilings: s.ceilings, caps: after, usage }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleQuery {
    /// How far ahead, in days (1 to 90; default 14).
    pub days: Option<i64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Schedule {
    pub instances: Vec<Instance>,
    pub reviews_due: Vec<Task>,
}

pub async fn schedule(State(s): State<AdminState>, Query(q): Query<ScheduleQuery>) -> ApiResult<Json<Schedule>> {
    let until = Utc::now() + Duration::days(q.days.unwrap_or(14).clamp(1, 90));
    let mut c = s.db.acquire().await?;
    let reviews_due = tasks::list(
        &mut c,
        &TaskFilter { review_due_before: Some(until), ..Default::default() },
        PageRequest { before: None, limit: 100 },
    )
    .await?
    .items;
    Ok(Json(Schedule { instances: instances::upcoming(&mut c, until).await?, reviews_due }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewQuery {
    /// `human` (the people's roadmap) or `bot` (the machine roadmap).
    pub audience: String,
}

/// The public roadmap as the public process would serve it to an audience (same code, same views).
pub async fn preview_roadmap(
    State(s): State<AdminState>,
    Query(q): Query<PreviewQuery>,
) -> ApiResult<Json<crate::public::Roadmap>> {
    let audience = match q.audience.as_str() {
        "human" => Audience::Human,
        "bot" => Audience::Bot,
        _ => return Err(ApiError::Invalid("audience must be human or bot".into())),
    };
    let mut c = s.db.acquire().await?;
    Ok(Json(crate::public::build_roadmap(&mut c, audience).await?))
}

pub async fn stats(State(s): State<AdminState>) -> ApiResult<Json<Stats>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(stats_repo::stats(&mut c).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
}

pub async fn audit(State(s): State<AdminState>, Query(q): Query<AuditQuery>) -> ApiResult<Json<Page<AuditEntry>>> {
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    Ok(Json(audit::list(&mut c, q.entity_type.as_deref(), q.entity_id.as_deref(), page).await?))
}
