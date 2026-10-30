//! The public process's reads. They touch only the `public.public_v1_*` views (the read-only role can read nothing
//! else), so every type here is public by construction.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Result;

/// Which audience a roadmap is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Human,
    Bot,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicPhase {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
    pub items: Vec<PublicItem>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicItem {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub status: String,
    /// Only when the operator chose to publish a date (e.g. a launch time).
    pub due_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub phase_id: Uuid,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FeedEntry {
    pub id: Uuid,
    pub title: String,
    pub body_markdown: String,
    pub published_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The roadmap for one audience: phases in order, each with its items (oldest first).
pub async fn roadmap(conn: &mut PgConnection, audience: Audience) -> Result<Vec<PublicPhase>> {
    let human = audience == Audience::Human;
    let phases = sqlx::query!(
        r#"SELECT public_id AS "public_id!", name AS "name!", description AS "description!", window_start, window_end
             FROM public.public_v1_phases
            WHERE CASE WHEN $1 THEN visible_human ELSE visible_bot END
            ORDER BY sort_order"#,
        human
    )
    .fetch_all(&mut *conn)
    .await?;
    let items = sqlx::query_as!(
        PublicItem,
        r#"SELECT public_id AS "id!", phase_public_id AS "phase_id!", title AS "title!", description AS "description!",
                  status AS "status!", due_at, updated_at AS "updated_at!"
             FROM public.public_v1_items
            WHERE CASE WHEN $1 THEN visible_human ELSE visible_bot END
            ORDER BY updated_at, public_id"#,
        human
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(phases
        .into_iter()
        .map(|p| PublicPhase {
            items: items.iter().filter(|i| i.phase_id == p.public_id).cloned().collect(),
            id: p.public_id,
            name: p.name,
            description: p.description,
            window_start: p.window_start,
            window_end: p.window_end,
        })
        .collect())
}

/// The newest owned-feed entries.
pub async fn feed(conn: &mut PgConnection, limit: i64) -> Result<Vec<FeedEntry>> {
    Ok(sqlx::query_as!(
        FeedEntry,
        r#"SELECT public_id AS "id!", title AS "title!", body_markdown AS "body_markdown!", published_at AS "published_at!",
                  updated_at AS "updated_at!"
             FROM public.public_v1_feed_entries
            ORDER BY published_at DESC, public_id LIMIT $1"#,
        limit
    )
    .fetch_all(conn)
    .await?)
}

/// The latest signed snapshot as the public view exposes it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicSnapshot {
    pub network: String,
    pub generated_at: DateTime<Utc>,
    pub schema_version: i32,
    pub sha256: String,
    pub byte_size: i32,
    pub signature: String,
    pub signer_address: String,
    pub blob_id: Option<String>,
    pub object_id: Option<String>,
    pub epochs: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// The most recent snapshot, of one network or (none given) of any.
pub async fn latest_snapshot(conn: &mut PgConnection, network: Option<&str>) -> Result<Option<PublicSnapshot>> {
    Ok(sqlx::query_as!(
        PublicSnapshot,
        r#"SELECT network AS "network!", generated_at AS "generated_at!", schema_version AS "schema_version!", sha256 AS "sha256!",
                  byte_size AS "byte_size!", signature AS "signature!", signer_address AS "signer_address!", blob_id, object_id,
                  epochs, expires_at
             FROM public.public_v1_snapshots
            WHERE ($1::text IS NULL OR network = $1)
            ORDER BY generated_at DESC LIMIT 1"#,
        network
    )
    .fetch_optional(conn)
    .await?)
}
