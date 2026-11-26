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

/// The budgets as `GET /api/v1/budgets` returns them.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Budgets {
    /// The instance's ceilings (configuration; the runtime caps cannot exceed them).
    pub ceilings: Ceilings,
    /// The runtime caps.
    pub caps: Vec<SpendCap>,
    /// This calendar month's (UTC) usage per platform.
    pub usage: Vec<Usage>,
}

/// The worker's stop switch: while `paused`, nothing is claimed (in-flight sends finish).
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Publishing {
    /// Whether the worker is stopped.
    pub paused: bool,
}

/// `GET /api/v1/publishing`: whether publishing is paused.
pub async fn get_publishing(State(s): State<AdminState>) -> ApiResult<Json<Publishing>> {
    // A plain read: no transaction needed.
    let mut c = s.db.acquire().await?;
    Ok(Json(Publishing { paused: settings::paused(&mut c).await? }))
}

/// `PUT /api/v1/publishing`: pause or resume publishing; the change is audited.
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

/// One cap as the operator sets it.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CapInput {
    /// `total`, `platform` or `target`.
    pub scope: String,
    /// The platform, for a platform cap.
    #[serde(default)]
    pub platform_definition_id: Option<i64>,
    /// The target, for a target cap.
    #[serde(default)]
    pub platform_target_id: Option<i64>,
    /// The monthly limit, in millionths of a US dollar.
    pub monthly_limit_usd_micros: i64,
    /// The daily post limit, if any.
    #[serde(default)]
    pub daily_post_limit: Option<i32>,
}

/// The caps to set (each replaces the cap of its scope).
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetsInput {
    /// The caps.
    pub caps: Vec<CapInput>,
}

/// `GET /api/v1/budgets`: the ceilings, the caps and this month's usage.
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
    // A bound on one request.
    if b.caps.len() > 200 {
        return Err(ApiError::Invalid("at most 200 caps per request".into()));
    }
    let mut tx = s.db.begin().await?;
    // The caps before, for the audit entry.
    let before = budgets::caps(&mut tx).await?;
    for cap in &b.caps {
        // Only the three scopes the database knows.
        let scope: SpendScope = parse_enum("scope", &cap.scope)?;
        // A limit is not negative, and a daily limit is positive.
        if cap.monthly_limit_usd_micros < 0 || cap.daily_post_limit.is_some_and(|d| d <= 0) {
            return Err(ApiError::Invalid("limits must be non-negative (daily_post_limit positive)".into()));
        }
        // The ceiling comes from the deployment's configuration, never from data: a runtime cap can only lower it.
        let ceiling = match scope {
            SpendScope::Total => s.ceilings.total_monthly_usd_micros,
            SpendScope::Platform | SpendScope::Target => {
                // No platform or target may be allowed more than the total ceiling.
                s.ceilings.per_platform_monthly_usd_micros.min(s.ceilings.total_monthly_usd_micros)
            }
        };
        // Over the ceiling: raising it is a deployment change, not an API call.
        if cap.monthly_limit_usd_micros > ceiling {
            return Err(ApiError::Invalid(format!(
                "{} cap {} exceeds the instance ceiling {} (raise it in the deployment's configuration first)",
                scope, cap.monthly_limit_usd_micros, ceiling
            )));
        }
        // The columns a cap needs depend on its scope, and the database refuses any other combination.
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
        // Insert or replace the cap of this scope, platform and target.
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
    // The whole set of caps before and after is audited.
    audit::record(&mut tx, &who.subject, "budgets.update", "budgets", None, json(&before), json(&after)).await?;
    let usage = budgets::usage_this_month(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(Budgets { ceilings: s.ceilings, caps: after, usage }))
}

/// The query of `GET /api/v1/schedule`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleQuery {
    /// How far ahead, in days (1 to 90; default 14).
    pub days: Option<i64>,
}

/// What is coming up.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Schedule {
    /// Instances due in the window.
    pub instances: Vec<Instance>,
    /// Tasks whose review falls in the window.
    pub reviews_due: Vec<Task>,
}

/// `GET /api/v1/schedule`: instances and reviews due in the next days.
pub async fn schedule(State(s): State<AdminState>, Query(q): Query<ScheduleQuery>) -> ApiResult<Json<Schedule>> {
    // The window is 1 to 90 days, 14 by default.
    let until = Utc::now() + Duration::days(q.days.unwrap_or(14).clamp(1, 90));
    let mut c = s.db.acquire().await?;
    let reviews_due = tasks::list(
        &mut c,
        &TaskFilter { review_due_before: Some(until), ..Default::default() },
        // At most the first hundred reviews.
        PageRequest { before: None, limit: 100 },
    )
    .await?
    .items;
    Ok(Json(Schedule { instances: instances::upcoming(&mut c, until).await?, reviews_due }))
}

/// The query of `GET /api/v1/preview/roadmap`.
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

/// `GET /api/v1/stats`: the dashboard counts.
pub async fn stats(State(s): State<AdminState>) -> ApiResult<Json<Stats>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(stats_repo::stats(&mut c).await?))
}

/// The query of `GET /api/v1/audit`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    /// The cursor from the previous page.
    pub cursor: Option<String>,
    /// How many rows to return.
    pub limit: Option<i64>,
    /// Only this kind of entity.
    pub entity_type: Option<String>,
    /// Only this entity.
    pub entity_id: Option<String>,
}

/// `GET /api/v1/audit`: the audit log, newest first.
pub async fn audit(State(s): State<AdminState>, Query(q): Query<AuditQuery>) -> ApiResult<Json<Page<AuditEntry>>> {
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    Ok(Json(audit::list(&mut c, q.entity_type.as_deref(), q.entity_id.as_deref(), page).await?))
}
