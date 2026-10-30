//! Operators seen by the admin API: only the identity provider's subject and a display name.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct User {
    pub subject: String,
    pub display_name: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Create or refresh the operator's row (on every authenticated request; cheap, one row).
pub async fn touch(conn: &mut PgConnection, subject: &str, display_name: &str) -> Result<User> {
    Ok(sqlx::query_as!(
        User,
        r#"INSERT INTO hub.users (subject, display_name) VALUES ($1, $2)
           ON CONFLICT (subject) DO UPDATE SET display_name = EXCLUDED.display_name, last_seen_at = now()
           RETURNING subject, display_name, first_seen_at, last_seen_at"#,
        subject,
        display_name
    )
    .fetch_one(conn)
    .await?)
}
