//! Roadmap items.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Item {
    pub id: i64,
    pub phase_id: i64,
    pub key: String,
    pub public_id: Uuid,
    pub item_type: String,
    pub internal_title: String,
    pub internal_description: String,
    pub public_title: Option<String>,
    pub public_description: Option<String>,
    pub visible_human: bool,
    pub visible_bot: bool,
    pub status: String,
    pub planned_at: Option<DateTime<Utc>>,
    pub due_at: Option<DateTime<Utc>>,
    pub public_due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFields {
    pub phase_id: i64,
    pub key: String,
    pub item_type: String,
    pub internal_title: String,
    pub internal_description: String,
    pub public_title: Option<String>,
    pub public_description: Option<String>,
    pub visible_human: bool,
    pub visible_bot: bool,
    pub status: String,
    pub planned_at: Option<DateTime<Utc>>,
    pub due_at: Option<DateTime<Utc>>,
    pub public_due_at: Option<DateTime<Utc>>,
}

impl From<&Item> for ItemFields {
    fn from(i: &Item) -> Self {
        ItemFields {
            phase_id: i.phase_id,
            key: i.key.clone(),
            item_type: i.item_type.clone(),
            internal_title: i.internal_title.clone(),
            internal_description: i.internal_description.clone(),
            public_title: i.public_title.clone(),
            public_description: i.public_description.clone(),
            visible_human: i.visible_human,
            visible_bot: i.visible_bot,
            status: i.status.clone(),
            planned_at: i.planned_at,
            due_at: i.due_at,
            public_due_at: i.public_due_at,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    pub phase_id: Option<i64>,
    pub status: Option<String>,
    pub item_type: Option<String>,
}

pub async fn list(conn: &mut PgConnection, f: &ItemFilter, page: PageRequest) -> Result<Page<Item>> {
    let rows = sqlx::query_as!(
        Item,
        r#"SELECT * FROM hub.roadmap_items
            WHERE id < $1 AND ($2::bigint IS NULL OR phase_id = $2) AND ($3::text IS NULL OR status = $3)
              AND ($4::text IS NULL OR item_type = $4)
            ORDER BY id DESC LIMIT $5"#,
        page.bound(),
        f.phase_id,
        f.status,
        f.item_type,
        page.fetch()
    )
    .fetch_all(conn)
    .await?;
    Ok(page.page(rows, |r| r.id))
}

pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Item>> {
    Ok(sqlx::query_as!(Item, "SELECT * FROM hub.roadmap_items WHERE id = $1", id).fetch_optional(conn).await?)
}

pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Item>> {
    Ok(sqlx::query_as!(Item, "SELECT * FROM hub.roadmap_items WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

pub async fn insert(conn: &mut PgConnection, f: &ItemFields) -> Result<Item> {
    Ok(sqlx::query_as!(
        Item,
        r#"INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, internal_description, public_title,
                                          public_description, visible_human, visible_bot, status, planned_at, due_at, public_due_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING *"#,
        f.phase_id,
        f.key,
        f.item_type,
        f.internal_title,
        f.internal_description,
        f.public_title,
        f.public_description,
        f.visible_human,
        f.visible_bot,
        f.status,
        f.planned_at,
        f.due_at,
        f.public_due_at
    )
    .fetch_one(conn)
    .await?)
}

pub async fn update(conn: &mut PgConnection, id: i64, f: &ItemFields) -> Result<Item> {
    Ok(sqlx::query_as!(
        Item,
        r#"UPDATE hub.roadmap_items SET phase_id = $2, key = $3, item_type = $4, internal_title = $5, internal_description = $6,
                  public_title = $7, public_description = $8, visible_human = $9, visible_bot = $10, status = $11,
                  planned_at = $12, due_at = $13, public_due_at = $14
            WHERE id = $1 RETURNING *"#,
        id,
        f.phase_id,
        f.key,
        f.item_type,
        f.internal_title,
        f.internal_description,
        f.public_title,
        f.public_description,
        f.visible_human,
        f.visible_bot,
        f.status,
        f.planned_at,
        f.due_at,
        f.public_due_at
    )
    .fetch_one(conn)
    .await?)
}

/// Upsert by key (the seed import). `public_id` is kept on update.
pub async fn upsert(conn: &mut PgConnection, f: &ItemFields) -> Result<Item> {
    Ok(sqlx::query_as!(
        Item,
        r#"INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, internal_description, public_title,
                                          public_description, visible_human, visible_bot, status, planned_at, due_at, public_due_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
           ON CONFLICT (key) DO UPDATE SET phase_id = EXCLUDED.phase_id, item_type = EXCLUDED.item_type,
                  internal_title = EXCLUDED.internal_title, internal_description = EXCLUDED.internal_description,
                  public_title = EXCLUDED.public_title, public_description = EXCLUDED.public_description,
                  visible_human = EXCLUDED.visible_human, visible_bot = EXCLUDED.visible_bot, status = EXCLUDED.status,
                  planned_at = EXCLUDED.planned_at, due_at = EXCLUDED.due_at, public_due_at = EXCLUDED.public_due_at
           RETURNING *"#,
        f.phase_id,
        f.key,
        f.item_type,
        f.internal_title,
        f.internal_description,
        f.public_title,
        f.public_description,
        f.visible_human,
        f.visible_bot,
        f.status,
        f.planned_at,
        f.due_at,
        f.public_due_at
    )
    .fetch_one(conn)
    .await?)
}
