//! Posts (canonical Markdown) and their revision history.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Post {
    pub id: i64,
    pub public_id: Uuid,
    pub title: String,
    pub body_markdown: String,
    pub status: String,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub roadmap_item_id: Option<i64>,
    pub task_id: Option<i64>,
    pub revision: i32,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PostRevision {
    pub id: i64,
    pub revision: i32,
    pub title: String,
    pub body_markdown: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostFields {
    pub title: String,
    pub body_markdown: String,
    pub status: String,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub roadmap_item_id: Option<i64>,
    pub task_id: Option<i64>,
}

impl From<&Post> for PostFields {
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

#[derive(Debug, Clone, Default)]
pub struct PostFilter {
    pub status: Option<String>,
    pub roadmap_item_id: Option<i64>,
    pub task_id: Option<i64>,
}

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

pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Post>> {
    Ok(sqlx::query_as!(Post, "SELECT * FROM hub.posts WHERE id = $1", id).fetch_optional(conn).await?)
}

pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Post>> {
    Ok(sqlx::query_as!(Post, "SELECT * FROM hub.posts WHERE id = $1 FOR UPDATE", id).fetch_optional(conn).await?)
}

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
