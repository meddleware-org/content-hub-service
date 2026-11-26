//! Tasks: fields, dependencies (no cycles), platforms, links, and status changes through the state machine.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use content_hub_core::model::{LinkKind, TaskStatus, TaskType};
use content_hub_core::task_state::{self, TaskContext, TaskTransitionError, has_dependency_cycle};
use content_hub_store::repo::tasks::{Task, TaskFields, TaskFilter, TaskLink, TaskPlatform};
use content_hub_store::repo::{Page, PageRequest, audit, tasks};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use super::{AdminState, found, https_url, json, key, parse_enum, text};
use crate::auth::Identity;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Body, Query, nullable};

/// A platform (and optionally a target) a task is about.
#[derive(Deserialize, Serialize, Clone, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlatformRef {
    /// The platform definition.
    pub platform_definition_id: i64,
    /// The target on that platform.
    #[serde(default)]
    pub platform_target_id: Option<i64>,
}

/// The body of `POST /api/v1/tasks`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    /// The stable key.
    pub key: String,
    /// What kind of task it is.
    pub task_type: String,
    /// The title.
    pub title: String,
    /// The description.
    #[serde(default)]
    pub description: String,
    /// The roadmap item.
    #[serde(default)]
    pub roadmap_item_id: Option<i64>,
    /// When it is scheduled.
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
    /// Days between reviews of a recurring task.
    #[serde(default)]
    pub review_interval_days: Option<i32>,
    /// Whether it waits for the legal entity.
    #[serde(default)]
    pub entity_gated: bool,
    /// The ids of the tasks that must be done first.
    #[serde(default)]
    pub depends_on: Vec<i64>,
    /// The platforms it is about.
    #[serde(default)]
    pub platforms: Vec<PlatformRef>,
}

/// The body of `PATCH /api/v1/tasks/{id}`; a field that is left out is unchanged.
#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskPatch {
    /// A new key.
    pub key: Option<String>,
    /// A new kind.
    pub task_type: Option<String>,
    /// A new title.
    pub title: Option<String>,
    /// A new description.
    pub description: Option<String>,
    /// A new roadmap item; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub roadmap_item_id: Option<Option<i64>>,
    /// A new date; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub scheduled_at: Option<Option<DateTime<Utc>>>,
    /// A new review interval; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub review_interval_days: Option<Option<i32>>,
    /// A new next review; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub next_review_at: Option<Option<DateTime<Utc>>>,
    /// A new assignee; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    pub assignee_subject: Option<Option<String>>,
    /// Change whether it waits for the entity.
    pub entity_gated: Option<bool>,
    /// Replace the dependencies.
    pub depends_on: Option<Vec<i64>>,
    /// Replace the platforms.
    pub platforms: Option<Vec<PlatformRef>>,
}

/// The body of `POST /api/v1/tasks/{id}/transition`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskTransition {
    /// The status to move to.
    pub to: String,
    /// Why, for a move to `blocked`.
    #[serde(default)]
    pub blocked_reason: Option<String>,
}

/// The body of `POST /api/v1/tasks/{id}/links`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewLink {
    /// What the link points to: `post`, `platform_instance` or `external`.
    pub link_kind: String,
    /// The post, for a post link.
    #[serde(default)]
    pub post_id: Option<i64>,
    /// The instance, for an instance link.
    #[serde(default)]
    pub platform_instance_id: Option<i64>,
    /// The address, for an outside link.
    #[serde(default)]
    pub url: Option<String>,
    /// A short label.
    #[serde(default)]
    pub label: String,
}

/// The query of `GET /api/v1/tasks`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskQuery {
    /// The cursor from the previous page.
    pub cursor: Option<String>,
    /// How many rows to return.
    pub limit: Option<i64>,
    /// Only this status.
    pub status: Option<String>,
    /// Only this kind.
    pub task_type: Option<String>,
    /// Only this item.
    pub roadmap_item_id: Option<i64>,
    /// Only tasks that do or do not wait for the entity.
    pub entity_gated: Option<bool>,
    /// `true`: only tasks whose review is due now.
    pub review_due: Option<bool>,
}

/// A task with its dependencies, platforms and links.
#[derive(Serialize, utoipa::ToSchema)]
pub struct TaskDetail {
    /// The task.
    pub task: Task,
    /// The ids of the tasks it depends on.
    pub depends_on: Vec<i64>,
    /// The platforms it is about.
    pub platforms: Vec<TaskPlatform>,
    /// Its links.
    pub links: Vec<TaskLink>,
    /// The statuses this task may move to now (the state machine and its dependencies).
    pub allowed_moves: Vec<String>,
}

/// Check a task's fields against the database's limits.
fn check_task(f: &TaskFields) -> ApiResult<()> {
    key("key", &f.key)?;
    parse_enum::<TaskType>("task_type", &f.task_type)?;
    text("title", &f.title, 1, 200)?;
    text("description", &f.description, 0, 4000)?;
    if f.review_interval_days.is_some_and(|d| !(1..=3650).contains(&d)) {
        return Err(ApiError::Invalid("review_interval_days must be 1 to 3650".into()));
    }
    if f.next_review_at.is_some() && f.review_interval_days.is_none() {
        return Err(ApiError::Invalid("next_review_at needs review_interval_days".into()));
    }
    Ok(())
}

/// Replace a task's dependencies, refusing a cycle or an unknown task.
async fn set_dependencies(tx: &mut PgConnection, id: i64, deps: &[i64]) -> ApiResult<()> {
    if deps.contains(&id) {
        return Err(ApiError::Invalid("a task cannot depend on itself".into()));
    }
    let mut edges: Vec<(i64, i64)> = tasks::all_dependencies(tx).await?.into_iter().filter(|(t, _)| *t != id).collect();
    edges.extend(deps.iter().map(|d| (id, *d)));
    if has_dependency_cycle(&edges) {
        return Err(ApiError::Invalid("these dependencies would create a cycle".into()));
    }
    tasks::set_dependencies(tx, id, deps).await?;
    Ok(())
}

/// Load a task's dependencies, platforms and links.
async fn detail(c: &mut PgConnection, task: Task) -> ApiResult<TaskDetail> {
    let from: TaskStatus = parse_enum("status", &task.status)?;
    let unfinished = tasks::unfinished_dependencies(c, task.id).await? as usize;
    let allowed_moves = TaskStatus::ALL
        .iter()
        .filter(|&&to| {
            // A move to `blocked` is offered as allowed (with a stand-in reason); the real reason is asked for when the move is made.
            let reason = (to == TaskStatus::Blocked).then_some("reason");
            task_state::check(from, to, TaskContext { unfinished_dependencies: unfinished, blocked_reason: reason })
                .is_ok()
        })
        // The statuses this task can move to now, so the UI offers only those.
        .map(|t| t.as_str().to_owned())
        .collect();
    Ok(TaskDetail {
        depends_on: tasks::dependencies(c, task.id).await?,
        platforms: tasks::platforms(c, task.id).await?,
        links: tasks::links(c, task.id).await?,
        allowed_moves,
        task,
    })
}

/// `GET /api/v1/tasks`: a page of tasks.
pub async fn list(State(s): State<AdminState>, Query(q): Query<TaskQuery>) -> ApiResult<Json<Page<Task>>> {
    let page = PageRequest::new(q.cursor.as_deref(), q.limit).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut c = s.db.acquire().await?;
    let f = TaskFilter {
        status: q.status,
        task_type: q.task_type,
        roadmap_item_id: q.roadmap_item_id,
        entity_gated: q.entity_gated,
        // `review_due=true` means tasks whose review date has passed.
        review_due_before: q.review_due.filter(|b| *b).map(|_| Utc::now()),
    };
    Ok(Json(tasks::list(&mut c, &f, page).await?))
}

/// `GET /api/v1/tasks/{id}`: a task with its dependencies, platforms and links.
pub async fn get_one(State(s): State<AdminState>, Path(id): Path<i64>) -> ApiResult<Json<TaskDetail>> {
    let mut c = s.db.acquire().await?;
    let t = found(tasks::get(&mut c, id).await?)?;
    Ok(Json(detail(&mut c, t).await?))
}

/// `POST /api/v1/tasks`: create a task; the change is audited.
pub async fn create(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Body(b): Body<NewTask>,
) -> ApiResult<(StatusCode, Json<TaskDetail>)> {
    let f = TaskFields {
        roadmap_item_id: b.roadmap_item_id,
        key: b.key,
        task_type: b.task_type,
        title: b.title,
        description: b.description,
        scheduled_at: b.scheduled_at,
        review_interval_days: b.review_interval_days,
        // A recurring task's first review is one interval from now.
        next_review_at: b.review_interval_days.map(|d| Utc::now() + Duration::days(i64::from(d))),
        // A new task is unassigned.
        assignee_subject: None,
        entity_gated: b.entity_gated,
    };
    // Validate before the transaction starts, so a bad request costs no lock.
    check_task(&f)?;
    let mut tx = s.db.begin().await?;
    // Every new task starts `planned`; moving it is a separate, checked step.
    let t = tasks::insert(&mut tx, &f, "planned").await?;
    // Dependencies are set once the task exists, and a cycle is refused.
    set_dependencies(&mut tx, t.id, &b.depends_on).await?;
    let pairs: Vec<(i64, Option<i64>)> =
        b.platforms.iter().map(|p| (p.platform_definition_id, p.platform_target_id)).collect();
    tasks::set_platforms(&mut tx, t.id, &pairs).await?;
    let d = detail(&mut tx, t).await?;
    // The audit entry holds the whole detail.
    audit::record(&mut tx, &who.subject, "task.create", "task", Some(d.task.id.to_string()), None, json(&d)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(d)))
}

/// `PATCH /api/v1/tasks/{id}`: change a task (not its status); the change is audited.
pub async fn patch(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<TaskPatch>,
) -> ApiResult<Json<TaskDetail>> {
    let mut tx = s.db.begin().await?;
    let cur = found(tasks::get_for_update(&mut tx, id).await?)?;
    // The state before, for the audit entry.
    let before = detail(&mut tx, cur.clone()).await?;
    // Start from the stored task; each field in the patch replaces one.
    let mut f = TaskFields::from(&cur);
    // Apply a field of the patch if it is present: `None` leaves it, `Some(None)` clears it.
    macro_rules! set {
        ($($field:ident),*) => { $( if let Some(v) = b.$field { f.$field = v; } )* };
    }
    set!(
        key,
        task_type,
        title,
        description,
        roadmap_item_id,
        scheduled_at,
        review_interval_days,
        next_review_at,
        assignee_subject,
        entity_gated
    );
    check_task(&f)?;
    // The status is not part of a patch: it only changes through `transition`.
    let t = tasks::update(&mut tx, id, &f).await?;
    // Dependencies and platforms are replaced as a whole when given.
    if let Some(deps) = &b.depends_on {
        set_dependencies(&mut tx, id, deps).await?;
    }
    if let Some(ps) = &b.platforms {
        let pairs: Vec<(i64, Option<i64>)> =
            ps.iter().map(|p| (p.platform_definition_id, p.platform_target_id)).collect();
        tasks::set_platforms(&mut tx, id, &pairs).await?;
    }
    let d = detail(&mut tx, t).await?;
    audit::record(&mut tx, &who.subject, "task.update", "task", Some(id.to_string()), json(&before), json(&d)).await?;
    tx.commit().await?;
    Ok(Json(d))
}

/// `POST /api/v1/tasks/{id}/transition`: move a task along an allowed edge, with the rules about reasons and dependencies.
pub async fn transition(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<TaskTransition>,
) -> ApiResult<Json<TaskDetail>> {
    let to: TaskStatus = parse_enum("to", &b.to)?;
    let mut tx = s.db.begin().await?;
    let cur = found(tasks::get_for_update(&mut tx, id).await?)?;
    let from: TaskStatus = parse_enum("status", &cur.status)?;
    // How many dependencies are not yet done.
    let unfinished = tasks::unfinished_dependencies(&mut tx, id).await? as usize;
    let reason = b.blocked_reason.as_deref().map(str::trim);
    task_state::check(from, to, TaskContext { unfinished_dependencies: unfinished, blocked_reason: reason }).map_err(
        |e| match e {
            // A move the table forbids is a conflict with the task's state.
            TaskTransitionError::NotAllowed { .. } => ApiError::Conflict(e.to_string()),
            // A missing reason or an unfinished dependency is a problem with the request.
            _ => ApiError::Invalid(e.to_string()),
        },
    )?;
    // The database also enforces that `blocked` has a reason and `done` a completion time.
    let t = tasks::set_status(&mut tx, id, to.as_str(), reason).await?;
    audit::record(
        &mut tx,
        &who.subject,
        "task.transition",
        "task",
        Some(id.to_string()),
        Some(serde_json::json!({ "status": from })),
        Some(serde_json::json!({ "status": to, "blocked_reason": reason })),
    )
    .await?;
    let d = detail(&mut tx, t).await?;
    tx.commit().await?;
    Ok(Json(d))
}

/// `POST /api/v1/tasks/{id}/links`: add a link to a task.
pub async fn add_link(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    Body(b): Body<NewLink>,
) -> ApiResult<(StatusCode, Json<TaskLink>)> {
    let kind: LinkKind = parse_enum("link_kind", &b.link_kind)?;
    text("label", &b.label, 0, 200)?;
    let ok = match kind {
        LinkKind::Post => b.post_id.is_some() && b.platform_instance_id.is_none() && b.url.is_none(),
        LinkKind::PlatformInstance => b.platform_instance_id.is_some() && b.post_id.is_none() && b.url.is_none(),
        LinkKind::External => b.url.is_some() && b.post_id.is_none() && b.platform_instance_id.is_none(),
    };
    if !ok {
        return Err(ApiError::Invalid("give exactly the field that matches link_kind".into()));
    }
    if let Some(u) = &b.url {
        https_url("url", u)?;
    }
    let mut tx = s.db.begin().await?;
    found(tasks::get_for_update(&mut tx, id).await?)?;
    let l = tasks::add_link(&mut tx, id, kind.as_str(), b.post_id, b.platform_instance_id, b.url.as_deref(), &b.label)
        .await?;
    audit::record(&mut tx, &who.subject, "task.link.add", "task", Some(id.to_string()), None, json(&l)).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(l)))
}

/// `DELETE /api/v1/tasks/{id}/links/{link_id}`: remove a link.
pub async fn delete_link(
    State(s): State<AdminState>,
    Extension(who): Extension<Identity>,
    Path((id, link_id)): Path<(i64, i64)>,
) -> ApiResult<StatusCode> {
    let mut tx = s.db.begin().await?;
    if !tasks::delete_link(&mut tx, id, link_id).await? {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut tx,
        &who.subject,
        "task.link.delete",
        "task",
        Some(id.to_string()),
        Some(serde_json::json!({ "link_id": link_id })),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
