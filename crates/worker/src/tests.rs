//! The worker against a real PostgreSQL (the production roles; it runs as the WRITER) and local mocks of the
//! platforms. Nothing here reaches a real platform.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use content_hub_core::delivery::RetryPolicy;
use content_hub_publish::Http;
use content_hub_store::Access;
use content_hub_store::repo::instances::{self, NewInstance, Payload};
use content_hub_store::repo::seed::{SeedReport, apply_definitions};
use content_hub_store::repo::{spend, targets};
use content_hub_store::testdb::TestDb;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use super::*;

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────────────────

struct Env {
    _db: TestDb,
    pool: PgPool,
}

async fn env() -> Option<Env> {
    let db = TestDb::migrated().await?;
    let pool = db.pool(&db.writer, Access::ReadWrite);
    let mut c = pool.acquire().await.expect("conn");
    let defs = content_hub_core::platforms::generic().expect("definitions");
    apply_definitions(&mut c, &defs, &mut SeedReport::default()).await.expect("definitions");
    Some(Env { _db: db, pool })
}

macro_rules! need {
    () => {
        match env().await {
            Some(e) => e,
            None => return,
        }
    };
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned())).execute(pool).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
}

async fn scalar<T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin>(
    pool: &PgPool,
    sql: &str,
) -> T {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .get(0)
}

async fn post(pool: &PgPool) -> i64 {
    scalar(pool, "INSERT INTO hub.posts (title, body_markdown, status, created_by) VALUES ('T', 'Hello', 'ready', 't') RETURNING id").await
}

/// A target on `code`; returns its id.
async fn target(
    pool: &PgPool,
    code: &str,
    live: bool,
    sandbox: bool,
    secret_ref: Option<&str>,
    ext: Option<&str>,
    config: Value,
) -> i64 {
    let mut c = pool.acquire().await.expect("conn");
    let def: i64 = scalar(pool, &format!("SELECT id FROM hub.platform_definitions WHERE code = '{code}'")).await;
    let f = targets::TargetFields {
        platform_definition_id: def,
        name: format!("t-{}", uuid::Uuid::new_v4().simple()),
        external_ref: ext.map(str::to_owned),
        config,
        secret_ref: secret_ref.map(str::to_owned),
        is_sandbox: sandbox,
        enabled: true,
        live,
    };
    targets::insert(&mut c, &f).await.expect("target")
}

/// A queued instance with a payload, due now; `cost` is reserved.
async fn queued(pool: &PgPool, target_id: i64, cost: i64) -> i64 {
    let p = post(pool).await;
    let mut c = pool.acquire().await.expect("conn");
    let payload = Payload {
        post_revision: 1,
        title: "T".into(),
        body: "Hello https://example.org".into(),
        sha256: "a".repeat(64),
    };
    let i = instances::insert(
        &mut c,
        &NewInstance {
            post_id: p,
            platform_target_id: target_id,
            override_title: None,
            override_body: None,
            status: "queued",
            scheduled_at: Some(Utc::now() - chrono::Duration::seconds(1)),
            cost_estimate_usd_micros: cost,
            payload: &payload,
            last_error: None,
        },
    )
    .await
    .expect("instance");
    if cost > 0 {
        spend::reserve(&mut c, i.id, cost, "test").await.expect("reserve");
    }
    i.id
}

async fn status(pool: &PgPool, id: i64) -> String {
    scalar(pool, &format!("SELECT status FROM hub.platform_instances WHERE id = {id}")).await
}

fn worker(pool: &PgPool, base: Option<&str>, cfg: WorkerConfig, secrets: &[(&str, &str)]) -> Worker {
    let map: BTreeMap<String, String> = secrets.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    Worker {
        db: pool.clone(),
        http: Http::unchecked_for_tests(Duration::from_millis(800)),
        cfg,
        secrets: Arc::new(move |n: &str| map.get(n).cloned()),
        base_override: base.map(str::to_owned),
    }
}

fn live_cfg() -> WorkerConfig {
    WorkerConfig { live: true, production: true, ..WorkerConfig::default() }
}

// ── a mock platform ──────────────────────────────────────────────────────────────────────────────────────

/// A scripted reply for a path: status, body, delay.
type Reply = dyn Fn(&str) -> (u16, String, Duration) + Send + Sync;

#[derive(Clone)]
struct Mock {
    log: Arc<Mutex<Vec<(String, Value)>>>,
    /// The instance status seen while a request was being handled (proves `sending` was committed first).
    seen_status: Arc<Mutex<Vec<String>>>,
    pool: PgPool,
    reply: Arc<Reply>,
}

async fn handle(State(m): State<Mock>, uri: Uri, body: Bytes) -> impl IntoResponse {
    let path = uri.path().to_owned();
    m.log.lock().expect("log").push((path.clone(), serde_json::from_slice(&body).unwrap_or(Value::Null)));
    if let Ok(s) = sqlx::query_scalar::<_, String>(
        "SELECT status FROM hub.platform_instances WHERE status IN ('sending', 'queued') ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&m.pool)
    .await
    {
        m.seen_status.lock().expect("seen").push(s);
    }
    let (code, text, delay) = (m.reply)(&path);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    (StatusCode::from_u16(code).expect("code"), [("content-type", "application/json")], text)
}

async fn mock(
    pool: &PgPool,
    reply: impl Fn(&str) -> (u16, String, Duration) + Send + Sync + 'static,
) -> (String, Mock) {
    let m = Mock { log: Arc::default(), seen_status: Arc::default(), pool: pool.clone(), reply: Arc::new(reply) };
    let app = Router::new().fallback(handle).with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(l, app).await.expect("serve") });
    (format!("http://{addr}"), m)
}

const DISCORD_OK: &str = r#"{"id":"1300000000000000001","channel_id":"1200000000000000002"}"#;
const HOOK_REF: &str = "PLATFORM_DISCORD_TEST_WEBHOOK_URL";

// ── tests ────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_dry_run_records_the_redacted_request_and_hands_over_to_a_person() {
    let e = need!();
    let (base, m) = mock(&e.pool, |_| (200, DISCORD_OK.into(), Duration::ZERO)).await;
    let hook = format!("{base}/api/webhooks/9/DRYRUNTOKEN");
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", false, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    // The worker itself is live; the target is not.
    let w = worker(&e.pool, Some(&base), live_cfg(), &[(HOOK_REF, &hook)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::DryRun(id));
    assert_eq!(status(&e.pool, id).await, "manual_required");
    let (outcome, dry, request): (String, bool, Value) = {
        let r = sqlx::query(
            "SELECT outcome, dry_run, request_redacted FROM hub.delivery_attempts WHERE platform_instance_id = $1",
        )
        .bind(id)
        .fetch_one(&e.pool)
        .await
        .expect("attempt");
        (r.get(0), r.get(1), r.get(2))
    };
    assert_eq!((outcome.as_str(), dry), ("dry_run", true));
    assert_eq!(request["url"], format!("{base}/api/webhooks/9/[redacted]?wait=true"));
    assert_eq!(request["body"]["allowed_mentions"], json!({"parse": []}));
    assert!(!request.to_string().contains("DRYRUNTOKEN"));
    assert!(m.log.lock().expect("log").is_empty(), "a dry run sends nothing");
    let why: String = scalar(&e.pool, &format!("SELECT last_error FROM hub.platform_instances WHERE id = {id}")).await;
    assert!(why.contains("dry run") && why.contains("not live"), "{why}");
    // With PUBLISHING_LIVE off, even a live target is a dry run.
    let t2 = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id2 = queued(&e.pool, t2, 0).await;
    let off = worker(&e.pool, Some(&base), WorkerConfig::default(), &[(HOOK_REF, &hook)]);
    assert_eq!(off.tick().await.expect("tick"), Tick::DryRun(id2));
    assert!(m.log.lock().expect("log").is_empty());
}

#[tokio::test]
async fn sending_is_committed_before_the_call_and_the_post_is_recorded() {
    let e = need!();
    let (base, m) = mock(&e.pool, |_| (200, DISCORD_OK.into(), Duration::ZERO)).await;
    let hook = format!("{base}/api/webhooks/9/LIVETOKEN");
    let t = target(
        &e.pool,
        "DISCORD_ANNOUNCEMENTS",
        true,
        false,
        Some(HOOK_REF),
        None,
        json!({"guild_id": "1100000000000000003"}),
    )
    .await;
    let id = queued(&e.pool, t, 0).await;
    let w = worker(&e.pool, Some(&base), live_cfg(), &[(HOOK_REF, &hook)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Posted));
    assert_eq!(
        m.seen_status.lock().expect("seen").as_slice(),
        ["sending"],
        "the platform was called only after `sending` was committed"
    );
    assert_eq!(status(&e.pool, id).await, "posted");
    let url: String =
        scalar(&e.pool, &format!("SELECT url_or_external_id FROM hub.platform_instances WHERE id = {id}")).await;
    assert_eq!(url, "https://discord.com/channels/1100000000000000003/1200000000000000002/1300000000000000001");
    let (outcome, http_status, sha): (String, i32, String) = {
        let r = sqlx::query("SELECT outcome, http_status, rendered_payload_sha256 FROM hub.delivery_attempts WHERE platform_instance_id = $1")
            .bind(id)
            .fetch_one(&e.pool)
            .await
            .expect("attempt");
        (r.get(0), r.get(1), r.get(2))
    };
    assert_eq!((outcome.as_str(), http_status, sha), ("success", 200, "a".repeat(64)));
    // Exactly the stored payload was sent.
    assert_eq!(m.log.lock().expect("log")[0].1["content"], "Hello https://example.org");
    // Nothing more is due.
    assert_eq!(w.tick().await.expect("tick"), Tick::Idle);
}

#[tokio::test]
async fn retries_back_off_and_unknown_is_never_claimed_again() {
    let e = need!();
    let (base, m) =
        mock(&e.pool, |_| (429, r#"{"message":"slow down","retry_after":2.0,"global":false}"#.into(), Duration::ZERO))
            .await;
    let hook = format!("{base}/api/webhooks/9/tok");
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    let w = worker(&e.pool, Some(&base), live_cfg(), &[(HOOK_REF, &hook)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Queued));
    let wait: f64 = scalar(
        &e.pool,
        &format!(
            "SELECT extract(epoch FROM next_attempt_at - now())::float8 FROM hub.platform_instances WHERE id = {id}"
        ),
    )
    .await;
    assert!(wait >= 25.0, "the backoff (30 s) wins over the 2 s hint: {wait}");
    assert_eq!(w.tick().await.expect("tick"), Tick::Idle, "not due yet");
    let n: i64 = scalar(
        &e.pool,
        &format!(
            "SELECT count(*) FROM hub.delivery_attempts WHERE platform_instance_id = {id} AND outcome = 'rate_limited'"
        ),
    )
    .await;
    assert_eq!(n, 1);

    // A timeout after the request was sent: unknown, and never claimed again.
    let (base2, _) = mock(&e.pool, |_| (200, DISCORD_OK.into(), Duration::from_secs(3))).await;
    let hook2 = format!("{base2}/api/webhooks/9/tok");
    let t2 = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id2 = queued(&e.pool, t2, 0).await;
    let w2 = worker(&e.pool, Some(&base2), live_cfg(), &[(HOOK_REF, &hook2)]);
    assert_eq!(w2.tick().await.expect("tick"), Tick::Sent(id2, InstanceStatus::Unknown));
    assert_eq!(status(&e.pool, id2).await, "unknown");
    // The rate-limited one is not due yet, and the unknown one is never claimed.
    assert_eq!(w2.tick().await.expect("tick"), Tick::Idle, "unknown is not queued");
    drop(m);
}

#[tokio::test]
async fn attempts_stop_at_the_bound() {
    let e = need!();
    let (base, _) = mock(&e.pool, |_| (429, r#"{"retry_after":0.0}"#.into(), Duration::ZERO)).await;
    let hook = format!("{base}/api/webhooks/9/tok");
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    let cfg = WorkerConfig { retry: RetryPolicy { max_attempts: 2, ..RetryPolicy::default() }, ..live_cfg() };
    let w = worker(&e.pool, Some(&base), cfg, &[(HOOK_REF, &hook)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Queued));
    exec(&e.pool, "UPDATE hub.platform_instances SET next_attempt_at = now() - interval '1 second'").await;
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Failed));
    let why: String = scalar(&e.pool, &format!("SELECT last_error FROM hub.platform_instances WHERE id = {id}")).await;
    assert!(why.starts_with("gave up after 2 attempts"), "{why}");
}

#[tokio::test]
async fn spend_is_settled_when_sent_and_released_when_refused() {
    let e = need!();
    exec(&e.pool, "UPDATE hub.platform_definitions SET enabled = true WHERE code = 'X'").await;
    let cred = r#"{"consumer_key":"ck","consumer_secret":"cs","access_token":"at","access_token_secret":"ats"}"#;
    let xref = "PLATFORM_X_TEST";
    let month_usage = |pool: PgPool| async move {
        scalar::<i64>(&pool, "SELECT COALESCE(sum(CASE WHEN kind = 'release' THEN -amount_usd_micros ELSE amount_usd_micros END), 0)::bigint FROM hub.spend_ledger").await
    };
    // Refused (duplicate content): the reservation is released, nothing is spent.
    let (base, _) =
        mock(&e.pool, |_| (403, r#"{"detail":"duplicate content","status":403}"#.into(), Duration::ZERO)).await;
    let t = target(&e.pool, "X", true, false, Some(xref), None, json!({})).await;
    let id = queued(&e.pool, t, 200_000).await;
    assert_eq!(month_usage(e.pool.clone()).await, 200_000, "reserved before sending");
    let w = worker(&e.pool, Some(&base), live_cfg(), &[(xref, cred)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Failed));
    assert_eq!(month_usage(e.pool.clone()).await, 0);
    // Sent: the cost is settled (and the reservation released): usage equals the cost once.
    let (base2, _) = mock(&e.pool, |_| (201, r#"{"data":{"id":"1","text":"x"}}"#.into(), Duration::ZERO)).await;
    let t2 = target(&e.pool, "X", true, false, Some(xref), None, json!({})).await;
    let id2 = queued(&e.pool, t2, 200_000).await;
    let w2 = worker(&e.pool, Some(&base2), live_cfg(), &[(xref, cred)]);
    assert_eq!(w2.tick().await.expect("tick"), Tick::Sent(id2, InstanceStatus::Posted));
    assert_eq!(month_usage(e.pool.clone()).await, 200_000);
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM hub.spend_ledger WHERE platform_instance_id = $1 ORDER BY id")
            .bind(id2)
            .fetch_all(&e.pool)
            .await
            .expect("ledger");
    assert_eq!(kinds, ["reserve", "settle", "release"]);
}

#[tokio::test]
async fn the_janitor_turns_stale_sends_into_unknown_and_late_results_are_kept_for_the_operator() {
    let e = need!();
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    let mut c = e.pool.acquire().await.expect("conn");
    let attempt = q::begin_send(&mut c, id, &"a".repeat(64)).await.expect("begin");
    exec(
        &e.pool,
        &format!(
            "UPDATE hub.platform_instances SET sending_started_at = now() - interval '11 minutes' WHERE id = {id}"
        ),
    )
    .await;
    let w = worker(&e.pool, None, live_cfg(), &[]);
    assert_eq!(w.janitor().await.expect("janitor"), vec![id]);
    assert_eq!(status(&e.pool, id).await, "unknown");
    let open: i64 = scalar(
        &e.pool,
        &format!(
            "SELECT count(*) FROM hub.delivery_attempts WHERE platform_instance_id = {id} AND finished_at IS NULL"
        ),
    )
    .await;
    assert_eq!(open, 0);
    // The send then finishes: the instance stays unknown; the result is in the audit log.
    let d = delivery::decide(
        &delivery::Sent::Posted { url_or_external_id: "https://late".into() },
        1,
        &RetryPolicy::default(),
        0.0,
    );
    let report = content_hub_publish::Report {
        sent: delivery::Sent::Posted { url_or_external_id: "https://late".into() },
        http_status: Some(200),
        excerpt: None,
        request: json!({}),
    };
    let f = q::Finish {
        to: d.to.as_str(),
        outcome: "success",
        url_or_external_id: Some("https://late"),
        next_attempt_at: None,
        last_error: None,
        http_status: report.http_status.map(i32::from),
        response_excerpt: None,
        request_redacted: &report.request,
    };
    assert!(!q::finish(&mut c, id, attempt, &f).await.expect("finish"), "the janitor's move stands");
    assert_eq!(status(&e.pool, id).await, "unknown");
}

#[tokio::test]
async fn claims_are_exclusive() {
    let e = need!();
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let a = queued(&e.pool, t, 0).await;
    let t2 = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let b = queued(&e.pool, t2, 0).await;
    let mut tx1 = e.pool.begin().await.expect("tx1");
    let mut tx2 = e.pool.begin().await.expect("tx2");
    let mut tx3 = e.pool.begin().await.expect("tx3");
    let c1 = q::next_due(&mut tx1).await.expect("claim").expect("first");
    let c2 = q::next_due(&mut tx2).await.expect("claim").expect("second");
    assert_ne!(c1.id, c2.id, "SKIP LOCKED hands each claimer a different row");
    let mut ids = [c1.id, c2.id];
    ids.sort_unstable();
    assert_eq!(ids, [a, b]);
    assert!(q::next_due(&mut tx3).await.expect("claim").is_none(), "both are held");
}

#[tokio::test]
async fn stops_daily_limits_the_stop_switch_and_sandbox_only_outside_production() {
    let e = need!();
    let (base, m) = mock(&e.pool, |_| (200, DISCORD_OK.into(), Duration::ZERO)).await;
    let hook = format!("{base}/api/webhooks/9/tok");
    // A switched-off target stops at claim time.
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    exec(&e.pool, &format!("UPDATE hub.platform_targets SET enabled = false WHERE id = {t}")).await;
    let w = worker(&e.pool, Some(&base), live_cfg(), &[(HOOK_REF, &hook)]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Stopped(id));
    assert_eq!(status(&e.pool, id).await, "manual_required");

    // The stop switch: nothing is claimed.
    let t2 = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id2 = queued(&e.pool, t2, 0).await;
    exec(&e.pool, "INSERT INTO hub.settings (key, value, updated_by) VALUES ('publishing.paused', 'true', 't')").await;
    assert_eq!(w.tick().await.expect("tick"), Tick::Paused);
    exec(&e.pool, "UPDATE hub.settings SET value = 'false' WHERE key = 'publishing.paused'").await;

    // Outside production a non-sandbox target is a dry run; a sandbox target is sent.
    let dev = worker(&e.pool, Some(&base), WorkerConfig { production: false, ..live_cfg() }, &[(HOOK_REF, &hook)]);
    assert_eq!(dev.tick().await.expect("tick"), Tick::DryRun(id2));
    let sb = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, true, Some(HOOK_REF), None, json!({})).await;
    let id3 = queued(&e.pool, sb, 0).await;
    assert_eq!(dev.tick().await.expect("tick"), Tick::Sent(id3, InstanceStatus::Posted));
    assert_eq!(m.log.lock().expect("log").len(), 1);

    // The daily limit: with a cap of 1 post a day the second waits for midnight UTC, still queued.
    exec(&e.pool, "INSERT INTO hub.spend_caps (scope, monthly_limit_usd_micros, daily_post_limit, updated_by) VALUES ('total', 0, 1, 't')").await;
    let id4 = queued(&e.pool, sb, 0).await;
    assert_eq!(dev.tick().await.expect("tick"), Tick::Deferred(id4));
    assert_eq!(status(&e.pool, id4).await, "queued");
    let midnight: bool = scalar(&e.pool, &format!(
        "SELECT next_attempt_at = date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' + interval '1 day' FROM hub.platform_instances WHERE id = {id4}"
    ))
    .await;
    assert!(midnight);
    // A platform without a publisher stops too.
    exec(&e.pool, "UPDATE hub.platform_definitions SET enabled = true WHERE code = 'FARCASTER'").await;
    let f = target(&e.pool, "FARCASTER", true, false, Some("PLATFORM_FC"), None, json!({})).await;
    let id5 = queued(&e.pool, f, 0).await;
    assert_eq!(dev.tick().await.expect("tick"), Tick::Stopped(id5));
}

#[tokio::test]
async fn a_missing_credential_fails_without_a_call() {
    let e = need!();
    let (base, m) = mock(&e.pool, |_| (200, DISCORD_OK.into(), Duration::ZERO)).await;
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    let w = worker(&e.pool, Some(&base), live_cfg(), &[]);
    assert_eq!(w.tick().await.expect("tick"), Tick::Sent(id, InstanceStatus::Failed));
    assert!(m.log.lock().expect("log").is_empty());
    let why: String = scalar(&e.pool, &format!("SELECT last_error FROM hub.platform_instances WHERE id = {id}")).await;
    assert!(why.contains("credential variable is not set"), "{why}");
    // Names outside the credential shape are never read from the environment.
    assert!((env_secrets())("DATABASE_URL").is_none());
}

#[tokio::test]
async fn metrics_alerts_and_instance_limits() {
    let e = need!();
    let t = target(&e.pool, "DISCORD_ANNOUNCEMENTS", true, false, Some(HOOK_REF), None, json!({})).await;
    let id = queued(&e.pool, t, 0).await;
    exec(&e.pool, &format!("UPDATE hub.platform_instances SET status = 'failed' WHERE id = {id}")).await;
    let cfg = live_cfg();
    let text = metrics::render(&e.pool, &cfg).await.expect("metrics");
    for want in [
        "# TYPE content_hub_instances gauge",
        "content_hub_instances{status=\"failed\"} 1",
        "content_hub_instances{status=\"queued\"} 0",
        "content_hub_spend_cap_usd_micros{scope=\"total\"} 0",
        "content_hub_publishing_paused 0",
        "content_hub_worker_live 1",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }

    // No snapshot yet: the snapshot gauges have no samples (an alert on absence is the monitoring stack's job).
    assert!(!text.contains("content_hub_snapshot_generated_timestamp_seconds{"), "{text}");
    exec(
        &e.pool,
        "INSERT INTO hub.snapshots (network, generated_at, schema_version, sha256, byte_size, signature, signer_address, blob_id, epochs, expires_at)
         VALUES ('testnet', '2026-10-30T10:00:00Z', 1, repeat('a', 64), 100, 'sig', '0x' || repeat('b', 64), 'blob', 30, '2026-11-29T10:00:00Z'),
                ('testnet', '2026-10-31T10:00:00Z', 1, repeat('c', 64), 100, 'sig', '0x' || repeat('b', 64), 'blob', 30, '2026-11-30T10:00:00Z')",
    )
    .await;
    let text = metrics::render(&e.pool, &cfg).await.expect("metrics");
    // The LATEST snapshot of the network: 2026-10-31T10:00:00Z = 1793440800, expiring 2026-11-30T10:00:00Z = 1796032800.
    for want in [
        "content_hub_snapshot_generated_timestamp_seconds{network=\"testnet\"} 1793440800",
        "content_hub_snapshot_storage_expires_timestamp_seconds{network=\"testnet\"} 1796032800",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }

    // One summary, then nothing until the interval passes.
    let (base, m) = mock(&e.pool, |_| (204, String::new(), Duration::ZERO)).await;
    let a = alerts::AlertConfig {
        webhook_url: format!("{base}/api/webhooks/5/ALERTTOKEN"),
        base_url: base.clone(),
        every: Duration::from_secs(3600),
    };
    let http = Http::unchecked_for_tests(Duration::from_millis(800));
    assert!(alerts::maybe_send(&e.pool, &http, &a, &cfg).await.expect("alert"));
    let sent = m.log.lock().expect("log").clone();
    assert_eq!(sent.len(), 1);
    let content = sent[0].1["content"].as_str().expect("content");
    assert!(content.contains("1 failed"), "{content}");
    assert_eq!(sent[0].1["allowed_mentions"], json!({"parse": []}));
    assert!(!alerts::maybe_send(&e.pool, &http, &a, &cfg).await.expect("alert"));
    assert_eq!(m.log.lock().expect("log").len(), 1);

    // Mastodon limits are fetched and cached in the target's configuration.
    let (mbase, _) = mock(&e.pool, |_| {
        (
            200,
            r#"{"configuration":{"statuses":{"max_characters":11000,"characters_reserved_per_url":23}}}"#.into(),
            Duration::ZERO,
        )
    })
    .await;
    let mt = target(&e.pool, "MASTODON", false, false, Some("PLATFORM_M"), Some(&mbase), json!({})).await;
    let w = worker(&e.pool, None, cfg, &[]);
    assert_eq!(w.refresh_mastodon_limits().await.expect("refresh"), 1);
    let limits: Value =
        scalar(&e.pool, &format!("SELECT config -> 'instance_limits' FROM hub.platform_targets WHERE id = {mt}")).await;
    assert_eq!(limits["max_characters"], 11000);
    assert_eq!(w.refresh_mastodon_limits().await.expect("refresh"), 0, "fresh limits are not fetched again");
}

#[test]
fn live_rules() {
    let off = WorkerConfig::default();
    assert_eq!(live(&off, true, true), Err("the worker runs with PUBLISHING_LIVE off"));
    let on = live_cfg();
    assert_eq!(live(&on, false, true), Err("the target is not live"));
    assert_eq!(live(&on, true, false), Ok(()));
    let dev = WorkerConfig { production: false, ..live_cfg() };
    assert!(live(&dev, true, false).is_err());
    assert_eq!(live(&dev, true, true), Ok(()));
}

// ── end to end: the admin API, the worker and the public feed ───────────────────────────────────────────

mod e2e {
    use axum::body::Body;
    use axum::http::Request;
    use content_hub_api::admin::{self, AdminState, Ceilings};
    use content_hub_api::auth::jwt::testing::{jwks, sign};
    use content_hub_api::auth::{AuthConfig, Authenticator, JwksSource, RoleSource};
    use content_hub_api::public::{self, PublicConfig, PublicState};
    use tower::ServiceExt;

    use super::*;

    const ISS: &str = "https://team.cloudflareaccess.com";
    const ADMIN: &str = "operator";
    const ORIGIN: &str = "https://hub.example.org";

    async fn call(r: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let now = Utc::now().timestamp();
        let token = sign(
            &json!({"alg": "RS256", "kid": "k1"}),
            &json!({"iss": ISS, "aud": ["aud"], "sub": ADMIN, "email": "op@example.org", "exp": now + 600}),
        );
        let b = Request::builder()
            .method(method)
            .uri(path)
            .header("cf-access-jwt-assertion", token)
            .header("x-content-hub-request", "1")
            .header("origin", ORIGIN);
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .expect("request");
        let res = r.clone().oneshot(req).await.expect("infallible");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 4 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_post_goes_from_the_admin_api_through_the_worker_to_the_feed_without_sending_anything() {
        let e = need!();
        let auth = Authenticator::new(AuthConfig {
            issuer: ISS.into(),
            audience: "aud".into(),
            jwks: JwksSource::Static(jwks("k1")),
            roles: RoleSource::Static([ADMIN.to_owned()].into()),
            admin_origin: ORIGIN.into(),
        })
        .expect("auth");
        let api = admin::router(AdminState { db: e.pool.clone(), auth, ceilings: Ceilings::default() });
        let public = public::router(PublicState {
            db: e._db.pool(&e._db.readonly, Access::ReadOnly),
            config: Arc::new(PublicConfig::default()),
        });
        let def = |code: &str| {
            let pool = e.pool.clone();
            let code = code.to_owned();
            async move {
                scalar::<i64>(&pool, &format!("SELECT id FROM hub.platform_definitions WHERE code = '{code}'")).await
            }
        };
        let mut ids = Vec::new();
        for body in [
            json!({"platform_definition_id": def("OWNED_FEED").await, "name": "main", "enabled": true, "live": true}),
            json!({"platform_definition_id": def("DISCORD_ANNOUNCEMENTS").await, "name": "announcements", "enabled": true,
                   "secret_ref": "PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL"}),
            json!({"platform_definition_id": def("REDDIT").await, "name": "r-sui", "enabled": true, "external_ref": "sui"}),
        ] {
            let (s, t) = call(&api, "POST", "/api/v1/targets", Some(body)).await;
            assert_eq!(s, StatusCode::CREATED, "{t}");
            ids.push(t["id"].as_i64().expect("target"));
        }
        let (_, p) = call(
            &api,
            "POST",
            "/api/v1/posts",
            Some(json!({"title": "Launch notes", "body_markdown": "We shipped **v1**: https://example.org/notes", "status": "ready"})),
        )
        .await;
        let post = p["id"].as_i64().expect("post");
        let targets: Vec<Value> = ids.iter().map(|t| json!({"target_id": t})).collect();
        let (s, made) =
            call(&api, "POST", &format!("/api/v1/posts/{post}/enqueue"), Some(json!({"targets": targets}))).await;
        assert_eq!(s, StatusCode::CREATED, "{made}");
        let st: Vec<&str> = made.as_array().expect("made").iter().filter_map(|i| i["status"].as_str()).collect();
        assert_eq!(st, ["queued", "queued", "manual_required"]);

        // A live worker: the feed target is live, the Discord target is not (and its webhook is a dry-run URL).
        let w = worker(
            &e.pool,
            None,
            live_cfg(),
            &[("PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL", "https://discord.com/api/webhooks/1/DRYTOKEN")],
        );
        let mut ticks = Vec::new();
        loop {
            match w.tick().await.expect("tick") {
                Tick::Idle => break,
                t => ticks.push(t),
            }
        }
        let feed_i = made[0]["id"].as_i64().expect("feed");
        let discord_i = made[1]["id"].as_i64().expect("discord");
        assert_eq!(ticks, [Tick::Sent(feed_i, InstanceStatus::Posted), Tick::DryRun(discord_i)]);

        // The dry run holds the exact request, rendered for Discord, without the token.
        let (_, d) = call(&api, "GET", &format!("/api/v1/instances/{discord_i}"), None).await;
        assert_eq!(d["instance"]["status"], "manual_required");
        let attempt = &d["attempts"][0];
        assert_eq!((attempt["outcome"].as_str(), attempt["dry_run"].as_bool()), (Some("dry_run"), Some(true)));
        assert_eq!(attempt["request_redacted"]["body"]["content"], "We shipped **v1**: https://example.org/notes");
        assert_eq!(attempt["request_redacted"]["url"], "https://discord.com/api/webhooks/1/[redacted]?wait=true");
        assert!(!d.to_string().contains("DRYTOKEN"));

        // The public feed lists the post; the manual platform is marked posted by a person.
        let (s, feed) = call(&public, "GET", "/public/v1/feed.json", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(feed["items"][0]["title"], "Launch notes");
        let reddit_i = made[2]["id"].as_i64().expect("reddit");
        let (s, _) = call(
            &api,
            "POST",
            &format!("/api/v1/instances/{reddit_i}/transition"),
            Some(json!({"to": "posted", "url_or_external_id": "https://www.reddit.com/r/sui/comments/abc"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, audit) = call(&api, "GET", "/api/v1/audit?entity_type=instance", None).await;
        let actions: Vec<&str> =
            audit["items"].as_array().expect("audit").iter().filter_map(|a| a["action"].as_str()).collect();
        assert!(actions.contains(&"instance.dry_run") && actions.contains(&"instance.move"), "{actions:?}");
    }
}
