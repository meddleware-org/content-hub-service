//! Roadmap items.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Page, PageRequest};
use crate::Result;

/// One roadmap item, with its internal and public wording.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Item {
    /// The item's id.
    pub id: i64,
    /// The phase it belongs to.
    pub phase_id: i64,
    /// The item's stable key.
    pub key: String,
    /// The id the public views show instead of the internal one.
    pub public_id: Uuid,
    /// What kind of item it is.
    pub item_type: String,
    /// The title the operator sees.
    pub internal_title: String,
    /// The description the operator sees.
    pub internal_description: String,
    /// The title the public views show.
    pub public_title: Option<String>,
    /// The description the public views show.
    pub public_description: Option<String>,
    /// Whether people reading in a browser see it.
    pub visible_human: bool,
    /// Whether the feeds for bots carry it.
    pub visible_bot: bool,
    /// Where it stands.
    pub status: String,
    /// When it is planned to start (never public).
    pub planned_at: Option<DateTime<Utc>>,
    /// When it is due (never public).
    pub due_at: Option<DateTime<Utc>>,
    /// The one date the public sees, when the operator chooses to publish it.
    pub public_due_at: Option<DateTime<Utc>>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// The writable fields of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFields {
    /// The phase it belongs to.
    pub phase_id: i64,
    /// The item's stable key.
    pub key: String,
    /// What kind of item it is.
    pub item_type: String,
    /// The title the operator sees.
    pub internal_title: String,
    /// The description the operator sees.
    pub internal_description: String,
    /// The title the public views show.
    pub public_title: Option<String>,
    /// The description the public views show.
    pub public_description: Option<String>,
    /// Whether people reading in a browser see it.
    pub visible_human: bool,
    /// Whether the feeds for bots carry it.
    pub visible_bot: bool,
    /// Where it stands.
    pub status: String,
    /// When it is planned to start.
    pub planned_at: Option<DateTime<Utc>>,
    /// When it is due.
    pub due_at: Option<DateTime<Utc>>,
    /// The date the public sees, if any.
    pub public_due_at: Option<DateTime<Utc>>,
}

impl From<&Item> for ItemFields {
    /// The editable fields of a item, taken from the stored one so a patch starts from the current state.
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

/// A filter for the list of items.
#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    /// Only this phase.
    pub phase_id: Option<i64>,
    /// Only this status.
    pub status: Option<String>,
    /// Only this kind.
    pub item_type: Option<String>,
}

/// A page of items matching the filter.
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

/// One item.
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Item>> {
    Ok(sqlx::query_as!(Item, "SELECT * FROM hub.roadmap_items WHERE id = $1", id).fetch_optional(conn).await?)
}

/// One item, locked for the rest of the transaction.
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Item>> {
    Ok(sqlx::query_as!(Item, "SELECT * FROM hub.roadmap_items WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

/// Insert an item.
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

/// Replace an item's fields.
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
