//! Schema migrations: embedded, forward-only, applied by the `migrate` Job as the database OWNER before a
//! rollout (ARCHITECTURE.md §2).
//!
//! - **Embedded and checksummed.** [`MIGRATOR`] holds `migrations/*.sql`; SQLx records each file's checksum
//!   in `public._sqlx_migrations` and refuses to continue when an applied migration's SQL changed. A
//!   released migration is never edited; a change is a new file.
//! - **Serialised.** SQLx takes a PostgreSQL advisory lock for the whole run, so two migrators never apply
//!   the same file.
//! - **No downgrade.** A database already migrated by a newer build (a version this build does not embed) is
//!   refused (`VersionMissing`), so an older Job cannot run against a schema it does not know.
//! - **Expand, then contract.** The previous release's processes keep running while a new Job migrates, so a
//!   migration must keep their queries working (add, don't rename or drop in one step).
//! - The application roles cannot run DDL. `admin`, `public` and `worker` answer 503 until
//!   [`schema_version`] reaches [`SCHEMA_VERSION`].

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnection, PgPool};
use sqlx::{Connection, PgExecutor};

use crate::config::DatabaseSettings;
use crate::error::{Result, StoreError};

/// Every migration of this build.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The schema version this build needs (its newest migration).
pub const SCHEMA_VERSION: i64 = 1;

/// A session advisory lock held around a whole run, so the versions a run reports as applied are exactly
/// the ones it applied (SQLx's own lock, a different key, covers only its part).
pub const RUN_LOCK_KEY: i64 = 0x4348_5542_4d49_4701;

/// Apply every pending migration over one connection as the owner. Returns the versions this call applied.
pub async fn run(owner: &DatabaseSettings) -> Result<Vec<i64>> {
    let mut conn = PgConnection::connect_with(&owner.connect_options()?).await?;
    sqlx::query("SELECT pg_advisory_lock($1)").bind(RUN_LOCK_KEY).execute(&mut conn).await?;
    let result = async {
        let before = applied_versions(&mut conn).await?;
        MIGRATOR.run(&mut conn).await?;
        let after = applied_versions(&mut conn).await?;
        Ok::<_, StoreError>(after.into_iter().filter(|v| !before.contains(v)).collect())
    }
    .await;
    // Closing the session releases the lock as well; unlock explicitly so a pooled caller could reuse it.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(RUN_LOCK_KEY).execute(&mut conn).await;
    let _ = conn.close().await;
    result
}

async fn applied_versions(conn: &mut PgConnection) -> Result<Vec<i64>> {
    match sqlx::query_scalar::<_, i64>("SELECT version FROM public._sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(&mut *conn)
        .await
    {
        Ok(v) => Ok(v),
        Err(e) if is_undefined_table(&e) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn is_undefined_table(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "42P01")
}

/// The newest successfully applied migration, or `None` before the first one (the table does not exist yet).
/// Readable by every role of the service (the bookkeeping table is in schema `public`).
pub async fn schema_version<'e>(db: impl PgExecutor<'e>) -> Result<Option<i64>> {
    match sqlx::query_scalar!(r#"SELECT max(version) AS "version" FROM public._sqlx_migrations WHERE success"#)
        .fetch_one(db)
        .await
    {
        Ok(v) => Ok(v),
        Err(e) if is_undefined_table(&e) => Ok(None),
        Err(e) => Err(StoreError::Db(e)),
    }
}

/// Whether the database is ready for this build: reachable and migrated to at least [`SCHEMA_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// Reachable, schema older than this build (the migrate Job has not run yet).
    SchemaBehind(Option<i64>),
    Unreachable,
}

pub async fn readiness(pool: &PgPool) -> Readiness {
    match schema_version(pool).await {
        Ok(Some(v)) if v >= SCHEMA_VERSION => Readiness::Ready,
        Ok(v) => Readiness::SchemaBehind(v),
        Err(_) => Readiness::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_numbered_from_one_without_gaps_and_the_last_is_the_schema_version() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(versions, (1..=SCHEMA_VERSION).collect::<Vec<_>>());
        for m in MIGRATOR.iter() {
            assert!(m.migration_type.is_up_migration(), "forward-only: {} is not an up migration", m.version);
        }
    }
}
