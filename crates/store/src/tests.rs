//! Database tests: migrations, role separation, constraints and the public views. Each test gets its own
//! database and the three production roles (`testdb`).

use std::collections::BTreeSet;

use content_hub_core::model::stored_enumerations;
use sqlx::postgres::PgConnection;
use sqlx::{AssertSqlSafe, Executor, Row};

use crate::config::Access;
use crate::migrate::{self, Readiness, SCHEMA_VERSION};
use crate::require_db;
use crate::testdb::{DataSchema, TestDb};

/// The SQLSTATE of a failed statement.
async fn code_of(c: &mut PgConnection, sql: &str) -> Option<String> {
    match c.execute(AssertSqlSafe(sql.to_owned())).await {
        Ok(_) => None,
        Err(e) => {
            Some(e.as_database_error().and_then(|d| d.code()).map(|c| c.into_owned()).unwrap_or_else(|| format!("{e}")))
        }
    }
}

async fn ok(c: &mut PgConnection, sql: &str) {
    c.execute(AssertSqlSafe(sql.to_owned())).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
}

const PERMISSION_DENIED: &str = "42501";
const CHECK_VIOLATION: &str = "23514";
const UNIQUE_VIOLATION: &str = "23505";
const FK_VIOLATION: &str = "23503";
const RESTRICT_VIOLATION: &str = "23001";

/// A minimal dataset as the writer: one phase, item, definition, target, post.
async fn seed_minimal(c: &mut PgConnection) {
    ok(
        c,
        "INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, public_name, visible_human, visible_bot)
           VALUES ('p1', 1, 'internal one', 'Public one', true, false)",
    )
    .await;
    ok(c, "INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, public_title, public_description, visible_human)
           SELECT id, 'i1', 'engineering', 'internal item', 'Public item', 'desc', true FROM hub.roadmap_phases WHERE key = 'p1'").await;
    ok(
        c,
        "INSERT INTO hub.platform_definitions (code, name, category, posting_mode, markup, enabled)
           VALUES ('OWNED_FEED', 'Feed', 'owned_channel', 'feed', 'markdown', true),
                  ('DISCORD_ANNOUNCEMENTS', 'Discord', 'social', 'webhook', 'discord_md', true)",
    )
    .await;
    ok(
        c,
        "INSERT INTO hub.platform_targets (platform_definition_id, name, enabled, secret_ref)
           SELECT id, 'main', true, NULL FROM hub.platform_definitions WHERE code = 'OWNED_FEED'",
    )
    .await;
    ok(c, "INSERT INTO hub.platform_targets (platform_definition_id, name, enabled, secret_ref)
           SELECT id, 'announcements', true, 'PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL' FROM hub.platform_definitions WHERE code = 'DISCORD_ANNOUNCEMENTS'").await;
    ok(c, "INSERT INTO hub.posts (title, body_markdown, status, created_by) VALUES ('Hello', 'Body', 'ready', 'test')")
        .await;
}

// ── migrations ───────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn migrations_apply_once_and_report_the_version() {
    let Some(db) = TestDb::create().await else { return };
    let pool = db.pool(&db.readonly, Access::ReadOnly);
    assert_eq!(migrate::readiness(&pool).await, Readiness::SchemaBehind(None));
    assert_eq!(migrate::run(&db.owner).await.expect("first run"), vec![1]);
    assert_eq!(migrate::run(&db.owner).await.expect("second run"), Vec::<i64>::new());
    assert_eq!(migrate::schema_version(&pool).await.expect("version"), Some(SCHEMA_VERSION));
    assert_eq!(migrate::readiness(&pool).await, Readiness::Ready);
}

#[tokio::test]
async fn racing_migrators_apply_each_migration_once() {
    let Some(db) = TestDb::create().await else { return };
    let (a, b) = tokio::join!(migrate::run(&db.owner), migrate::run(&db.owner));
    let mut applied = a.expect("a").into_iter().chain(b.expect("b")).collect::<Vec<_>>();
    applied.sort();
    assert_eq!(applied, vec![1], "exactly one of the two applied it");
    let mut admin = db.admin().await;
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations").fetch_one(&mut admin).await.expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_schema_from_a_newer_build_is_refused() {
    let db = require_db!();
    let mut admin = db.admin().await;
    ok(
        &mut admin,
        "INSERT INTO public._sqlx_migrations (version, description, success, checksum, execution_time)
                    VALUES (999, 'future', true, '\\x00', 0)",
    )
    .await;
    let e = migrate::run(&db.owner).await.expect_err("an older build must not run against a newer schema");
    assert!(e.to_string().contains("999"), "{e}");
}

#[tokio::test]
async fn an_edited_migration_is_refused() {
    let db = require_db!();
    let mut admin = db.admin().await;
    ok(&mut admin, "UPDATE public._sqlx_migrations SET checksum = '\\x00' WHERE version = 1").await;
    assert!(migrate::run(&db.owner).await.is_err());
}

#[tokio::test]
async fn the_first_migration_creates_the_data_schema_when_provisioning_did_not() {
    let Some(db) = TestDb::create_variant(DataSchema::LeftToMigration).await else { return };
    assert_eq!(migrate::run(&db.owner).await.expect("applies"), vec![1]);
    let mut owner = db.conn(&db.owner).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname = 'hub'")
        .fetch_one(&mut owner)
        .await
        .expect("count");
    assert!(n >= 15, "{n} tables in hub");
}

#[tokio::test]
async fn an_unreachable_database_is_reported_as_such() {
    let Some(db) = TestDb::create().await else { return };
    let wrong = db.readonly.with_credentials(db.readonly.user(), "wrong-password", &db.name);
    let pool = wrong.lazy_pool(1, Access::ReadOnly).expect("pool");
    assert_eq!(migrate::readiness(&pool).await, Readiness::Unreachable);
}

// ── roles ────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_readonly_role_reads_only_the_public_views() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    let mut r = db.conn(&db.readonly).await;
    for view in ["public_v1_phases", "public_v1_items", "public_v1_feed_entries", "public_v1_snapshots"] {
        ok(&mut r, &format!("SELECT * FROM public.{view}")).await;
    }
    ok(&mut r, "SELECT max(version) FROM public._sqlx_migrations").await;
    for table in ["roadmap_phases", "roadmap_items", "posts", "platform_targets", "audit_log", "settings", "users"] {
        assert_eq!(
            code_of(&mut r, &format!("SELECT * FROM hub.{table}")).await.as_deref(),
            Some(PERMISSION_DENIED),
            "{table}"
        );
    }
    // Sessions are read-only by default, and writes are refused by privilege even with that switched off
    // (the simple views are auto-updatable, so the privilege is what stops them).
    let ro: String = sqlx::query_scalar("SHOW default_transaction_read_only").fetch_one(&mut r).await.expect("show");
    assert_eq!(ro, "on");
    ok(&mut r, "SET default_transaction_read_only = off").await;
    for sql in [
        "UPDATE public.public_v1_phases SET name = 'x'",
        "INSERT INTO public.public_v1_snapshots (network) VALUES ('testnet')",
        "CREATE TABLE public.x (a int)",
        "CREATE TEMP TABLE x (a int)",
    ] {
        assert_eq!(code_of(&mut r, sql).await.as_deref(), Some(PERMISSION_DENIED), "{sql}");
    }
}

#[tokio::test]
async fn the_writer_changes_data_but_never_the_schema() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "UPDATE hub.posts SET title = 'Edited'").await;
    ok(&mut w, "SELECT * FROM public.public_v1_items").await;
    for ddl in [
        "CREATE TABLE hub.x (a int)",
        "CREATE TABLE public.x (a int)",
        "ALTER TABLE hub.posts ADD COLUMN x int",
        "DROP TABLE hub.settings",
        "TRUNCATE hub.posts CASCADE",
        "CREATE SCHEMA other",
    ] {
        assert_eq!(code_of(&mut w, ddl).await.as_deref(), Some(PERMISSION_DENIED), "{ddl}");
    }
    // The writer cannot rewrite migration history either.
    assert_eq!(code_of(&mut w, "DELETE FROM public._sqlx_migrations").await.as_deref(), Some(PERMISSION_DENIED));
}

#[tokio::test]
async fn audit_log_and_spend_ledger_are_append_only() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "INSERT INTO hub.audit_log (actor_subject, action, entity_type, entity_id) VALUES ('s', 'post.create', 'post', '1')").await;
    ok(
        &mut w,
        "INSERT INTO hub.spend_ledger (period_month, platform_definition_id, kind, amount_usd_micros)
                SELECT date '2026-10-01', id, 'reserve', 0 FROM hub.platform_definitions WHERE code = 'OWNED_FEED'",
    )
    .await;
    for sql in [
        "UPDATE hub.audit_log SET action = 'x.y'",
        "DELETE FROM hub.audit_log",
        "UPDATE hub.spend_ledger SET amount_usd_micros = 1",
        "DELETE FROM hub.spend_ledger",
    ] {
        assert_eq!(code_of(&mut w, sql).await.as_deref(), Some(PERMISSION_DENIED), "{sql}");
    }
    // The owner is refused by the trigger too (only dropping the trigger, a migration, could change that).
    let mut o = db.conn(&db.owner).await;
    assert_eq!(code_of(&mut o, "TRUNCATE hub.audit_log").await.as_deref(), Some(PERMISSION_DENIED));
}

// ── constraints ──────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn constraints_refuse_inconsistent_rows() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    let cases: &[(&str, &str)] = &[
        // A visible item always has public wording.
        ("INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, visible_bot)
          SELECT id, 'i2', 'other', 'secret plan', true FROM hub.roadmap_phases WHERE key = 'p1'", CHECK_VIOLATION),
        // Keys and codes have their shapes.
        ("INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, public_name) VALUES ('Bad Key', 9, 'a', 'b')", CHECK_VIOLATION),
        ("INSERT INTO hub.platform_definitions (code, name, category, posting_mode) VALUES ('bad', 'n', 'social', 'api')", CHECK_VIOLATION),
        // A limit needs a counting method.
        ("INSERT INTO hub.platform_definitions (code, name, category, posting_mode, primary_limit) VALUES ('NOCOUNT', 'n', 'social', 'api', 10)", CHECK_VIOLATION),
        // A credential VALUE is refused where only its NAME belongs.
        ("INSERT INTO hub.platform_targets (platform_definition_id, name, secret_ref)
          SELECT id, 'leak', 'https://discord.com/api/webhooks/1/abc' FROM hub.platform_definitions WHERE code = 'DISCORD_ANNOUNCEMENTS'", CHECK_VIOLATION),
        // ... and a name outside the PLATFORM_ prefix (another process setting) is refused too.
        ("INSERT INTO hub.platform_targets (platform_definition_id, name, secret_ref)
          SELECT id, 'db', 'DATABASE_URL' FROM hub.platform_definitions WHERE code = 'DISCORD_ANNOUNCEMENTS'", CHECK_VIOLATION),
        // Two phases cannot share a position (checked at commit; this statement runs in its own transaction).
        ("INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, public_name) VALUES ('p2', 1, 'a', 'b')", UNIQUE_VIOLATION),
        // Blocked needs a reason; done needs a completion time.
        ("INSERT INTO hub.tasks (key, task_type, title, status) VALUES ('t1', 'OTHER', 't', 'blocked')", CHECK_VIOLATION),
        ("INSERT INTO hub.tasks (key, task_type, title, status) VALUES ('t2', 'OTHER', 't', 'done')", CHECK_VIOLATION),
        // A post needs a title.
        ("INSERT INTO hub.posts (title, body_markdown, created_by) VALUES ('', 'b', 's')", CHECK_VIOLATION),
    ];
    for (sql, want) in cases {
        assert_eq!(code_of(&mut w, sql).await.as_deref(), Some(*want), "{sql}");
    }
}

#[tokio::test]
async fn an_instance_is_live_at_most_once_per_target_and_keeps_its_state_consistent() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    let insert = "INSERT INTO hub.platform_instances (post_id, platform_target_id, status)
                  SELECT p.id, t.id, 'planned' FROM hub.posts p, hub.platform_targets t WHERE t.name = 'announcements'";
    ok(&mut w, insert).await;
    assert_eq!(code_of(&mut w, insert).await.as_deref(), Some(UNIQUE_VIOLATION), "a second live instance");
    ok(&mut w, "UPDATE hub.platform_instances SET status = 'cancelled'").await;
    ok(&mut w, insert).await; // a cancelled one does not count
    let cases: &[&str] = &[
        "UPDATE hub.platform_instances SET status = 'posted' WHERE status = 'planned'",
        "UPDATE hub.platform_instances SET status = 'sending' WHERE status = 'planned'",
        "UPDATE hub.platform_instances SET sending_started_at = now() WHERE status = 'planned'",
        // Nothing is queued or sent without a validated payload, and a payload is all or nothing.
        "UPDATE hub.platform_instances SET status = 'queued' WHERE status = 'planned'",
        "UPDATE hub.platform_instances SET status = 'sending', sending_started_at = now() WHERE status = 'planned'",
        "UPDATE hub.platform_instances SET payload_body = 'b', payload_sha256 = repeat('a', 64) WHERE status = 'planned'",
    ];
    for sql in cases {
        assert_eq!(code_of(&mut w, sql).await.as_deref(), Some(CHECK_VIOLATION), "{sql}");
    }
    ok(
        &mut w,
        "UPDATE hub.platform_instances SET post_revision = 1, payload_title = 't', payload_body = 'b',
                payload_sha256 = repeat('a', 64) WHERE status = 'planned'",
    )
    .await;
    ok(
        &mut w,
        "UPDATE hub.platform_instances SET status = 'sending', sending_started_at = now() WHERE status = 'planned'",
    )
    .await;
    // Idempotency keys are unique and generated.
    let n: i64 = sqlx::query_scalar("SELECT count(DISTINCT idempotency_key) FROM hub.platform_instances")
        .fetch_one(&mut w)
        .await
        .expect("count");
    assert_eq!(n, 2);
    // A delivery attempt's dry-run flag agrees with its outcome, and an outcome comes with a finish time.
    let attempt = |outcome: &str, dry: bool, finished: bool| {
        format!(
            "INSERT INTO hub.delivery_attempts (platform_instance_id, outcome, dry_run, finished_at, rendered_payload_sha256)
             SELECT id, {}, {dry}, {}, repeat('a', 64) FROM hub.platform_instances WHERE status = 'sending'",
            if outcome.is_empty() { "NULL".to_owned() } else { format!("'{outcome}'") },
            if finished { "now()" } else { "NULL" }
        )
    };
    ok(&mut w, &attempt("dry_run", true, true)).await;
    ok(&mut w, &attempt("", false, false)).await;
    assert_eq!(code_of(&mut w, &attempt("success", true, true)).await.as_deref(), Some(CHECK_VIOLATION));
    assert_eq!(code_of(&mut w, &attempt("success", false, false)).await.as_deref(), Some(CHECK_VIOLATION));
}

#[tokio::test]
async fn references_must_agree() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "INSERT INTO hub.tasks (key, task_type, title) VALUES ('t1', 'SOCIAL_POST', 't')").await;
    // A task's target must be on the platform it names.
    let mismatched = "INSERT INTO hub.task_platforms (task_id, platform_definition_id, platform_target_id)
                      SELECT t.id, d.id, pt.id FROM hub.tasks t, hub.platform_definitions d, hub.platform_targets pt
                      WHERE d.code = 'OWNED_FEED' AND pt.name = 'announcements'";
    assert_eq!(code_of(&mut w, mismatched).await.as_deref(), Some(FK_VIOLATION));
    // The same platform twice without a target is one row (NULLS NOT DISTINCT).
    let untargeted = "INSERT INTO hub.task_platforms (task_id, platform_definition_id)
                      SELECT t.id, d.id FROM hub.tasks t, hub.platform_definitions d WHERE d.code = 'OWNED_FEED'";
    ok(&mut w, untargeted).await;
    assert_eq!(code_of(&mut w, untargeted).await.as_deref(), Some(UNIQUE_VIOLATION));
    // A task cannot depend on itself; a link carries exactly the column of its kind.
    assert_eq!(
        code_of(&mut w, "INSERT INTO hub.task_dependencies SELECT id, id FROM hub.tasks").await.as_deref(),
        Some(CHECK_VIOLATION)
    );
    assert_eq!(
        code_of(
            &mut w,
            "INSERT INTO hub.task_links (task_id, link_kind, url) SELECT id, 'post', 'https://x.example' FROM hub.tasks"
        )
        .await
        .as_deref(),
        Some(CHECK_VIOLATION)
    );
    assert_eq!(
        code_of(&mut w, "INSERT INTO hub.task_links (task_id, link_kind, url) SELECT id, 'external', 'http://x.example' FROM hub.tasks")
            .await
            .as_deref(),
        Some(CHECK_VIOLATION)
    );
    ok(&mut w, "INSERT INTO hub.task_links (task_id, link_kind, url) SELECT id, 'external', 'https://x.example' FROM hub.tasks").await;
    // A post with instances cannot be deleted (history is kept).
    ok(&mut w, "INSERT INTO hub.platform_instances (post_id, platform_target_id) SELECT p.id, t.id FROM hub.posts p, hub.platform_targets t WHERE t.name = 'main'").await;
    assert_eq!(code_of(&mut w, "DELETE FROM hub.posts").await.as_deref(), Some(RESTRICT_VIOLATION));
}

#[tokio::test]
async fn updated_at_moves_on_update() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "UPDATE hub.posts SET updated_at = now() - interval '1 day'").await;
    ok(&mut w, "UPDATE hub.posts SET title = 'Again'").await;
    let fresh: bool = sqlx::query_scalar("SELECT updated_at > now() - interval '1 minute' FROM hub.posts")
        .fetch_one(&mut w)
        .await
        .expect("query");
    assert!(fresh);
}

// ── enumerations: Rust and SQL agree ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_check_constrained_enumeration_matches_the_rust_enum() {
    let db = require_db!();
    let mut o = db.conn(&db.owner).await;
    for (table, column, values) in stored_enumerations() {
        // The CHECK constraint on exactly this column whose definition is an IN-list (= ANY (ARRAY[...])).
        let defs: Vec<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(c.oid)
               FROM pg_constraint c
               JOIN pg_class t ON t.oid = c.conrelid
               JOIN pg_namespace n ON n.oid = t.relnamespace
              WHERE n.nspname = 'hub' AND t.relname = $1 AND c.contype = 'c'
                AND array_length(c.conkey, 1) = 1
                AND c.conkey[1] = (SELECT attnum FROM pg_attribute WHERE attrelid = t.oid AND attname = $2)
                AND pg_get_constraintdef(c.oid) LIKE '%ANY%'",
        )
        .bind(table)
        .bind(column)
        .fetch_all(&mut o)
        .await
        .expect("constraints");
        assert_eq!(defs.len(), 1, "{table}.{column}: {defs:?}");
        let sql: BTreeSet<String> =
            defs[0].split('\'').enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, s)| s.to_owned()).collect();
        let rust: BTreeSet<String> = values.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(sql, rust, "{table}.{column}");
    }
}

// ── public views ─────────────────────────────────────────────────────────────────────────────────────

/// The exact columns each public view exposes. Adding a column is a deliberate exposure decision: change
/// this list, the public DTOs and ARCHITECTURE.md §5 together.
const PUBLIC_VIEW_COLUMNS: &[(&str, &[&str])] = &[
    (
        "public_v1_phases",
        &[
            "public_id",
            "sort_order",
            "name",
            "description",
            "window_start",
            "window_end",
            "visible_human",
            "visible_bot",
            "updated_at",
        ],
    ),
    (
        "public_v1_items",
        &[
            "public_id",
            "phase_public_id",
            "title",
            "description",
            "status",
            "due_at",
            "visible_human",
            "visible_bot",
            "updated_at",
        ],
    ),
    ("public_v1_feed_entries", &["public_id", "title", "body_markdown", "published_at", "updated_at"]),
    (
        "public_v1_snapshots",
        &[
            "network",
            "generated_at",
            "schema_version",
            "sha256",
            "byte_size",
            "signature",
            "signer_address",
            "blob_id",
            "object_id",
            "epochs",
            "expires_at",
        ],
    ),
];

#[tokio::test]
async fn public_view_columns_are_exactly_the_documented_ones() {
    let db = require_db!();
    let mut r = db.conn(&db.readonly).await;
    let views: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.views WHERE table_schema = 'public' ORDER BY 1",
    )
    .fetch_all(&mut r)
    .await
    .expect("views");
    let mut want: Vec<String> = PUBLIC_VIEW_COLUMNS.iter().map(|(v, _)| (*v).to_owned()).collect();
    want.sort();
    assert_eq!(views, want, "a view was added or removed");
    for (view, columns) in PUBLIC_VIEW_COLUMNS {
        let got: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(view)
        .fetch_all(&mut r)
        .await
        .expect("columns");
        assert_eq!(got, columns.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>(), "{view}");
        for internal in [
            "key",
            "internal_name",
            "internal_title",
            "internal_description",
            "secret_ref",
            "created_by",
            "id",
            "planned_at",
        ] {
            assert!(!got.iter().any(|c| c == internal), "{view} exposes {internal}");
        }
    }
}

#[tokio::test]
async fn visibility_follows_both_the_item_and_its_phase() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    // A hidden phase with a "visible" item: the item must not surface (it would reveal the hidden phase).
    ok(&mut w, "INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, public_name) VALUES ('hidden', 2, 'secret phase', 'Later')").await;
    ok(&mut w, "INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, public_title, public_description, visible_human, visible_bot)
                SELECT id, 'i-hidden', 'other', 'secret', 'Shown?', 'd', true, true FROM hub.roadmap_phases WHERE key = 'hidden'").await;
    // A bot-only item in a human-only phase is visible to nobody.
    ok(&mut w, "INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, public_title, public_description, visible_bot)
                SELECT id, 'i-bot', 'other', 'internal', 'Bots', 'd', true FROM hub.roadmap_phases WHERE key = 'p1'").await;
    let mut r = db.conn(&db.readonly).await;
    let phases: Vec<String> = sqlx::query_scalar("SELECT name FROM public.public_v1_phases ORDER BY sort_order")
        .fetch_all(&mut r)
        .await
        .expect("phases");
    assert_eq!(phases, vec!["Public one"]);
    let items = sqlx::query("SELECT title, visible_human, visible_bot FROM public.public_v1_items ORDER BY title")
        .fetch_all(&mut r)
        .await
        .expect("items");
    let items: Vec<(String, bool, bool)> = items.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(items, vec![("Public item".to_owned(), true, false)]);
}

#[tokio::test]
async fn only_the_public_due_date_is_published() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "UPDATE hub.roadmap_items SET due_at = '2027-01-01T00:00:00Z', planned_at = '2026-12-01T00:00:00Z'")
        .await;
    let mut r = db.conn(&db.readonly).await;
    let hidden: Option<String> =
        sqlx::query_scalar("SELECT due_at::text FROM public.public_v1_items").fetch_one(&mut r).await.expect("item");
    assert_eq!(hidden, None, "internal dates must not appear");
    ok(&mut w, "UPDATE hub.roadmap_items SET public_due_at = '2027-02-01T12:00:00Z'").await;
    let shown: bool =
        sqlx::query_scalar("SELECT due_at = timestamptz '2027-02-01T12:00:00Z' FROM public.public_v1_items")
            .fetch_one(&mut r)
            .await
            .expect("item");
    assert!(shown);
}

#[tokio::test]
async fn the_feed_lists_only_posted_entries_on_enabled_feed_targets() {
    let db = require_db!();
    let mut w = db.conn(&db.writer).await;
    seed_minimal(&mut w).await;
    ok(&mut w, "INSERT INTO hub.posts (title, body_markdown, status, created_by) VALUES ('Draft', 'x', 'draft', 't')")
        .await;
    // Posted to the feed: listed. Posted to Discord: not a feed entry. A posted draft: not listed.
    ok(&mut w, "INSERT INTO hub.platform_instances (post_id, platform_target_id, status, posted_at, override_title)
                SELECT p.id, t.id, 'posted', now(), 'Feed title' FROM hub.posts p, hub.platform_targets t WHERE p.title = 'Hello' AND t.name = 'main'").await;
    ok(&mut w, "INSERT INTO hub.platform_instances (post_id, platform_target_id, status, posted_at)
                SELECT p.id, t.id, 'posted', now() FROM hub.posts p, hub.platform_targets t WHERE p.title = 'Hello' AND t.name = 'announcements'").await;
    ok(&mut w, "INSERT INTO hub.platform_instances (post_id, platform_target_id, status, posted_at)
                SELECT p.id, t.id, 'posted', now() FROM hub.posts p, hub.platform_targets t WHERE p.title = 'Draft' AND t.name = 'main'").await;
    let mut r = db.conn(&db.readonly).await;
    let titles: Vec<String> =
        sqlx::query_scalar("SELECT title FROM public.public_v1_feed_entries").fetch_all(&mut r).await.expect("feed");
    assert_eq!(titles, vec!["Feed title"]);
    // Disabling the target removes it.
    ok(&mut w, "UPDATE hub.platform_targets SET enabled = false WHERE name = 'main'").await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM public.public_v1_feed_entries")
        .fetch_one(&mut r)
        .await
        .expect("count");
    assert_eq!(n, 0);
}
