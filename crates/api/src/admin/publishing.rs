//! Platform instances (operator moves), platform definitions (edit limits, templates, enabled) and targets.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use content_hub_core::ident::is_secret_ref;
use content_hub_core::instance_state::{self, Actor};
use content_hub_core::model::{ExternalStatus, InstanceStatus, Markup, PostingMode};
use content_hub_core::platform::Kind;
use content_hub_core::platforms::check_template;
use content_hub_core::text::intent::fill;
use content_hub_core::text::render::render_source;
use content_hub_store::repo::instances::{Attempt, Instance, InstanceFilter};
use content_hub_store::repo::platforms::{PlatformDefinition, PlatformEdit};
use content_hub_store::repo::targets::{PlatformTarget, TargetFields};
use content_hub_store::repo::{Page, PageRequest, audit, instances, platforms, posts, spend, targets};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::prepare::{over_cap, prepare};
use super::{AdminState, found, https_url, json, parse_enum, text};
use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Body, Query, nullable};

// ── instances ───────────────────────────────────────────────────────────────────────────────────────────

/// The query of `GET /api/v1/instances`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceQuery {
    /// The cursor from the previous page.
    pub cursor: Option<String>,
    /// How many rows to return.
    pub limit: Option<i64>,
    /// Only this status.
    pub status: Option<String>,
    /// Only this target.
    pub platform_target_id: Option<i64>,
    /// Only this post.
    pub post_id: Option<i64>,
}

/// An instance with its attempts.
#[derive(Serialize, utoipa::ToSchema)]
pub struct InstanceDetail {
    /// The instance.
    pub instance: Instance,
    /// Its send attempts, newest first.
    pub attempts: Vec<Attempt>,
    /// The prefilled compose link of a manual platform (when its definition has one).
    pub intent_url: Option<String>,
    /// The moves an operator may make now.
    pub operator_moves: Vec<String>,
}

/// The body of `POST /api/v1/instances/{id}/transition`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceMove {
    /// The new status (omit to change only `external_status`).
    #[serde(default)]
    pub to: Option<String>,
    /// Required with `to: posted`: the post's URL or the platform's id for it.
    #[serde(default)]
    pub url_or_external_id: Option<String>,
    /// With `to: queued` (a retry): when the worker may send it.
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
    /// A third party's progress (KOL, directory); `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub external_status: Option<Option<String>>,
}

/// The prefilled compose link for a manual platform, filled from the platform's template with the instance's payload.
async fn intent_for(c: &mut sqlx::PgConnection, inst: &Instance) -> ApiResult<Option<String>> {
    let Some(target) = targets::get(c, inst.platform_target_id).await? else { return Ok(None) };
    let Some(def) = platforms::get(c, target.platform_definition_id).await? else { return Ok(None) };
    let Some(template) = def.url_templates.get("intent").and_then(Value::as_str) else { return Ok(None) };
    let Some(post) = posts::get(c, inst.post_id).await? else { return Ok(None) };
    let body = inst.override_body.clone().unwrap_or(post.body_markdown);
    let title = inst.override_title.clone().unwrap_or(post.title);
    let markup: Markup = def.markup.parse().unwrap_or(Markup::Plain);
    let plain = render_source(Markup::Plain, &body).payload;
    let rendered = render_source(markup, &body).payload;
    let ext = target.external_ref.clone().unwrap_or_default();
    let values = BTreeMap::from([
        ("text", plain.as_str()),
        ("body", rendered.as_str()),
        ("title", title.as_str()),
        ("url", ""),
        ("subreddit", ext.as_str()),
        ("target_base", ext.as_str()),
    ]);
    Ok(fill(template, &values).ok())
}

/// The statuses an operator may move an instance to from `status`.
fn operator_moves(status: &str) -> Vec<String> {
    let Ok(from) = status.parse::<InstanceStatus>() else { return Vec::new() };
    InstanceStatus::ALL
        .iter()
        .filter(|&&to| instance_state::allowed(Actor::Operator, from, to))
        .map(|t| t.as_str().to_owned())
        .collect()
}

/// `GET /api/v1/instances`: a page of instances.
pub async fn list_instances(
    State(s): State<AdminState>,
    Query(q): Query<InstanceQuery>,
) -> ApiResult<Json<Page<Instance>>> {
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    let f = InstanceFilter { status: q.status, platform_target_id: q.platform_target_id, post_id: q.post_id };
    Ok(Json(instances::list(&mut c, &f, page).await?))
}

/// `GET /api/v1/instances/{id}`: an instance, its attempts, its compose link and the moves open to the operator.
pub async fn get_instance(State(s): State<AdminState>, Path(id): Path<i64>) -> ApiResult<Json<InstanceDetail>> {
    let mut c = s.db.acquire().await?;
    // A missing instance is a 404.
    let inst = found(instances::get(&mut c, id).await?)?;
    Ok(Json(InstanceDetail {
        attempts: instances::attempts(&mut c, id).await?,
        intent_url: intent_for(&mut c, &inst).await?,
        // The UI shows only the buttons that would work.
        operator_moves: operator_moves(&inst.status),
        instance: inst,
    }))
}

/// `POST /api/v1/instances/{id}/transition`: an operator's move (queue, unqueue, retry, cancel, mark posted or failed); the change is audited.
pub async fn transition_instance(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<InstanceMove>,
) -> ApiResult<Json<InstanceDetail>> {
    // A move must change something.
    if b.to.is_none() && b.external_status.is_none() {
        return Err(ApiError::Invalid("give `to`, `external_status`, or both".into()));
    }
    let mut tx = s.db.begin().await?;
    // Lock the row, so the worker and the operator cannot move it at the same time.
    let cur = found(instances::get_for_update(&mut tx, id).await?)?;
    // The instance after the move; the audit entry records the one before.
    let mut inst = cur.clone();
    if let Some(to_s) = &b.to {
        let to: InstanceStatus = parse_enum("to", to_s)?;
        let from: InstanceStatus = parse_enum("status", &cur.status)?;
        // The same state table the tests pin: an operator may only make the moves it allows.
        instance_state::check(Actor::Operator, from, to).map_err(|e| ApiError::Conflict(e.to_string()))?;
        // Marking posted needs the address or id of the post, so there is a record of where it went.
        if to == InstanceStatus::Posted {
            let u = b.url_or_external_id.as_deref().map(str::trim).filter(|u| !u.is_empty());
            let Some(u) = u else { return Err(ApiError::Invalid("posted needs url_or_external_id".into())) };
            text("url_or_external_id", u, 1, 2000)?;
            // An address must be an https URL to a public host; a bare id (a platform's own) is allowed.
            if u.contains("://") {
                https_url("url_or_external_id", u)?;
            }
        }
        // Re-queueing runs the same checks as the first queueing.
        if to == InstanceStatus::Queued {
            requeue(&s, &mut tx, &cur).await?;
        }
        // The database change; it also clears the fields that belong to the old state.
        inst = instances::operator_move(
            &mut tx,
            id,
            to.as_str(),
            b.url_or_external_id.as_deref().map(str::trim),
            b.scheduled_at,
        )
        .await?;
        // The spend ledger follows the move: re-queued → reserved again (above); an unknown send confirmed as
        // posted → settled; anything else → released (nothing is spent).
        match to {
            // Re-queueing reserved the spend already.
            InstanceStatus::Queued => {}
            // An unknown send that the operator confirms as posted did cost money, so it is settled.
            InstanceStatus::Posted if from == InstanceStatus::Unknown => {
                spend::settle(&mut tx, id, cur.cost_estimate_usd_micros, "confirmed posted by an operator").await?;
            }
            _ => {
                // Any other move means nothing was sent by us, so the reservation is released.
                spend::release(&mut tx, id, &format!("operator: {from} -> {to}")).await?;
            }
        }
    }
    // Progress reported by a third party is separate from the delivery status.
    if let Some(ext) = &b.external_status {
        if let Some(e) = ext {
            parse_enum::<ExternalStatus>("external_status", e)?;
        }
        inst = instances::set_external_status(&mut tx, id, ext.as_deref()).await?;
    }
    // Every move is audited with the instance before and after.
    audit::record(&mut tx, &who.subject, "instance.move", "instance", Some(id.to_string()), json(&cur), json(&inst))
        .await?;
    let detail = InstanceDetail {
        attempts: instances::attempts(&mut tx, id).await?,
        intent_url: intent_for(&mut tx, &inst).await?,
        operator_moves: operator_moves(&inst.status),
        instance: inst,
    };
    tx.commit().await?;
    Ok(Json(detail))
}

/// Re-render and re-validate the instance from the current post (it may have been edited) and reserve its spend
/// again: a move into `queued` passes the same checks as the enqueue step.
async fn requeue(s: &AdminState, tx: &mut sqlx::PgConnection, inst: &Instance) -> ApiResult<()> {
    // The post may have been edited since it was first queued.
    let post = found(posts::get(tx, inst.post_id).await?)?;
    // Only a ready post can go out.
    if post.status != "ready" {
        return Err(ApiError::Invalid("only a ready post can be queued".into()));
    }
    let target = found(targets::get(tx, inst.platform_target_id).await?)?;
    let def = found(platforms::get(tx, target.platform_definition_id).await?)?;
    let p = prepare(&post, inst.override_title.as_deref(), inst.override_body.as_deref(), &target, &def)
        // The reason (too long, empty after rendering) is told to the operator.
        .map_err(|m| ApiError::Invalid(format!("cannot queue: {m}")))?;
    // A manual platform is posted by a person, not by the worker.
    if p.mode == PostingMode::Manual {
        return Err(ApiError::Invalid("a manual platform is posted by hand; it cannot be queued".into()));
    }
    // Store the fresh payload: the worker sends these bytes and nothing else.
    instances::set_payload(tx, inst.id, &p.payload, p.cost).await?;
    // Give back the old reservation first, so the cap check sees the right total.
    spend::release(tx, inst.id, "re-queued: replaced by a new reservation").await?;
    // Refuse if the new reservation would break a cap.
    if let Some(why) = over_cap(tx, &target, p.cost, &s.ceilings).await? {
        return Err(ApiError::Invalid(format!("cannot queue: {why}")));
    }
    // A free platform needs no reservation.
    if p.cost > 0 {
        spend::reserve(tx, inst.id, p.cost, "re-queued by an operator").await?;
    }
    Ok(())
}

// ── platform definitions ────────────────────────────────────────────────────────────────────────────────

/// The body of `PATCH /api/v1/platforms/{id}`; a field that is left out is unchanged.
#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlatformPatch {
    /// A new name.
    pub name: Option<String>,
    /// A new main limit; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub primary_limit: Option<Option<i32>>,
    /// New further limits.
    pub secondary_limits: Option<BTreeMap<String, Value>>,
    /// New link templates.
    pub url_templates: Option<BTreeMap<String, String>>,
    /// Switch the platform on or off.
    pub enabled: Option<bool>,
    /// New notes.
    pub notes: Option<String>,
}

/// `GET /api/v1/platforms`: every platform definition.
pub async fn list_platforms(State(s): State<AdminState>) -> ApiResult<Json<Vec<PlatformDefinition>>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(platforms::list(&mut c).await?))
}

/// `PATCH /api/v1/platforms/{id}`: change the editable fields of a definition; the change is audited.
pub async fn patch_platform(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<PlatformPatch>,
) -> ApiResult<Json<PlatformDefinition>> {
    let mut tx = s.db.begin().await?;
    let cur = found(platforms::get_for_update(&mut tx, id).await?)?;
    // Start from the stored values; each field in the patch replaces one.
    let mut e = PlatformEdit::from(&cur);
    if let Some(v) = b.name {
        text("name", &v, 1, 100)?;
        e.name = v;
    }
    if let Some(v) = b.primary_limit {
        // A limit must be positive.
        if v.is_some_and(|l| l <= 0) {
            return Err(ApiError::Invalid("primary_limit must be positive".into()));
        }
        // A limit needs a way to count against it.
        if v.is_some() && cur.counting_method.is_none() {
            return Err(ApiError::Invalid("this platform has no counting method, so it cannot have a limit".into()));
        }
        e.primary_limit = v;
    }
    if let Some(v) = b.secondary_limits {
        for (k, val) in &v {
            // A further limit is a positive number or a non-empty string.
            let ok = val.as_u64().is_some_and(|n| n > 0) || val.as_str().is_some_and(|s| !s.is_empty());
            if !ok {
                return Err(ApiError::Invalid(format!(
                    "secondary limit {k} must be a positive integer or a non-empty string"
                )));
            }
        }
        e.secondary_limits = serde_json::to_value(v).map_err(|_| ApiError::Internal)?;
    }
    if let Some(v) = b.url_templates {
        for (k, t) in &v {
            check_template(t).map_err(|m| ApiError::Invalid(format!("template {k}: {m}")))?;
        }
        e.url_templates = serde_json::to_value(v).map_err(|_| ApiError::Internal)?;
    }
    if let Some(v) = b.enabled {
        e.enabled = v;
    }
    if let Some(v) = b.notes {
        text("notes", &v, 0, 4000)?;
        e.notes = v;
    }
    let d = platforms::update(&mut tx, id, &e).await?;
    audit::record(&mut tx, &who.subject, "platform.update", "platform", Some(id.to_string()), json(&cur), json(&d))
        .await?;
    tx.commit().await?;
    Ok(Json(d))
}

// ── targets ─────────────────────────────────────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/targets`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTarget {
    /// The platform definition.
    pub platform_definition_id: i64,
    /// The name.
    pub name: String,
    /// The destination's reference on the platform.
    #[serde(default)]
    pub external_ref: Option<String>,
    /// The target's own configuration.
    #[serde(default)]
    pub config: Option<BTreeMap<String, Value>>,
    /// The NAME of the environment variable holding the credential (e.g. `DISCORD_ANNOUNCEMENTS_WEBHOOK_URL`).
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// Whether it is a sandbox.
    #[serde(default)]
    pub is_sandbox: bool,
    /// Whether it is switched on.
    #[serde(default)]
    pub enabled: bool,
    /// Real sends (default off: the worker records dry runs).
    #[serde(default)]
    pub live: bool,
}

/// The body of `PATCH /api/v1/targets/{id}`; a field that is left out is unchanged.
#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetPatch {
    /// A new name.
    pub name: Option<String>,
    /// A new reference; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub external_ref: Option<Option<String>>,
    /// A new configuration.
    pub config: Option<BTreeMap<String, Value>>,
    /// A new credential variable name; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub secret_ref: Option<Option<String>>,
    /// Change whether it is a sandbox.
    pub is_sandbox: Option<bool>,
    /// Switch it on or off.
    pub enabled: Option<bool>,
    /// Allow or stop real sends.
    pub live: Option<bool>,
}

/// Check a target's fields: the name, the credential variable's name (never the credential), the reference and the configuration.
async fn check_target(c: &mut sqlx::PgConnection, f: &TargetFields) -> ApiResult<()> {
    // The name is 1 to 200 characters.
    text("name", &f.name, 1, 200)?;
    if let Some(r) = &f.secret_ref
        // The field holds the NAME of a variable, in the shape the database also checks, so a pasted secret is refused.
        && !is_secret_ref(r)
    {
        return Err(ApiError::Invalid(
            "secret_ref is the NAME of an environment variable (A-Z, 0-9, _), never the secret itself".into(),
        ));
    }
    if let Some(x) = &f.external_ref {
        text("external_ref", x, 1, 500)?;
    }
    let def = found(platforms::get(c, f.platform_definition_id).await?)?;
    // Whether a publisher exists for this platform.
    let kind = def.posting_mode.parse().ok().and_then(|m| Kind::for_definition(&def.code, m));
    // A live target on a platform that needs a credential must name one.
    if f.live && kind.is_some_and(Kind::needs_secret) && f.secret_ref.is_none() {
        return Err(ApiError::Invalid(format!("a live {} target needs secret_ref", def.code)));
    }
    // Hosts the worker will call: https and public (the worker checks the resolved address again).
    // A Mastodon target is an instance, so its reference is that instance's address.
    if def.code == "MASTODON" {
        let Some(x) = &f.external_ref else {
            return Err(ApiError::Invalid("MASTODON targets need external_ref: the instance's https base URL".into()));
        };
        https_url("external_ref", x)?;
    }
    // A Bluesky PDS address in the configuration is checked like any host the worker will call.
    if let Some(pds) = f.config.get("pds") {
        let Some(pds) = pds.as_str() else { return Err(ApiError::Invalid("config.pds must be a URL".into())) };
        https_url("config.pds", pds)?;
    }
    // Some platforms' link templates start from the target's own site.
    let uses_base = def
        .url_templates
        .as_object()
        .is_some_and(|m| m.values().any(|v| v.as_str().is_some_and(|t| t.contains("{target_base}"))));
    if uses_base {
        let Some(x) = &f.external_ref else {
            return Err(ApiError::Invalid(format!(
                "{} targets need external_ref: the site's https base URL",
                def.code
            )));
        };
        https_url("external_ref", x)?;
    }
    // The configuration is small by design.
    if serde_json::to_string(&f.config).map(|s| s.len()).unwrap_or(0) > 8192 {
        return Err(ApiError::Invalid("config is too large".into()));
    }
    Ok(())
}

/// `GET /api/v1/targets`: every target.
pub async fn list_targets(State(s): State<AdminState>) -> ApiResult<Json<Vec<PlatformTarget>>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(targets::list(&mut c).await?))
}

/// `POST /api/v1/targets`: create a target; the change is audited.
pub async fn create_target(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<NewTarget>,
) -> ApiResult<(StatusCode, Json<PlatformTarget>)> {
    let f = TargetFields {
        platform_definition_id: b.platform_definition_id,
        name: b.name,
        external_ref: b.external_ref,
        config: serde_json::to_value(b.config.unwrap_or_default()).map_err(|_| ApiError::Internal)?,
        secret_ref: b.secret_ref,
        is_sandbox: b.is_sandbox,
        enabled: b.enabled,
        live: b.live,
    };
    let mut tx = s.db.begin().await?;
    // Validated inside the transaction that inserts, so the check and the insert see the same platform.
    check_target(&mut tx, &f).await?;
    let id = targets::insert(&mut tx, &f).await?;
    // Read back, so the answer and the audit entry show what was stored.
    let t = found(targets::get(&mut tx, id).await?)?;
    // The audit entry has no credential: only the variable's name is stored.
    audit::record(&mut tx, &who.subject, "target.create", "target", Some(id.to_string()), None, json(&t)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(t)))
}

/// `PATCH /api/v1/targets/{id}`: change a target; the change is audited.
pub async fn patch_target(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<TargetPatch>,
) -> ApiResult<Json<PlatformTarget>> {
    let mut tx = s.db.begin().await?;
    let cur = found(targets::get(&mut tx, id).await?)?;
    let mut f = TargetFields::from(&cur);
    if let Some(v) = b.name {
        f.name = v;
    }
    if let Some(v) = b.external_ref {
        f.external_ref = v;
    }
    if let Some(v) = b.config {
        f.config = serde_json::to_value(v).map_err(|_| ApiError::Internal)?;
    }
    if let Some(v) = b.secret_ref {
        f.secret_ref = v;
    }
    if let Some(v) = b.is_sandbox {
        f.is_sandbox = v;
    }
    if let Some(v) = b.enabled {
        f.enabled = v;
    }
    if let Some(v) = b.live {
        f.live = v;
    }
    check_target(&mut tx, &f).await?;
    targets::update(&mut tx, id, &f).await?;
    let t = found(targets::get(&mut tx, id).await?)?;
    audit::record(&mut tx, &who.subject, "target.update", "target", Some(id.to_string()), json(&cur), json(&t)).await?;
    tx.commit().await?;
    Ok(Json(t))
}
