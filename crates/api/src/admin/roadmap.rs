//! Phases and items.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use content_hub_core::model::{ItemStatus, ItemType};
use content_hub_store::repo::items::{Item, ItemFields, ItemFilter};
use content_hub_store::repo::phases::{Phase, PhaseFields};
use content_hub_store::repo::posts::{Post, PostFilter};
use content_hub_store::repo::tasks::{Task, TaskFilter};
use content_hub_store::repo::{Page, PageRequest, audit, items, phases, posts, tasks};
use serde::{Deserialize, Serialize};

use super::{AdminState, found, json, key, parse_enum, text};
use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Body, Query, nullable};

// ── phases ──────────────────────────────────────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/phases`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPhase {
    /// The stable key.
    pub key: String,
    /// Its position in the roadmap.
    pub sort_order: i32,
    /// The name the operator sees.
    pub internal_name: String,
    /// The description the operator sees.
    #[serde(default)]
    pub internal_description: String,
    /// The name the public views show.
    pub public_name: String,
    /// The description the public views show.
    #[serde(default)]
    pub public_description: String,
    /// When the phase starts.
    #[serde(default)]
    pub window_start: Option<DateTime<Utc>>,
    /// When the phase ends.
    #[serde(default)]
    pub window_end: Option<DateTime<Utc>>,
    /// Whether people reading in a browser see it.
    #[serde(default)]
    pub visible_human: bool,
    /// Whether the feeds for bots carry it.
    #[serde(default)]
    pub visible_bot: bool,
}

/// The body of `PATCH /api/v1/phases/{id}`; a field that is left out is unchanged.
#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PhasePatch {
    /// A new key.
    pub key: Option<String>,
    /// A new position.
    pub sort_order: Option<i32>,
    /// A new internal name.
    pub internal_name: Option<String>,
    /// A new internal description.
    pub internal_description: Option<String>,
    /// A new public name.
    pub public_name: Option<String>,
    /// A new public description.
    pub public_description: Option<String>,
    /// A new start; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub window_start: Option<Option<DateTime<Utc>>>,
    /// A new end; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub window_end: Option<Option<DateTime<Utc>>>,
    /// Change who sees it in a browser.
    pub visible_human: Option<bool>,
    /// Change whether bot feeds carry it.
    pub visible_bot: Option<bool>,
}

/// Check a phase's fields against the database's limits.
fn check_phase(f: &PhaseFields) -> ApiResult<()> {
    key("key", &f.key)?;
    if f.sort_order < 1 {
        return Err(ApiError::Invalid("sort_order must be at least 1".into()));
    }
    text("internal_name", &f.internal_name, 1, 200)?;
    text("public_name", &f.public_name, 1, 200)?;
    text("internal_description", &f.internal_description, 0, 4000)?;
    text("public_description", &f.public_description, 0, 4000)?;
    if let (Some(a), Some(b)) = (f.window_start, f.window_end)
        && b < a
    {
        return Err(ApiError::Invalid("window_end is before window_start".into()));
    }
    Ok(())
}

/// `GET /api/v1/phases`: every phase in roadmap order.
pub async fn list_phases(State(s): State<AdminState>) -> ApiResult<Json<Vec<Phase>>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(phases::list(&mut c).await?))
}

/// `GET /api/v1/phases/{id}`: one phase.
pub async fn get_phase(State(s): State<AdminState>, Path(id): Path<i64>) -> ApiResult<Json<Phase>> {
    let mut c = s.db.acquire().await?;
    Ok(Json(found(phases::get(&mut c, id).await?)?))
}

/// `POST /api/v1/phases`: create a phase; the change is audited.
pub async fn create_phase(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<NewPhase>,
) -> ApiResult<(StatusCode, Json<Phase>)> {
    let f = PhaseFields {
        key: b.key,
        sort_order: b.sort_order,
        internal_name: b.internal_name,
        internal_description: b.internal_description,
        public_name: b.public_name,
        public_description: b.public_description,
        window_start: b.window_start,
        window_end: b.window_end,
        visible_human: b.visible_human,
        visible_bot: b.visible_bot,
    };
    check_phase(&f)?;
    let mut tx = s.db.begin().await?;
    let p = phases::insert(&mut tx, &f).await?;
    audit::record(&mut tx, &who.subject, "phase.create", "phase", Some(p.id.to_string()), None, json(&p)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(p)))
}

/// `PATCH /api/v1/phases/{id}`: change a phase; the change is audited.
pub async fn patch_phase(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<PhasePatch>,
) -> ApiResult<Json<Phase>> {
    let mut tx = s.db.begin().await?;
    let cur = found(phases::get_for_update(&mut tx, id).await?)?;
    let mut f = PhaseFields::from(&cur);
    if let Some(v) = b.key {
        f.key = v;
    }
    if let Some(v) = b.sort_order {
        f.sort_order = v;
    }
    if let Some(v) = b.internal_name {
        f.internal_name = v;
    }
    if let Some(v) = b.internal_description {
        f.internal_description = v;
    }
    if let Some(v) = b.public_name {
        f.public_name = v;
    }
    if let Some(v) = b.public_description {
        f.public_description = v;
    }
    if let Some(v) = b.window_start {
        f.window_start = v;
    }
    if let Some(v) = b.window_end {
        f.window_end = v;
    }
    if let Some(v) = b.visible_human {
        f.visible_human = v;
    }
    if let Some(v) = b.visible_bot {
        f.visible_bot = v;
    }
    check_phase(&f)?;
    let p = phases::update(&mut tx, id, &f).await?;
    audit::record(&mut tx, &who.subject, "phase.update", "phase", Some(id.to_string()), json(&cur), json(&p)).await?;
    tx.commit().await?;
    Ok(Json(p))
}

// ── items ───────────────────────────────────────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/items`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewItem {
    /// The phase.
    pub phase_id: i64,
    /// The stable key.
    pub key: String,
    /// What kind of item it is.
    pub item_type: String,
    /// The title the operator sees.
    pub internal_title: String,
    /// The description the operator sees.
    #[serde(default)]
    pub internal_description: String,
    /// The title the public views show.
    #[serde(default)]
    pub public_title: Option<String>,
    /// The description the public views show.
    #[serde(default)]
    pub public_description: Option<String>,
    /// Whether people reading in a browser see it.
    #[serde(default)]
    pub visible_human: bool,
    /// Whether the feeds for bots carry it.
    #[serde(default)]
    pub visible_bot: bool,
    /// The starting status (default `planned`).
    #[serde(default)]
    pub status: Option<String>,
    /// When it is planned to start.
    #[serde(default)]
    pub planned_at: Option<DateTime<Utc>>,
    /// When it is due.
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// The date the public sees.
    #[serde(default)]
    pub public_due_at: Option<DateTime<Utc>>,
}

/// The body of `PATCH /api/v1/items/{id}`; a field that is left out is unchanged.
#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemPatch {
    /// Move it to another phase.
    pub phase_id: Option<i64>,
    /// A new key.
    pub key: Option<String>,
    /// A new kind.
    pub item_type: Option<String>,
    /// A new internal title.
    pub internal_title: Option<String>,
    /// A new internal description.
    pub internal_description: Option<String>,
    /// A new public title; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub public_title: Option<Option<String>>,
    /// A new public description; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub public_description: Option<Option<String>>,
    /// Change who sees it in a browser.
    pub visible_human: Option<bool>,
    /// Change whether bot feeds carry it.
    pub visible_bot: Option<bool>,
    /// A new status.
    pub status: Option<String>,
    /// A new planned date; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub planned_at: Option<Option<DateTime<Utc>>>,
    /// A new due date; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub due_at: Option<Option<DateTime<Utc>>>,
    /// A new public date; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub public_due_at: Option<Option<DateTime<Utc>>>,
}

/// The query of `GET /api/v1/items`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemQuery {
    /// The cursor from the previous page.
    pub cursor: Option<String>,
    /// How many rows to return.
    pub limit: Option<i64>,
    /// Only this phase.
    pub phase_id: Option<i64>,
    /// Only this status.
    pub status: Option<String>,
    /// Only this kind.
    pub item_type: Option<String>,
}

/// An item with its tasks and posts.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ItemDetail {
    /// The item.
    pub item: Item,
    /// Its tasks.
    pub tasks: Vec<Task>,
    /// Its posts.
    pub posts: Vec<Post>,
}

/// Check an item's fields against the database's limits and the rule that a visible item has public wording.
fn check_item(f: &ItemFields) -> ApiResult<()> {
    key("key", &f.key)?;
    parse_enum::<ItemType>("item_type", &f.item_type)?;
    parse_enum::<ItemStatus>("status", &f.status)?;
    text("internal_title", &f.internal_title, 1, 200)?;
    text("internal_description", &f.internal_description, 0, 4000)?;
    if let Some(t) = &f.public_title {
        text("public_title", t, 1, 200)?;
    }
    if let Some(d) = &f.public_description {
        text("public_description", d, 0, 4000)?;
    }
    if (f.visible_human || f.visible_bot) && (f.public_title.is_none() || f.public_description.is_none()) {
        return Err(ApiError::Invalid("a visible item needs public_title and public_description".into()));
    }
    Ok(())
}

/// `GET /api/v1/items`: a page of items.
pub async fn list_items(State(s): State<AdminState>, Query(q): Query<ItemQuery>) -> ApiResult<Json<Page<Item>>> {
    // A bad cursor is a 400.
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    Ok(Json(
        items::list(&mut c, &ItemFilter { phase_id: q.phase_id, status: q.status, item_type: q.item_type }, page)
            .await?,
    ))
}

/// `GET /api/v1/items/{id}`: an item with its tasks and posts.
pub async fn get_item(State(s): State<AdminState>, Path(id): Path<i64>) -> ApiResult<Json<ItemDetail>> {
    let mut c = s.db.acquire().await?;
    let item = found(items::get(&mut c, id).await?)?;
    let all = PageRequest { before: None, limit: 100 };
    let t = tasks::list(&mut c, &TaskFilter { roadmap_item_id: Some(id), ..Default::default() }, all).await?.items;
    let p = posts::list(&mut c, &PostFilter { roadmap_item_id: Some(id), ..Default::default() }, all).await?.items;
    Ok(Json(ItemDetail { item, tasks: t, posts: p }))
}

/// `POST /api/v1/items`: create an item; the change is audited.
pub async fn create_item(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<NewItem>,
) -> ApiResult<(StatusCode, Json<Item>)> {
    let f = ItemFields {
        phase_id: b.phase_id,
        key: b.key,
        item_type: b.item_type,
        internal_title: b.internal_title,
        internal_description: b.internal_description,
        public_title: b.public_title,
        public_description: b.public_description,
        visible_human: b.visible_human,
        visible_bot: b.visible_bot,
        status: b.status.unwrap_or_else(|| "planned".into()),
        planned_at: b.planned_at,
        due_at: b.due_at,
        public_due_at: b.public_due_at,
    };
    // Validated before the transaction starts, so a bad request costs no lock.
    check_item(&f)?;
    let mut tx = s.db.begin().await?;
    let i = items::insert(&mut tx, &f).await?;
    audit::record(&mut tx, &who.subject, "item.create", "item", Some(i.id.to_string()), None, json(&i)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(i)))
}

/// `PATCH /api/v1/items/{id}`: change an item; the change is audited.
pub async fn patch_item(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<ItemPatch>,
) -> ApiResult<Json<Item>> {
    let mut tx = s.db.begin().await?;
    let cur = found(items::get_for_update(&mut tx, id).await?)?;
    let mut f = ItemFields::from(&cur);
    macro_rules! set {
        ($($field:ident),*) => { $( if let Some(v) = b.$field { f.$field = v; } )* };
    }
    set!(
        phase_id,
        key,
        item_type,
        internal_title,
        internal_description,
        public_title,
        public_description,
        visible_human,
        visible_bot,
        status,
        planned_at,
        due_at,
        public_due_at
    );
    check_item(&f)?;
    let i = items::update(&mut tx, id, &f).await?;
    audit::record(&mut tx, &who.subject, "item.update", "item", Some(id.to_string()), json(&cur), json(&i)).await?;
    tx.commit().await?;
    Ok(Json(i))
}
