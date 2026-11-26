//! Posts (canonical Markdown) and their revision history.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

/// One canonical post.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Post {
    /// The post's id.
    pub id: i64,
    /// The id a public feed shows.
    pub public_id: Uuid,
    /// The title.
    pub title: String,
    /// The body, in the canonical Markdown subset.
    pub body_markdown: String,
    /// Where the post stands.
    pub status: String,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Categories.
    pub categories: Vec<String>,
    /// The roadmap item it belongs to.
    pub roadmap_item_id: Option<i64>,
    /// The task it belongs to.
    pub task_id: Option<i64>,
    /// The revision number; it rises with every change.
    pub revision: i32,
    /// The subject of the person who created it.
    pub created_by: String,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// One saved revision of a post.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PostRevision {
    /// The revision's id.
    pub id: i64,
    /// The revision number.
    pub revision: i32,
    /// The title at that revision.
    pub title: String,
    /// The body at that revision.
    pub body_markdown: String,
    /// Who saved it.
    pub created_by: String,
    /// When it was saved.
    pub created_at: DateTime<Utc>,
}

/// The writable fields of a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostFields {
    /// The title.
    pub title: String,
    /// The body.
    pub body_markdown: String,
    /// Where the post stands.
    pub status: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Categories.
    pub categories: Vec<String>,
    /// The roadmap item.
    pub roadmap_item_id: Option<i64>,
    /// The task.
    pub task_id: Option<i64>,
}

impl From<&Post> for PostFields {
    /// The editable fields of a post, taken from the stored one so a patch starts from the current state.
    fn from(p: &Post) -> Self {
        PostFields {
            title: p.title.clone(),
            body_markdown: p.body_markdown.clone(),
            status: p.status.clone(),
            tags: p.tags.clone(),
            categories: p.categories.clone(),
            roadmap_item_id: p.roadmap_item_id,
            task_id: p.task_id,
        }
    }
}

/// A filter for the list of posts.
#[derive(Debug, Clone, Default)]
pub struct PostFilter {
    /// Only this status.
    pub status: Option<String>,
    /// Only this roadmap item.
    pub roadmap_item_id: Option<i64>,
    /// Only this task.
    pub task_id: Option<i64>,
}

/// A page of posts matching the filter.
pub async fn list(conn: &mut PgConnection, f: &PostFilter, page: PageRequest) -> Result<Page<Post>> {
    let rows = sqlx::query_as!(
        Post,
        r#"SELECT * FROM hub.posts
            WHERE id < $1 AND ($2::text IS NULL OR status = $2) AND ($3::bigint IS NULL OR roadmap_item_id = $3)
              AND ($4::bigint IS NULL OR task_id = $4)
            ORDER BY id DESC LIMIT $5"#,
        page.bound(),
        f.status,
        f.roadmap_item_id,
        f.task_id,
        page.fetch()
    )
    .fetch_all(conn)
    .await?;
    Ok(page.page(rows, |r| r.id))
}

/// One post.
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Post>> {
    Ok(sqlx::query_as!(Post, "SELECT * FROM hub.posts WHERE id = $1", id).fetch_optional(conn).await?)
}

/// One post, locked for the rest of the transaction.
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Post>> {
    Ok(sqlx::query_as!(Post, "SELECT * FROM hub.posts WHERE id = $1 FOR UPDATE", id).fetch_optional(conn).await?)
}

/// The saved revisions of a post, newest first.
pub async fn revisions(conn: &mut PgConnection, id: i64) -> Result<Vec<PostRevision>> {
    Ok(sqlx::query_as!(
        PostRevision,
        "SELECT id, revision, title, body_markdown, created_by, created_at FROM hub.post_revisions WHERE post_id = $1 ORDER BY revision DESC",
        id
    )
    .fetch_all(conn)
    .await?)
}

/// Create a post and its first revision.
pub async fn insert(conn: &mut PgConnection, f: &PostFields, created_by: &str) -> Result<Post> {
    let post = sqlx::query_as!(
        Post,
        r#"INSERT INTO hub.posts (title, body_markdown, status, tags, categories, roadmap_item_id, task_id, created_by)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"#,
        f.title,
        f.body_markdown,
        f.status,
        &f.tags,
        &f.categories,
        f.roadmap_item_id,
        f.task_id,
        created_by
    )
    .fetch_one(&mut *conn)
    .await?;
    add_revision(conn, &post, created_by).await?;
    Ok(post)
}

/// Save the post's current title and body as a revision.
async fn add_revision(conn: &mut PgConnection, p: &Post, by: &str) -> Result<()> {
    sqlx::query!(
        "INSERT INTO hub.post_revisions (post_id, revision, title, body_markdown, created_by) VALUES ($1, $2, $3, $4, $5)",
        p.id,
        p.revision,
        p.title,
        p.body_markdown,
        by
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Update a post; a change of title or body is a new revision.
pub async fn update(conn: &mut PgConnection, current: &Post, f: &PostFields, by: &str) -> Result<Post> {
    let text_changed = current.title != f.title || current.body_markdown != f.body_markdown;
    let revision = if text_changed { current.revision + 1 } else { current.revision };
    let post = sqlx::query_as!(
        Post,
        r#"UPDATE hub.posts SET title = $2, body_markdown = $3, status = $4, tags = $5, categories = $6, roadmap_item_id = $7,
                  task_id = $8, revision = $9
            WHERE id = $1 RETURNING *"#,
        current.id,
        f.title,
        f.body_markdown,
        f.status,
        &f.tags,
        &f.categories,
        f.roadmap_item_id,
        f.task_id,
        revision
    )
    .fetch_one(&mut *conn)
    .await?;
    if text_changed {
        add_revision(conn, &post, by).await?;
    }
    Ok(post)
}
