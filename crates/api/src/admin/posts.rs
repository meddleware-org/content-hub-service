//! Posts and the enqueue step (ARCHITECTURE.md §10, step 1): render and validate every target's payload
//! (`prepare`), then create the instances in one transaction. Automated platforms are `queued` for the worker with
//! their spend reserved; one that would pass a cap is `manual_required` instead (nothing is spent, a person can post
//! it by hand); manual platforms are `manual_required` with their intent link.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use content_hub_core::model::{PostStatus, PostingMode};
use content_hub_store::repo::instances::{Instance, NewInstance};
use content_hub_store::repo::posts::{Post, PostFields, PostFilter, PostRevision};
use content_hub_store::repo::{Page, PageRequest, audit, instances, platforms, posts, spend, targets};
use serde::{Deserialize, Serialize};

use super::prepare::{over_cap, prepare};
use super::{AdminState, found, json, normalise_body, parse_enum, text};
use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Body, Query, nullable};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPost {
    pub title: String,
    pub body_markdown: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub roadmap_item_id: Option<i64>,
    #[serde(default)]
    pub task_id: Option<i64>,
}

#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PostPatch {
    pub title: Option<String>,
    pub body_markdown: Option<String>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub roadmap_item_id: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    pub task_id: Option<Option<i64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    pub status: Option<String>,
    pub roadmap_item_id: Option<i64>,
    pub task_id: Option<i64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PostDetail {
    pub post: Post,
    pub revisions: Vec<PostRevision>,
    pub instances: Vec<Instance>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnqueueTarget {
    pub target_id: i64,
    #[serde(default)]
    pub override_title: Option<String>,
    #[serde(default)]
    pub override_body: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Enqueue {
    pub targets: Vec<EnqueueTarget>,
    /// When automated sends may start (default: now).
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// Why one target was refused.
#[derive(Serialize, utoipa::ToSchema)]
pub struct TargetProblem {
    pub target_id: i64,
    pub problem: String,
}

fn check_post(f: &PostFields) -> ApiResult<()> {
    text("title", &f.title, 1, 300)?;
    text("body_markdown", &f.body_markdown, 0, 100_000)?;
    parse_enum::<PostStatus>("status", &f.status)?;
    if f.tags.len() > 50 || f.categories.len() > 20 {
        return Err(ApiError::Invalid("at most 50 tags and 20 categories".into()));
    }
    for t in f.tags.iter().chain(&f.categories) {
        text("tag or category", t, 1, 50)?;
    }
    Ok(())
}

pub async fn list(State(s): State<AdminState>, Query(q): Query<PostQuery>) -> ApiResult<Json<Page<Post>>> {
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    let f = PostFilter { status: q.status, roadmap_item_id: q.roadmap_item_id, task_id: q.task_id };
    Ok(Json(posts::list(&mut c, &f, page).await?))
}

pub async fn get_one(State(s): State<AdminState>, Path(id): Path<i64>) -> ApiResult<Json<PostDetail>> {
    let mut c = s.db.acquire().await?;
    let post = found(posts::get(&mut c, id).await?)?;
    Ok(Json(PostDetail {
        revisions: posts::revisions(&mut c, id).await?,
        instances: instances::for_post(&mut c, id).await?,
        post,
    }))
}

pub async fn create(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<NewPost>,
) -> ApiResult<(StatusCode, Json<Post>)> {
    let f = PostFields {
        title: b.title,
        body_markdown: normalise_body(&b.body_markdown),
        status: b.status.unwrap_or_else(|| "draft".into()),
        tags: b.tags,
        categories: b.categories,
        roadmap_item_id: b.roadmap_item_id,
        task_id: b.task_id,
    };
    check_post(&f)?;
    let mut tx = s.db.begin().await?;
    let p = posts::insert(&mut tx, &f, &who.subject).await?;
    audit::record(&mut tx, &who.subject, "post.create", "post", Some(p.id.to_string()), None, json(&p)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(p)))
}

pub async fn patch(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<PostPatch>,
) -> ApiResult<Json<Post>> {
    let mut tx = s.db.begin().await?;
    let cur = found(posts::get_for_update(&mut tx, id).await?)?;
    let mut f = PostFields::from(&cur);
    if let Some(v) = b.title {
        f.title = v;
    }
    if let Some(v) = b.body_markdown {
        f.body_markdown = normalise_body(&v);
    }
    if let Some(v) = b.status {
        f.status = v;
    }
    if let Some(v) = b.tags {
        f.tags = v;
    }
    if let Some(v) = b.categories {
        f.categories = v;
    }
    if let Some(v) = b.roadmap_item_id {
        f.roadmap_item_id = v;
    }
    if let Some(v) = b.task_id {
        f.task_id = v;
    }
    check_post(&f)?;
    let p = posts::update(&mut tx, &cur, &f, &who.subject).await?;
    audit::record(&mut tx, &who.subject, "post.update", "post", Some(id.to_string()), json(&cur), json(&p)).await?;
    tx.commit().await?;
    Ok(Json(p))
}

#[expect(clippy::result_large_err, reason = "an axum handler: the error is the response itself, built once")]
pub async fn enqueue(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<Enqueue>,
) -> Result<(StatusCode, Json<Vec<Instance>>), axum::response::Response> {
    use axum::response::IntoResponse;
    let fail = |e: ApiError| e.into_response();
    if b.targets.is_empty() || b.targets.len() > 50 {
        return Err(fail(ApiError::Invalid("give 1 to 50 targets".into())));
    }
    let mut tx = s.db.begin().await.map_err(|e| fail(e.into()))?;
    let post = found(posts::get_for_update(&mut tx, id).await.map_err(|e| fail(e.into()))?).map_err(fail)?;
    if post.status != "ready" {
        return Err(fail(ApiError::Invalid("only a ready post can be enqueued".into())));
    }
    let mut problems: Vec<TargetProblem> = Vec::new();
    let mut created = Vec::new();
    for t in &b.targets {
        let problem = |p: String| TargetProblem { target_id: t.target_id, problem: p };
        for (field, v, max) in
            [("override_title", &t.override_title, 300), ("override_body", &t.override_body, 100_000)]
        {
            if let Some(v) = v
                && (v.is_empty() || v.chars().count() > max)
            {
                problems.push(problem(format!("{field} must be 1 to {max} characters")));
            }
        }
        let Some(target) = targets::get(&mut tx, t.target_id).await.map_err(|e| fail(e.into()))? else {
            problems.push(problem("no such target".into()));
            continue;
        };
        let def = found(platforms::get(&mut tx, target.platform_definition_id).await.map_err(|e| fail(e.into()))?)
            .map_err(fail)?;
        let p = match prepare(&post, t.override_title.as_deref(), t.override_body.as_deref(), &target, &def) {
            Ok(p) => p,
            Err(m) => {
                problems.push(problem(m));
                continue;
            }
        };
        if !problems.is_empty() {
            continue; // nothing will be written; keep collecting problems only
        }
        let manual = p.mode == PostingMode::Manual;
        let queued_at = b.scheduled_at.unwrap_or_else(Utc::now);
        let new = |status, last_error| NewInstance {
            post_id: id,
            platform_target_id: t.target_id,
            override_title: t.override_title.as_deref(),
            override_body: t.override_body.as_deref(),
            status,
            scheduled_at: (status == "queued").then_some(queued_at),
            cost_estimate_usd_micros: p.cost,
            payload: &p.payload,
            last_error,
        };
        let conflict = |e: content_hub_store::StoreError| {
            let api: ApiError = e.into();
            match api {
                ApiError::Conflict(_) => {
                    fail(ApiError::Conflict(format!("target {} already has a live instance of this post", t.target_id)))
                }
                other => fail(other),
            }
        };
        let over = if manual {
            None
        } else {
            over_cap(&mut tx, &target, p.cost, &s.ceilings).await.map_err(|e| fail(e.into()))?
        };
        // Over a cap: nothing is reserved or spent; a person can still post it by hand (the intent link).
        let status = if manual || over.is_some() { "manual_required" } else { "queued" };
        let inst = instances::insert(&mut tx, &new(status, over.as_deref())).await.map_err(conflict)?;
        if status == "queued" && p.cost > 0 {
            spend::reserve(&mut tx, inst.id, p.cost, "enqueue").await.map_err(|e| fail(e.into()))?;
        }
        created.push(inst);
    }
    if !problems.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "invalid", "message": "some targets were refused; nothing was enqueued", "problems": problems })),
        )
            .into_response());
    }
    audit::record(&mut tx, &who.subject, "post.enqueue", "post", Some(id.to_string()), None, json(&created))
        .await
        .map_err(|e| fail(e.into()))?;
    tx.commit().await.map_err(|e| fail(e.into()))?;
    Ok((StatusCode::CREATED, Json(created)))
}
