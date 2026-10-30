//! Platform targets: an operator's destinations. `secret_ref` is the NAME of an environment variable, never a value.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PlatformTarget {
    pub id: i64,
    pub platform_definition_id: i64,
    pub platform_code: String,
    pub name: String,
    pub external_ref: Option<String>,
    pub config: Value,
    pub secret_ref: Option<String>,
    pub is_sandbox: bool,
    pub enabled: bool,
    /// Real sends (see the worker's live rules); off means dry runs.
    pub live: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetFields {
    pub platform_definition_id: i64,
    pub name: String,
    pub external_ref: Option<String>,
    pub config: Value,
    pub secret_ref: Option<String>,
    pub is_sandbox: bool,
    pub enabled: bool,
    pub live: bool,
}

impl From<&PlatformTarget> for TargetFields {
    fn from(t: &PlatformTarget) -> Self {
        TargetFields {
            platform_definition_id: t.platform_definition_id,
            name: t.name.clone(),
            external_ref: t.external_ref.clone(),
            config: t.config.clone(),
            secret_ref: t.secret_ref.clone(),
            is_sandbox: t.is_sandbox,
            enabled: t.enabled,
            live: t.live,
        }
    }
}

pub async fn list(conn: &mut PgConnection) -> Result<Vec<PlatformTarget>> {
    Ok(sqlx::query_as!(
        PlatformTarget,
        r#"SELECT t.id, t.platform_definition_id, d.code AS platform_code, t.name, t.external_ref, t.config, t.secret_ref,
                  t.is_sandbox, t.enabled, t.live, t.created_at, t.updated_at
             FROM hub.platform_targets t JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
            ORDER BY d.code, t.name"#
    )
    .fetch_all(conn)
    .await?)
}

pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<PlatformTarget>> {
    Ok(sqlx::query_as!(
        PlatformTarget,
        r#"SELECT t.id, t.platform_definition_id, d.code AS platform_code, t.name, t.external_ref, t.config, t.secret_ref,
                  t.is_sandbox, t.enabled, t.live, t.created_at, t.updated_at
             FROM hub.platform_targets t JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
            WHERE t.id = $1"#,
        id
    )
    .fetch_optional(conn)
    .await?)
}

pub async fn insert(conn: &mut PgConnection, f: &TargetFields) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO hub.platform_targets (platform_definition_id, name, external_ref, config, secret_ref, is_sandbox, enabled, live)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id"#,
        f.platform_definition_id,
        f.name,
        f.external_ref,
        f.config,
        f.secret_ref,
        f.is_sandbox,
        f.enabled,
        f.live
    )
    .fetch_one(conn)
    .await?)
}

pub async fn update(conn: &mut PgConnection, id: i64, f: &TargetFields) -> Result<()> {
    sqlx::query!(
        r#"UPDATE hub.platform_targets SET name = $2, external_ref = $3, config = $4, secret_ref = $5, is_sandbox = $6, enabled = $7,
                  live = $8
            WHERE id = $1"#,
        id,
        f.name,
        f.external_ref,
        f.config,
        f.secret_ref,
        f.is_sandbox,
        f.enabled,
        f.live
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Set one key of a target's configuration (the worker caches a Mastodon instance's limits this way).
pub async fn set_config_key(conn: &mut PgConnection, id: i64, key: &str, value: &Value) -> Result<()> {
    sqlx::query!(
        "UPDATE hub.platform_targets SET config = jsonb_set(config, ARRAY[$2::text], $3::jsonb, true) WHERE id = $1",
        id,
        key,
        value
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Every enabled target of a platform (by code), for the worker's periodic refreshes.
pub async fn enabled_for_code(conn: &mut PgConnection, code: &str) -> Result<Vec<PlatformTarget>> {
    Ok(sqlx::query_as!(
        PlatformTarget,
        r#"SELECT t.id, t.platform_definition_id, d.code AS platform_code, t.name, t.external_ref, t.config, t.secret_ref,
                  t.is_sandbox, t.enabled, t.live, t.created_at, t.updated_at
             FROM hub.platform_targets t JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
            WHERE d.code = $1 AND t.enabled ORDER BY t.id"#,
        code
    )
    .fetch_all(conn)
    .await?)
}
