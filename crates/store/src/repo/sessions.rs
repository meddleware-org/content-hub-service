//! Sign-in state of the admin console: logins in progress and signed-in sessions (migration 0002). Only hashes of the
//! browser's secrets are stored; the lifetimes are applied in the queries, so an expired row is never returned.

use sqlx::PgConnection;

use crate::Result;

/// A sign-in the callback is completing, consumed by [`take_login`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// SHA-256 of the binding cookie's value.
    pub binding_hash: String,
    /// The `nonce` the ID token must carry.
    pub nonce: String,
    /// The PKCE verifier.
    pub verifier: String,
}

/// A live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The provider's `sub`.
    pub subject: String,
    /// The name shown.
    pub display_name: String,
    /// The ID token, for the provider's logout.
    pub id_token: String,
}

/// Record a sign-in in progress.
pub async fn begin_login(
    conn: &mut PgConnection,
    state_hash: &str,
    binding_hash: &str,
    nonce: &str,
    verifier: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO hub.login_attempts (state_hash, binding_hash, nonce, verifier) VALUES ($1, $2, $3, $4)",
        state_hash,
        binding_hash,
        nonce,
        verifier
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Consume a sign-in: the row is deleted whether or not it is still fresh, so a state value never works twice.
/// `None` when there is no such row or it is older than `max_age_secs`.
pub async fn take_login(conn: &mut PgConnection, state_hash: &str, max_age_secs: i64) -> Result<Option<Login>> {
    let row = sqlx::query!(
        r#"DELETE FROM hub.login_attempts WHERE state_hash = $1
           RETURNING binding_hash, nonce, verifier, (created_at > now() - make_interval(secs => $2::bigint)) AS "fresh!""#,
        state_hash,
        max_age_secs
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.filter(|r| r.fresh).map(|r| Login { binding_hash: r.binding_hash, nonce: r.nonce, verifier: r.verifier }))
}

/// Create a session for a completed sign-in.
pub async fn create_session(
    conn: &mut PgConnection,
    id_hash: &str,
    subject: &str,
    provider_sid: &str,
    display_name: &str,
    id_token: &str,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO hub.admin_sessions (id_hash, subject, provider_sid, display_name, id_token) VALUES ($1, $2, $3, $4, $5)",
        id_hash,
        subject,
        provider_sid,
        display_name,
        id_token
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Find a session that is inside both its lifetimes (`absolute_secs` since sign-in, `idle_secs` since last use) and
/// mark it used. `None` for an unknown or expired session.
pub async fn use_session(
    conn: &mut PgConnection,
    id_hash: &str,
    idle_secs: i64,
    absolute_secs: i64,
) -> Result<Option<Session>> {
    let row = sqlx::query!(
        r#"UPDATE hub.admin_sessions SET last_seen_at = now()
           WHERE id_hash = $1
             AND created_at > now() - make_interval(secs => $3::bigint)
             AND last_seen_at > now() - make_interval(secs => $2::bigint)
           RETURNING subject, display_name, id_token"#,
        id_hash,
        idle_secs,
        absolute_secs
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| Session { subject: r.subject, display_name: r.display_name, id_token: r.id_token }))
}

/// End one session (sign-out) and return what the provider's logout needs, or `None` when there was none.
pub async fn end_session(conn: &mut PgConnection, id_hash: &str) -> Result<Option<Session>> {
    let row = sqlx::query!(
        "DELETE FROM hub.admin_sessions WHERE id_hash = $1 RETURNING subject, display_name, id_token",
        id_hash
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| Session { subject: r.subject, display_name: r.display_name, id_token: r.id_token }))
}

/// End the sessions of one provider session (back-channel logout naming a `sid`). Returns how many ended.
pub async fn end_by_provider_session(conn: &mut PgConnection, provider_sid: &str) -> Result<u64> {
    Ok(sqlx::query!("DELETE FROM hub.admin_sessions WHERE provider_sid = $1 AND provider_sid <> ''", provider_sid)
        .execute(conn)
        .await?
        .rows_affected())
}

/// End every session of a person (back-channel logout naming only a `sub`). Returns how many ended.
pub async fn end_by_subject(conn: &mut PgConnection, subject: &str) -> Result<u64> {
    Ok(sqlx::query!("DELETE FROM hub.admin_sessions WHERE subject = $1", subject).execute(conn).await?.rows_affected())
}

/// Delete what has expired: sign-ins older than `login_secs` and sessions outside either lifetime. Returns the rows removed.
pub async fn purge(conn: &mut PgConnection, login_secs: i64, idle_secs: i64, absolute_secs: i64) -> Result<u64> {
    let logins = sqlx::query!(
        "DELETE FROM hub.login_attempts WHERE created_at <= now() - make_interval(secs => $1::bigint)",
        login_secs
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let sessions = sqlx::query!(
        "DELETE FROM hub.admin_sessions WHERE created_at <= now() - make_interval(secs => $2::bigint) OR last_seen_at <= now() - make_interval(secs => $1::bigint)",
        idle_secs,
        absolute_secs
    )
    .execute(conn)
    .await?
    .rows_affected();
    Ok(logins + sessions)
}
