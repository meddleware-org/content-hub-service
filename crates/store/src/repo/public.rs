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
    /// The roadmap for people reading it in a browser.
    Human,
    /// The roadmap for bots and machine readers.
    Bot,
}

/// A phase as the public sees it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicPhase {
    /// The public id.
    pub id: Uuid,
    /// The public name.
    pub name: String,
    /// The public description.
    pub description: String,
    /// When the phase starts, if dated.
    pub window_start: Option<DateTime<Utc>>,
    /// When the phase ends, if dated.
    pub window_end: Option<DateTime<Utc>>,
    /// The phase's visible items.
    pub items: Vec<PublicItem>,
}

/// An item as the public sees it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicItem {
    /// The public id.
    pub id: Uuid,
    /// The public title.
    pub title: String,
    /// The public description.
    pub description: String,
    /// Where it stands.
    pub status: String,
    /// Only when the operator chose to publish a date (e.g. a launch time).
    pub due_at: Option<DateTime<Utc>>,
    /// The public id of the phase it belongs to (not part of the JSON).
    #[serde(skip)]
    pub phase_id: Uuid,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A post published to an owned feed.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FeedEntry {
    /// The public id.
    pub id: Uuid,
    /// The title.
    pub title: String,
    /// The body, in the canonical Markdown subset.
    pub body_markdown: String,
    /// When it was published.
    pub published_at: DateTime<Utc>,
    /// When it last changed.
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
    /// The network.
    pub network: String,
    /// When the snapshot was generated.
    pub generated_at: DateTime<Utc>,
    /// The version of the document's format.
    pub schema_version: i32,
    /// The SHA-256 of the document.
    pub sha256: String,
    /// The document's size in bytes.
    pub byte_size: i32,
    /// The Sui personal-message signature, base64.
    pub signature: String,
    /// The signer's Sui address.
    pub signer_address: String,
    /// The Walrus blob id, if stored.
    pub blob_id: Option<String>,
    /// The Sui object id of the blob, if stored.
    pub object_id: Option<String>,
    /// How many Walrus epochs it was stored for.
    pub epochs: Option<i32>,
    /// When the storage is estimated to run out.
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
