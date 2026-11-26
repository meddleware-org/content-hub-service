//! Tasks, their dependencies, platforms and links. Status changes go through `set_status` only (the API checks the
//! state machine in `content-hub-core` first; the table's CHECKs hold the column invariants).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use super::{Page, PageRequest};
use crate::Result;

/// One operator task.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Task {
    /// The task's id.
    pub id: i64,
    /// The roadmap item it belongs to.
    pub roadmap_item_id: Option<i64>,
    /// The task's stable key.
    pub key: String,
    /// What kind of task it is.
    pub task_type: String,
    /// The title.
    pub title: String,
    /// The description.
    pub description: String,
    /// Where it stands.
    pub status: String,
    /// Why it is blocked (set only while blocked).
    pub blocked_reason: Option<String>,
    /// When it is scheduled.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// When it was finished (set only when done).
    pub completed_at: Option<DateTime<Utc>>,
    /// Days between reviews of a recurring task.
    pub review_interval_days: Option<i32>,
    /// When the next review is due.
    pub next_review_at: Option<DateTime<Utc>>,
    /// The subject of the person it is assigned to.
    pub assignee_subject: Option<String>,
    /// Whether it waits for the legal entity to exist.
    pub entity_gated: bool,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// Fields set on create or update (not the status: see `set_status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFields {
    /// The roadmap item.
    pub roadmap_item_id: Option<i64>,
    /// The stable key.
    pub key: String,
    /// The kind of task.
    pub task_type: String,
    /// The title.
    pub title: String,
    /// The description.
    pub description: String,
    /// When it is scheduled.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// Days between reviews.
    pub review_interval_days: Option<i32>,
    /// When the next review is due.
    pub next_review_at: Option<DateTime<Utc>>,
    /// The assignee's subject.
    pub assignee_subject: Option<String>,
    /// Whether it waits for the legal entity.
    pub entity_gated: bool,
}

impl From<&Task> for TaskFields {
    /// The editable fields of a task, taken from the stored one so a patch starts from the current state.
    fn from(t: &Task) -> Self {
        TaskFields {
            roadmap_item_id: t.roadmap_item_id,
            key: t.key.clone(),
            task_type: t.task_type.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            scheduled_at: t.scheduled_at,
            review_interval_days: t.review_interval_days,
            next_review_at: t.next_review_at,
            assignee_subject: t.assignee_subject.clone(),
            entity_gated: t.entity_gated,
        }
    }
}

/// A platform a task is about, with an optional target.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TaskPlatform {
    /// The row's id.
    pub id: i64,
    /// The platform definition's id.
    pub platform_definition_id: i64,
    /// The platform's code.
    pub platform_code: String,
    /// The target, if the task names one.
    pub platform_target_id: Option<i64>,
}

/// A link from a task to a post, an instance or an outside page.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TaskLink {
    /// The link's id.
    pub id: i64,
    /// What it links to.
    pub link_kind: String,
    /// The post, for a post link.
    pub post_id: Option<i64>,
    /// The instance, for an instance link.
    pub platform_instance_id: Option<i64>,
    /// The address, for an outside link.
    pub url: Option<String>,
    /// A short label.
    pub label: String,
    /// When it was added.
    pub created_at: DateTime<Utc>,
}

/// A filter for the list of tasks.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    /// Only this status.
    pub status: Option<String>,
    /// Only this kind.
    pub task_type: Option<String>,
    /// Only this item.
    pub roadmap_item_id: Option<i64>,
    /// Only tasks that do or do not wait for the entity.
    pub entity_gated: Option<bool>,
    /// Tasks whose review is due at or before this time.
    pub review_due_before: Option<DateTime<Utc>>,
}

/// A page of tasks matching the filter.
pub async fn list(conn: &mut PgConnection, f: &TaskFilter, page: PageRequest) -> Result<Page<Task>> {
    let rows = sqlx::query_as!(
        Task,
        r#"SELECT * FROM hub.tasks
            WHERE id < $1 AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR task_type = $3)
              AND ($4::bigint IS NULL OR roadmap_item_id = $4) AND ($5::bool IS NULL OR entity_gated = $5)
              AND ($6::timestamptz IS NULL OR next_review_at <= $6)
            ORDER BY id DESC LIMIT $7"#,
        page.bound(),
        f.status,
        f.task_type,
        f.roadmap_item_id,
        f.entity_gated,
        f.review_due_before,
        page.fetch()
    )
    .fetch_all(conn)
    .await?;
    // One more row was fetched than the page holds, to know whether a next page exists.
    Ok(page.page(rows, |r| r.id))
}

/// One task.
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Task>> {
    // `None` when there is no such task.
    Ok(sqlx::query_as!(Task, "SELECT * FROM hub.tasks WHERE id = $1", id).fetch_optional(conn).await?)
}

/// One task, locked for the rest of the transaction.
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Task>> {
    // The lock keeps a concurrent change from slipping between the read and the write.
    Ok(sqlx::query_as!(Task, "SELECT * FROM hub.tasks WHERE id = $1 FOR UPDATE", id).fetch_optional(conn).await?)
}

/// The id of the task with this key.
pub async fn id_by_key(conn: &mut PgConnection, key: &str) -> Result<Option<i64>> {
    // Keys are unique, so there is at most one.
    Ok(sqlx::query_scalar!("SELECT id FROM hub.tasks WHERE key = $1", key).fetch_optional(conn).await?)
}

/// Insert a task with a starting status.
pub async fn insert(conn: &mut PgConnection, f: &TaskFields, status: &str) -> Result<Task> {
    // A task created as done gets its completion time now (the database requires it).
    let completed: Option<DateTime<Utc>> = (status == "done").then(Utc::now);
    Ok(sqlx::query_as!(
        Task,
        r#"INSERT INTO hub.tasks (roadmap_item_id, key, task_type, title, description, status, scheduled_at, completed_at,
                                  review_interval_days, next_review_at, assignee_subject, entity_gated, blocked_reason)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, CASE WHEN $6 = 'blocked' THEN 'imported as blocked' END)
           RETURNING *"#,
        f.roadmap_item_id,
        f.key,
        f.task_type,
        f.title,
        f.description,
        status,
        f.scheduled_at,
        completed,
        f.review_interval_days,
        f.next_review_at,
        f.assignee_subject,
        f.entity_gated
    )
    .fetch_one(conn)
    .await?)
}

/// Replace a task's fields (not its status).
pub async fn update(conn: &mut PgConnection, id: i64, f: &TaskFields) -> Result<Task> {
    Ok(sqlx::query_as!(
        Task,
        r#"UPDATE hub.tasks SET roadmap_item_id = $2, key = $3, task_type = $4, title = $5, description = $6, scheduled_at = $7,
                  review_interval_days = $8, next_review_at = $9, assignee_subject = $10, entity_gated = $11
            WHERE id = $1 RETURNING *"#,
        id,
        f.roadmap_item_id,
        f.key,
        f.task_type,
        f.title,
        f.description,
        f.scheduled_at,
        f.review_interval_days,
        f.next_review_at,
        f.assignee_subject,
        f.entity_gated
    )
    .fetch_one(conn)
    .await?)
}

/// Upsert by key for the seed import; the status is set directly (an import is not a transition).
pub async fn upsert(conn: &mut PgConnection, f: &TaskFields, status: &str) -> Result<Task> {
    // The same for an import that sets a task to done.
    let completed: Option<DateTime<Utc>> = (status == "done").then(Utc::now);
    Ok(sqlx::query_as!(
        Task,
        r#"INSERT INTO hub.tasks (roadmap_item_id, key, task_type, title, description, status, scheduled_at, completed_at,
                                  review_interval_days, next_review_at, assignee_subject, entity_gated, blocked_reason)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, CASE WHEN $6 = 'blocked' THEN 'imported as blocked' END)
           ON CONFLICT (key) DO UPDATE SET roadmap_item_id = EXCLUDED.roadmap_item_id, task_type = EXCLUDED.task_type,
                  title = EXCLUDED.title, description = EXCLUDED.description, status = EXCLUDED.status,
                  scheduled_at = EXCLUDED.scheduled_at,
                  completed_at = CASE WHEN EXCLUDED.status = 'done' THEN COALESCE(hub.tasks.completed_at, EXCLUDED.completed_at) END,
                  review_interval_days = EXCLUDED.review_interval_days, next_review_at = EXCLUDED.next_review_at,
                  entity_gated = EXCLUDED.entity_gated,
                  blocked_reason = CASE WHEN EXCLUDED.status = 'blocked' THEN COALESCE(hub.tasks.blocked_reason, EXCLUDED.blocked_reason) END
           RETURNING *"#,
        f.roadmap_item_id,
        f.key,
        f.task_type,
        f.title,
        f.description,
        status,
        f.scheduled_at,
        completed,
        f.review_interval_days,
        f.next_review_at,
        f.assignee_subject,
        f.entity_gated
    )
    .fetch_one(conn)
    .await?)
}

/// Apply a checked transition. `completed_at` follows `done`; `blocked_reason` follows `blocked`.
pub async fn set_status(conn: &mut PgConnection, id: i64, status: &str, blocked_reason: Option<&str>) -> Result<Task> {
    Ok(sqlx::query_as!(
        Task,
        r#"UPDATE hub.tasks SET status = $2,
                  blocked_reason = CASE WHEN $2 = 'blocked' THEN $3 END,
                  completed_at = CASE WHEN $2 = 'done' THEN now() END
            WHERE id = $1 RETURNING *"#,
        id,
        status,
        blocked_reason
    )
    .fetch_one(conn)
    .await?)
}

/// Dependencies of one task that are not done.
pub async fn unfinished_dependencies(conn: &mut PgConnection, id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM hub.task_dependencies d JOIN hub.tasks t ON t.id = d.depends_on_task_id
            WHERE d.task_id = $1 AND t.status <> 'done'"#,
        id
    )
    .fetch_one(conn)
    .await?)
}

/// Every dependency edge (task, depends_on), for the cycle check.
pub async fn all_dependencies(conn: &mut PgConnection) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query!("SELECT task_id, depends_on_task_id FROM hub.task_dependencies")
        .fetch_all(conn)
        .await?
        .into_iter()
        .map(|r| (r.task_id, r.depends_on_task_id))
        .collect())
}

/// The ids of the tasks this one depends on.
pub async fn dependencies(conn: &mut PgConnection, id: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar!("SELECT depends_on_task_id FROM hub.task_dependencies WHERE task_id = $1 ORDER BY 1", id)
        .fetch_all(conn)
        .await?)
}

/// Replace a task's dependencies.
pub async fn set_dependencies(conn: &mut PgConnection, id: i64, deps: &[i64]) -> Result<()> {
    sqlx::query!("DELETE FROM hub.task_dependencies WHERE task_id = $1", id).execute(&mut *conn).await?;
    sqlx::query!(
        "INSERT INTO hub.task_dependencies (task_id, depends_on_task_id) SELECT $1, unnest($2::bigint[])",
        id,
        deps
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The platforms a task is about.
pub async fn platforms(conn: &mut PgConnection, id: i64) -> Result<Vec<TaskPlatform>> {
    Ok(sqlx::query_as!(
        TaskPlatform,
        r#"SELECT tp.id, tp.platform_definition_id, d.code AS platform_code, tp.platform_target_id
             FROM hub.task_platforms tp JOIN hub.platform_definitions d ON d.id = tp.platform_definition_id
            WHERE tp.task_id = $1 ORDER BY tp.id"#,
        id
    )
    .fetch_all(conn)
    .await?)
}

/// Replace a task's platforms: (definition, optional target) pairs.
pub async fn set_platforms(conn: &mut PgConnection, id: i64, pairs: &[(i64, Option<i64>)]) -> Result<()> {
    sqlx::query!("DELETE FROM hub.task_platforms WHERE task_id = $1", id).execute(&mut *conn).await?;
    for (def, target) in pairs {
        sqlx::query!(
            "INSERT INTO hub.task_platforms (task_id, platform_definition_id, platform_target_id) VALUES ($1, $2, $3)",
            id,
            def,
            *target
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The links on a task.
pub async fn links(conn: &mut PgConnection, id: i64) -> Result<Vec<TaskLink>> {
    Ok(sqlx::query_as!(
        TaskLink,
        "SELECT id, link_kind, post_id, platform_instance_id, url, label, created_at FROM hub.task_links WHERE task_id = $1 ORDER BY id",
        id
    )
    .fetch_all(conn)
    .await?)
}

/// Add a link to a task; exactly the column that matches the kind is set.
pub async fn add_link(
    conn: &mut PgConnection,
    task_id: i64,
    kind: &str,
    post_id: Option<i64>,
    instance_id: Option<i64>,
    url: Option<&str>,
    label: &str,
) -> Result<TaskLink> {
    Ok(sqlx::query_as!(
        TaskLink,
        r#"INSERT INTO hub.task_links (task_id, link_kind, post_id, platform_instance_id, url, label) VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id, link_kind, post_id, platform_instance_id, url, label, created_at"#,
        task_id,
        kind,
        post_id,
        instance_id,
        url,
        label
    )
    .fetch_one(conn)
    .await?)
}

/// Delete one link of a task; false when it does not exist (or belongs to another task).
pub async fn delete_link(conn: &mut PgConnection, task_id: i64, link_id: i64) -> Result<bool> {
    Ok(sqlx::query!("DELETE FROM hub.task_links WHERE id = $1 AND task_id = $2", link_id, task_id)
        .execute(conn)
        .await?
        .rows_affected()
        == 1)
}
