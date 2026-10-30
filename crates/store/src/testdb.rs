//! The test harness: an isolated database per test on a real PostgreSQL, provisioned with the three roles a
//! deployment has, so every test runs with production's privilege levels:
//!
//! - **owner** — owns the database and schema `hub` (what the `migrate` Job uses);
//! - **writer** — `SELECT, INSERT, UPDATE, DELETE` on the tables in `hub`, `SELECT` on `public`, no DDL
//!   (what `admin`, `worker`, `seed` and `export-snapshot` use);
//! - **readonly** — `SELECT` on `public` only, read-only transactions (what `public` uses).
//!
//! The grants mirror the platform database tier's provisioning (`owner`, `readonly`, and the `writer` record
//! type with its data schema; ARCHITECTURE.md §6 "Roles"). A deployment on another PostgreSQL needs the same
//! statements (README → Database roles).
//!
//! `TEST_DATABASE_URL` is a superuser URL (`scripts/test-postgres.sh` starts a TLS server and writes it).
//! Unset, a test prints `SKIPPED` and returns; CI sets `REQUIRE_TEST_DATABASE=1`, which turns a missing
//! server into a failure.

use sqlx::postgres::{PgConnection, PgPool};
use sqlx::{AssertSqlSafe, Connection, Executor};

use crate::config::{Access, DatabaseSettings};

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("the OS random source is available");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Whether a missing test database is a failure (CI) rather than a skip.
pub fn required() -> bool {
    std::env::var("REQUIRE_TEST_DATABASE").is_ok_and(|v| v == "1")
}

/// The superuser settings from `TEST_DATABASE_URL`, or `None` (after saying so) when unset and not required.
pub fn admin_settings() -> Option<DatabaseSettings> {
    match std::env::var("TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            Some(DatabaseSettings::parse(&url, true).expect("TEST_DATABASE_URL is a valid PostgreSQL URL"))
        }
        _ => {
            assert!(!required(), "REQUIRE_TEST_DATABASE=1 but TEST_DATABASE_URL is not set: tests would be skipped");
            eprintln!(
                "SKIPPED: TEST_DATABASE_URL is not set (these tests need a PostgreSQL; see README -> Development)"
            );
            None
        }
    }
}

/// Whether the provisioning creates the data schema `hub` (as the tier's writer record does), or leaves it
/// to the first migration (a server provisioned without that step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSchema {
    Provisioned,
    LeftToMigration,
}

/// An isolated, provisioned database.
pub struct TestDb {
    admin: DatabaseSettings,
    admin_password: String,
    pub name: String,
    pub owner: DatabaseSettings,
    pub writer: DatabaseSettings,
    pub readonly: DatabaseSettings,
}

async fn admin_conn(admin: &DatabaseSettings, password: &str, db: &str) -> PgConnection {
    let s = admin.with_credentials(admin.user(), password, db);
    PgConnection::connect_with(&s.connect_options().expect("admin options")).await.expect("connect as the test admin")
}

async fn exec(c: &mut PgConnection, sql: String) {
    c.execute(AssertSqlSafe(sql.clone())).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
}

impl TestDb {
    /// A fresh database with its three roles; `None` when no test database is configured.
    pub async fn create() -> Option<TestDb> {
        Some(Self::create_with(admin_settings()?, DataSchema::Provisioned).await)
    }

    /// Like [`TestDb::create`], choosing how the data schema comes to exist.
    pub async fn create_variant(schema: DataSchema) -> Option<TestDb> {
        Some(Self::create_with(admin_settings()?, schema).await)
    }

    async fn create_with(admin: DatabaseSettings, schema: DataSchema) -> TestDb {
        // The admin password lives only inside the URL; read it back once.
        let admin_password = url::Url::parse(admin.url_unredacted())
            .ok()
            .and_then(|u| u.password().map(percent_decode))
            .expect("TEST_DATABASE_URL has a password");
        let tag = random_hex(6);
        let name = format!("ch_t_{tag}");
        let (own, app, ro) = (format!("{name}_own"), format!("{name}_app"), format!("{name}_ro"));
        let (own_pw, app_pw, ro_pw) = (random_hex(16), random_hex(16), random_hex(16));
        // All identifiers and passwords above are generated hex: safe to interpolate.
        let mut c = admin_conn(&admin, &admin_password, "postgres").await;
        for (role, pw, limit) in [(&own, &own_pw, 4), (&app, &app_pw, 12), (&ro, &ro_pw, 10)] {
            exec(
                &mut c,
                format!(
                    "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS \
                     CONNECTION LIMIT {limit} PASSWORD '{pw}'"
                ),
            )
            .await;
        }
        exec(&mut c, format!("ALTER ROLE {own} SET statement_timeout = '60s'")).await;
        exec(&mut c, format!("ALTER ROLE {app} SET statement_timeout = '30s'")).await;
        exec(&mut c, format!("ALTER ROLE {app} SET idle_in_transaction_session_timeout = '60s'")).await;
        exec(&mut c, format!("ALTER ROLE {ro} SET statement_timeout = '5s'")).await;
        exec(&mut c, format!("ALTER ROLE {ro} SET default_transaction_read_only = on")).await;
        exec(&mut c, format!("CREATE DATABASE {name} OWNER {own}")).await;
        exec(&mut c, format!("REVOKE ALL ON DATABASE {name} FROM PUBLIC")).await;
        exec(&mut c, format!("GRANT CONNECT, TEMPORARY, CREATE ON DATABASE {name} TO {own}")).await;
        exec(&mut c, format!("GRANT CONNECT ON DATABASE {name} TO {app}, {ro}")).await;
        let _ = c.close().await;

        let mut d = admin_conn(&admin, &admin_password, &name).await;
        // Schema public: views and the migrations table, readable by both application roles.
        exec(&mut d, "REVOKE ALL ON SCHEMA public FROM PUBLIC".to_owned()).await;
        exec(&mut d, format!("GRANT USAGE, CREATE ON SCHEMA public TO {own}")).await;
        exec(&mut d, format!("GRANT USAGE ON SCHEMA public TO {app}, {ro}")).await;
        exec(
            &mut d,
            format!("ALTER DEFAULT PRIVILEGES FOR ROLE {own} IN SCHEMA public GRANT SELECT ON TABLES TO {app}, {ro}"),
        )
        .await;
        if schema == DataSchema::Provisioned {
            // Schema hub: the data, DML for the writer only. No sequence grants: identity columns need none.
            exec(&mut d, format!("CREATE SCHEMA hub AUTHORIZATION {own}")).await;
            exec(&mut d, format!("GRANT USAGE ON SCHEMA hub TO {app}")).await;
            exec(
                &mut d,
                format!(
                    "ALTER DEFAULT PRIVILEGES FOR ROLE {own} IN SCHEMA hub GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {app}"
                ),
            )
            .await;
        }
        let _ = d.close().await;

        TestDb {
            owner: admin.with_credentials(&own, &own_pw, &name),
            writer: admin.with_credentials(&app, &app_pw, &name),
            readonly: admin.with_credentials(&ro, &ro_pw, &name),
            admin,
            admin_password,
            name,
        }
    }

    /// Create the database and apply every migration as the owner.
    pub async fn migrated() -> Option<TestDb> {
        let db = Self::create().await?;
        crate::migrate::run(&db.owner).await.expect("migrations apply");
        Some(db)
    }

    /// A pool for one of the roles.
    pub fn pool(&self, who: &DatabaseSettings, access: Access) -> PgPool {
        who.lazy_pool(4, access).expect("pool options")
    }

    /// A single connection as `who`.
    pub async fn conn(&self, who: &DatabaseSettings) -> PgConnection {
        PgConnection::connect_with(&who.connect_options().expect("options")).await.expect("connect")
    }

    /// A superuser connection to this database (to inspect or damage it on purpose).
    pub async fn admin(&self) -> PgConnection {
        admin_conn(&self.admin, &self.admin_password, &self.name).await
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| u8::from_str_radix(&s[i + 1..i + 3], 16).ok()).flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Drop a database made by [`TestDb`] and its three roles.
async fn drop_database(admin: &DatabaseSettings, password: &str, name: &str) {
    assert!(
        name.starts_with("ch_t_") && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "not a test database: {name}"
    );
    let mut c = admin_conn(admin, password, "postgres").await;
    let _ = c.execute(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))).await;
    for suffix in ["own", "app", "ro"] {
        let _ = c.execute(AssertSqlSafe(format!("DROP ROLE IF EXISTS {name}_{suffix}"))).await;
    }
    let _ = c.close().await;
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (admin, password, name) = (self.admin.clone(), self.admin_password.clone(), self.name.clone());
        // A separate thread and runtime: this can run while the test's own runtime is shutting down.
        let cleanup = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("cleanup runtime");
            rt.block_on(drop_database(&admin, &password, &name));
        });
        let _ = cleanup.join();
    }
}

/// `let db = require_db!();` — a migrated test database, or return (skipped) when none is configured.
#[macro_export]
macro_rules! require_db {
    () => {
        match $crate::testdb::TestDb::migrated().await {
            Some(db) => db,
            None => return,
        }
    };
}
