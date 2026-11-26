//! Platform definitions (seeded from the generic list; limits, templates and the enabled flag are editable).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use crate::Result;

/// One platform definition: what the platform is, how a post reaches it, and its limits.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PlatformDefinition {
    /// The definition's id.
    pub id: i64,
    /// The stable code that ties it to a publisher.
    pub code: String,
    /// The name shown to the operator.
    pub name: String,
    /// What kind of destination it is.
    pub category: String,
    /// How a post is delivered.
    pub posting_mode: String,
    /// How the platform counts characters.
    pub counting_method: Option<String>,
    /// The main length limit.
    pub primary_limit: Option<i32>,
    /// The further limits.
    pub secondary_limits: Value,
    /// The markup the platform renders.
    pub markup: String,
    /// Whether it takes media.
    pub supports_media: bool,
    /// Its base URL.
    pub base_url: Option<String>,
    /// The link templates for manual posting.
    pub url_templates: Value,
    /// What a post costs.
    pub cost_model: Value,
    /// Whether it is switched on.
    pub enabled: bool,
    /// Free text for the operator.
    pub notes: String,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// The editable fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformEdit {
    /// The name shown to the operator.
    pub name: String,
    /// The main length limit.
    pub primary_limit: Option<i32>,
    /// The further limits.
    pub secondary_limits: Value,
    /// The link templates for manual posting.
    pub url_templates: Value,
    /// Whether it is switched on.
    pub enabled: bool,
    /// Free text for the operator.
    pub notes: String,
}

impl From<&PlatformDefinition> for PlatformEdit {
    /// The editable fields of a definition, taken from the stored one so a patch starts from the current state.
    fn from(d: &PlatformDefinition) -> Self {
        PlatformEdit {
            name: d.name.clone(),
            primary_limit: d.primary_limit,
            secondary_limits: d.secondary_limits.clone(),
            url_templates: d.url_templates.clone(),
            enabled: d.enabled,
            notes: d.notes.clone(),
        }
    }
}

/// Every definition, by code.
pub async fn list(conn: &mut PgConnection) -> Result<Vec<PlatformDefinition>> {
    Ok(sqlx::query_as!(PlatformDefinition, "SELECT * FROM hub.platform_definitions ORDER BY code")
        .fetch_all(conn)
        .await?)
}

/// One definition.
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<PlatformDefinition>> {
    Ok(sqlx::query_as!(PlatformDefinition, "SELECT * FROM hub.platform_definitions WHERE id = $1", id)
        .fetch_optional(conn)
        .await?)
}

/// One definition, locked for the rest of the transaction.
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<PlatformDefinition>> {
    Ok(sqlx::query_as!(PlatformDefinition, "SELECT * FROM hub.platform_definitions WHERE id = $1 FOR UPDATE", id)
        .fetch_optional(conn)
        .await?)
}

/// The id of the definition with this code.
pub async fn id_by_code(conn: &mut PgConnection, code: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar!("SELECT id FROM hub.platform_definitions WHERE code = $1", code)
        .fetch_optional(conn)
        .await?)
}

/// Apply an edit and return the definition.
pub async fn update(conn: &mut PgConnection, id: i64, e: &PlatformEdit) -> Result<PlatformDefinition> {
    Ok(sqlx::query_as!(
        PlatformDefinition,
        r#"UPDATE hub.platform_definitions SET name = $2, primary_limit = $3, secondary_limits = $4, url_templates = $5,
                  enabled = $6, notes = $7
            WHERE id = $1 RETURNING *"#,
        id,
        e.name,
        e.primary_limit,
        e.secondary_limits,
        e.url_templates,
        e.enabled,
        e.notes
    )
    .fetch_one(conn)
    .await?)
}

/// The generic definition as seeded.
#[derive(Debug, Clone)]
pub struct SeedDefinition<'a> {
    /// The stable code.
    pub code: &'a str,
    /// The name.
    pub name: &'a str,
    /// The category.
    pub category: &'a str,
    /// How a post is delivered.
    pub posting_mode: &'a str,
    /// How the platform counts characters.
    pub counting_method: Option<&'a str>,
    /// The main limit.
    pub primary_limit: Option<i32>,
    /// The further limits.
    pub secondary_limits: Value,
    /// The markup.
    pub markup: &'a str,
    /// Whether it takes media.
    pub supports_media: bool,
    /// The base URL.
    pub base_url: Option<&'a str>,
    /// The link templates.
    pub url_templates: Value,
    /// What a post costs.
    pub cost_model: Value,
    /// Whether it starts switched on.
    pub enabled: bool,
    /// Free text for the operator.
    pub notes: &'a str,
}

/// Insert a generic definition, or refresh its descriptive fields. An operator's edits to the enabled flag and the
/// limits are kept: a re-seed only fills what the operator has not changed (insert), and updates name, category,
/// mode, markup, counting method, templates, cost model and notes, which describe the platform itself.
pub async fn upsert_generic(conn: &mut PgConnection, d: &SeedDefinition<'_>) -> Result<bool> {
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO hub.platform_definitions (code, name, category, posting_mode, counting_method, primary_limit,
                  secondary_limits, markup, supports_media, base_url, url_templates, cost_model, enabled, notes)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
           ON CONFLICT (code) DO UPDATE SET name = EXCLUDED.name, category = EXCLUDED.category, posting_mode = EXCLUDED.posting_mode,
                  counting_method = EXCLUDED.counting_method, markup = EXCLUDED.markup, supports_media = EXCLUDED.supports_media,
                  base_url = EXCLUDED.base_url, url_templates = EXCLUDED.url_templates, cost_model = EXCLUDED.cost_model,
                  notes = EXCLUDED.notes
           RETURNING (xmax = 0) AS "inserted!""#,
        d.code,
        d.name,
        d.category,
        d.posting_mode,
        d.counting_method,
        d.primary_limit,
        d.secondary_limits,
        d.markup,
        d.supports_media,
        d.base_url,
        d.url_templates,
        d.cost_model,
        d.enabled,
        d.notes
    )
    .fetch_one(conn)
    .await?;
    Ok(inserted)
}
