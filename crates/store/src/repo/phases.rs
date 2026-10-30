//! Roadmap phases.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Phase {
    pub id: i64,
    pub key: String,
    pub public_id: Uuid,
    pub sort_order: i32,
    pub internal_name: String,
    pub internal_description: String,
    pub public_name: String,
    pub public_description: String,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
    pub visible_human: bool,
    pub visible_bot: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The writable fields (create, or the full new state of an update).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseFields {
    pub key: String,
    pub sort_order: i32,
    pub internal_name: String,
    pub internal_description: String,
    pub public_name: String,
    pub public_description: String,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
    pub visible_human: bool,
    pub visible_bot: bool,
}

impl From<&Phase> for PhaseFields {
    fn from(p: &Phase) -> Self {
        PhaseFields {
            key: p.key.clone(),
            sort_order: p.sort_order,
            internal_name: p.internal_name.clone(),
            internal_description: p.internal_description.clone(),
            public_name: p.public_name.clone(),
            public_description: p.public_description.clone(),
            window_start: p.window_start,
            window_end: p.window_end,
            visible_human: p.visible_human,
            visible_bot: p.visible_bot,
        }
    }
}

pub async fn list(conn: &mut PgConnection) -> Result<Vec<Phase>> {
    Ok(sqlx::query_as!(Phase, "SELECT * FROM hub.roadmap_phases ORDER BY sort_order, id").fetch_all(conn).await?)
}

pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Phase>> {
    Ok(sqlx::query_as!(Phase, "SELECT * FROM hub.roadmap_phases WHERE id = $1", id).fetch_optional(conn).await?)
}

pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Phase>> {
    Ok(sqlx::query_as!(Phase, "SELECT * FROM hub.roadmap_phases WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

pub async fn insert(conn: &mut PgConnection, f: &PhaseFields) -> Result<Phase> {
    Ok(sqlx::query_as!(
        Phase,
        r#"INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, internal_description, public_name, public_description,
                                           window_start, window_end, visible_human, visible_bot)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *"#,
        f.key,
        f.sort_order,
        f.internal_name,
        f.internal_description,
        f.public_name,
        f.public_description,
        f.window_start,
        f.window_end,
        f.visible_human,
        f.visible_bot
    )
    .fetch_one(conn)
    .await?)
}

pub async fn update(conn: &mut PgConnection, id: i64, f: &PhaseFields) -> Result<Phase> {
    Ok(sqlx::query_as!(
        Phase,
        r#"UPDATE hub.roadmap_phases SET key = $2, sort_order = $3, internal_name = $4, internal_description = $5,
                  public_name = $6, public_description = $7, window_start = $8, window_end = $9, visible_human = $10,
                  visible_bot = $11
            WHERE id = $1 RETURNING *"#,
        id,
        f.key,
        f.sort_order,
        f.internal_name,
        f.internal_description,
        f.public_name,
        f.public_description,
        f.window_start,
        f.window_end,
        f.visible_human,
        f.visible_bot
    )
    .fetch_one(conn)
    .await?)
}

/// Upsert by key (the seed import).
pub async fn upsert(conn: &mut PgConnection, f: &PhaseFields) -> Result<Phase> {
    Ok(sqlx::query_as!(
        Phase,
        r#"INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, internal_description, public_name, public_description,
                                           window_start, window_end, visible_human, visible_bot)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (key) DO UPDATE SET sort_order = EXCLUDED.sort_order, internal_name = EXCLUDED.internal_name,
                  internal_description = EXCLUDED.internal_description, public_name = EXCLUDED.public_name,
                  public_description = EXCLUDED.public_description, window_start = EXCLUDED.window_start,
                  window_end = EXCLUDED.window_end, visible_human = EXCLUDED.visible_human, visible_bot = EXCLUDED.visible_bot
           RETURNING *"#,
        f.key,
        f.sort_order,
        f.internal_name,
        f.internal_description,
        f.public_name,
        f.public_description,
        f.window_start,
        f.window_end,
        f.visible_human,
        f.visible_bot
    )
    .fetch_one(conn)
    .await?)
}
