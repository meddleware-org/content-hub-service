//! Handler tests against a real PostgreSQL with the production roles: the admin router runs as the WRITER, the
//! public router as the READ-ONLY role. Tokens are signed with the test key (auth::jwt::testing); state-changing
//! requests carry the CSRF header and Origin a browser behind Access would send.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use content_hub_store::Access;
use content_hub_store::repo::seed::{SeedReport, apply_definitions};
use content_hub_store::testdb::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::admin::{self, AdminState, Ceilings};
use crate::auth::jwt::testing::{jwks, sign};
use crate::auth::{AuthConfig, Authenticator, JwksSource, RoleSource};
use crate::public::{self, PublicConfig, PublicState};

const ISS: &str = "https://team.cloudflareaccess.com";
const AUD: &str = "aud-tag";
const ORIGIN: &str = "https://hub.example.org";
const ADMIN: &str = "admin-subject";

fn token(sub: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    sign(
        &json!({"alg": "RS256", "kid": "k1"}),
        &json!({"iss": ISS, "aud": [AUD], "sub": sub, "email": "op@example.org", "exp": now + 600}),
    )
}

fn auth(roles: RoleSource, jwks_src: JwksSource) -> Arc<Authenticator> {
    Authenticator::new(AuthConfig {
        issuer: ISS.into(),
        audience: AUD.into(),
        jwks: jwks_src,
        roles,
        admin_origin: ORIGIN.into(),
    })
    .expect("auth")
}

fn static_auth() -> Arc<Authenticator> {
    auth(RoleSource::Static([ADMIN.to_owned()].into()), JwksSource::Static(jwks("k1")))
}

struct Env {
    db: TestDb,
    admin: Router,
    public: Router,
}

async fn env_with(ceilings: Ceilings) -> Option<Env> {
    let db = TestDb::migrated().await?;
    let mut w = db.conn(&db.writer).await;
    let defs = content_hub_core::platforms::generic().expect("definitions");
    apply_definitions(&mut w, &defs, &mut SeedReport::default()).await.expect("seed definitions");
    let admin = admin::router(AdminState { db: db.pool(&db.writer, Access::ReadWrite), auth: static_auth(), ceilings });
    let public = public::router(PublicState {
        db: db.pool(&db.readonly, Access::ReadOnly),
        config: Arc::new(PublicConfig::default()),
    });
    Some(Env { db, admin, public })
}

async fn env() -> Option<Env> {
    env_with(Ceilings::default()).await
}

async fn call(
    r: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    sub: Option<&str>,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(s) = sub {
        b = b.header("cf-access-jwt-assertion", token(s));
    }
    if csrf {
        b = b.header("x-content-hub-request", "1").header("origin", ORIGIN);
    }
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

async fn get(e: &Env, path: &str) -> (StatusCode, Value) {
    call(&e.admin, "GET", path, None, Some(ADMIN), false).await
}

async fn send(e: &Env, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    call(&e.admin, method, path, Some(body), Some(ADMIN), true).await
}

/// Every key path of a JSON document (`a[].b`), so a public response's shape is pinned exactly.
fn key_paths(v: &Value) -> std::collections::BTreeSet<String> {
    fn walk(v: &Value, at: &str, out: &mut std::collections::BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    let p = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                    out.insert(p.clone());
                    walk(x, &p, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, &format!("{at}[]"), out)),
            _ => {}
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(v, "", &mut out);
    out
}

fn set(paths: &[&str]) -> std::collections::BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

async fn platform_id(e: &Env, code: &str) -> i64 {
    let (_, list) = get(e, "/api/v1/platforms").await;
    list.as_array().expect("list").iter().find(|p| p["code"] == code).and_then(|p| p["id"].as_i64()).expect("platform")
}

macro_rules! need {
    () => {
        match env().await {
            Some(e) => e,
            None => return,
        }
    };
}

// ── authentication and authorisation ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_auth_matrix() {
    let e = need!();
    assert_eq!(call(&e.admin, "GET", "/api/v1/me", None, None, false).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&e.admin, "GET", "/api/v1/me", None, Some("someone-else"), false).await.0, StatusCode::FORBIDDEN);
    let (s, me) = get(&e, "/api/v1/me").await;
    assert_eq!(
        (s, me["subject"].as_str(), me["display_name"].as_str()),
        (StatusCode::OK, Some(ADMIN), Some("op@example.org"))
    );
    // A write without the CSRF header and Origin is refused even with a valid token.
    let body = json!({"key": "p", "sort_order": 1, "internal_name": "n", "public_name": "n"});
    assert_eq!(
        call(&e.admin, "POST", "/api/v1/phases", Some(body.clone()), Some(ADMIN), false).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(send(&e, "POST", "/api/v1/phases", body).await.0, StatusCode::CREATED);
    // A garbage token is a 401, never a 500.
    let req = Request::builder()
        .uri("/api/v1/me")
        .header("cf-access-jwt-assertion", "x.y.z")
        .body(Body::empty())
        .expect("req");
    assert_eq!(e.admin.clone().oneshot(req).await.expect("ok").status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn keto_and_jwks_outages_fail_closed() {
    let Some(db) = TestDb::migrated().await else { return };
    let keto_down = admin::router(AdminState {
        db: db.pool(&db.writer, Access::ReadWrite),
        auth: auth(RoleSource::Keto("http://127.0.0.1:1".into()), JwksSource::Static(jwks("k1"))),
        ceilings: Ceilings::default(),
    });
    assert_eq!(
        call(&keto_down, "GET", "/api/v1/me", None, Some(ADMIN), false).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let jwks_down = admin::router(AdminState {
        db: db.pool(&db.writer, Access::ReadWrite),
        auth: auth(
            RoleSource::Static([ADMIN.to_owned()].into()),
            JwksSource::Remote("http://127.0.0.1:1/certs".into()),
        ),
        ceilings: Ceilings::default(),
    });
    assert_eq!(
        call(&jwks_down, "GET", "/api/v1/me", None, Some(ADMIN), false).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

// ── roadmap ────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn phases_and_items_with_validation_and_audit() {
    let e = need!();
    let (s, p) = send(&e, "POST", "/api/v1/phases", json!({"key": "launch", "sort_order": 1, "internal_name": "Internal", "public_name": "Launch", "visible_human": true})).await;
    assert_eq!(s, StatusCode::CREATED);
    let pid = p["id"].as_i64().expect("id");
    assert_eq!(
        send(
            &e,
            "POST",
            "/api/v1/phases",
            json!({"key": "Bad Key", "sort_order": 2, "internal_name": "a", "public_name": "b"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(
            &e,
            "POST",
            "/api/v1/phases",
            json!({"key": "launch", "sort_order": 3, "internal_name": "a", "public_name": "b"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(
            &e,
            "POST",
            "/api/v1/phases",
            json!({"key": "x", "sort_order": 3, "internal_name": "a", "public_name": "b", "surprise": 1})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (s, p2) =
        send(&e, "PATCH", &format!("/api/v1/phases/{pid}"), json!({"public_description": "soon", "window_end": null}))
            .await;
    assert_eq!((s, p2["public_description"].as_str()), (StatusCode::OK, Some("soon")));
    // A visible item without public wording is refused; with it, created.
    let item = json!({"phase_id": pid, "key": "mint", "item_type": "content", "internal_title": "NFT mint", "visible_human": true});
    assert_eq!(send(&e, "POST", "/api/v1/items", item).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let item = json!({"phase_id": pid, "key": "mint", "item_type": "content", "internal_title": "NFT mint", "public_title": "Mint",
                      "public_description": "Opens soon", "visible_human": true, "public_due_at": "2027-03-01T12:00:00Z"});
    let (s, i) = send(&e, "POST", "/api/v1/items", item).await;
    assert_eq!(s, StatusCode::CREATED, "{i}");
    let (_, page) = get(&e, &format!("/api/v1/items?phase_id={pid}&limit=1")).await;
    assert_eq!(page["items"][0]["key"], "mint");
    assert_eq!(get(&e, "/api/v1/items?bogus=1").await.0, StatusCode::BAD_REQUEST);
    let (_, audit) = get(&e, "/api/v1/audit?entity_type=phase").await;
    let actions: Vec<&str> =
        audit["items"].as_array().expect("items").iter().filter_map(|a| a["action"].as_str()).collect();
    assert_eq!(actions, vec!["phase.update", "phase.create"]);

    // The public roadmap shows the public wording and date only, with an ETag that revalidates.
    let (s, r) = call(&e.public, "GET", "/public/v1/roadmap", None, None, false).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["phases"][0]["name"], "Launch");
    assert_eq!(r["phases"][0]["items"][0]["title"], "Mint");
    assert_eq!(r["phases"][0]["items"][0]["due_at"], "2027-03-01T12:00:00Z");
    assert_eq!(
        key_paths(&r),
        set(&[
            "phases",
            "phases[].description",
            "phases[].id",
            "phases[].items",
            "phases[].items[].description",
            "phases[].items[].due_at",
            "phases[].items[].id",
            "phases[].items[].status",
            "phases[].items[].title",
            "phases[].items[].updated_at",
            "phases[].name",
            "phases[].window_end",
            "phases[].window_start",
            "schema_version",
            "updated_at",
        ]),
        "the public roadmap's keys are a decision (ARCHITECTURE.md §6)"
    );
    let text = r.to_string();
    for internal in ["Internal", "NFT mint", "launch", "\"key\""] {
        assert!(!text.contains(internal), "public roadmap leaks {internal}: {text}");
    }
    let res = e
        .public
        .clone()
        .oneshot(Request::builder().uri("/public/v1/roadmap").body(Body::empty()).expect("req"))
        .await
        .expect("ok");
    let etag = res.headers()["etag"].to_str().expect("etag").to_owned();
    let res = e
        .public
        .clone()
        .oneshot(
            Request::builder()
                .uri("/public/v1/roadmap")
                .header("if-none-match", etag)
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("ok");
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    // Not visible to bots.
    let (_, bot) = call(&e.public, "GET", "/public/v1/roadmap-bot", None, None, false).await;
    assert_eq!(bot["phases"], json!([]));
    // The admin preview is the public document itself, for each audience.
    let (s, human) = get(&e, "/api/v1/preview/roadmap?audience=human").await;
    assert_eq!((s, &human), (StatusCode::OK, &r));
    let (s, bots) = get(&e, "/api/v1/preview/roadmap?audience=bot").await;
    assert_eq!((s, &bots), (StatusCode::OK, &bot));
    assert_eq!(get(&e, "/api/v1/preview/roadmap?audience=everyone").await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(get(&e, "/api/v1/preview/roadmap").await.0, StatusCode::BAD_REQUEST);
    // Posts per phase are counted by the phase's internal name.
    let (_, post) =
        send(&e, "POST", "/api/v1/posts", json!({"title": "T", "body_markdown": "b", "roadmap_item_id": i["id"]}))
            .await;
    assert!(post["id"].is_i64());
    let (_, st) = get(&e, "/api/v1/stats").await;
    assert_eq!(st["posts_by_phase"], json!([{"key": "Internal", "count": 1}]));
}

// ── tasks ──────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tasks_follow_the_state_machine_and_refuse_cycles() {
    let e = need!();
    let (_, a) = send(&e, "POST", "/api/v1/tasks", json!({"key": "a", "task_type": "ENGINEERING", "title": "A"})).await;
    let a = a["task"]["id"].as_i64().expect("a");
    let (s, b) =
        send(&e, "POST", "/api/v1/tasks", json!({"key": "b", "task_type": "OTHER", "title": "B", "depends_on": [a]}))
            .await;
    assert_eq!(s, StatusCode::CREATED);
    let b = b["task"]["id"].as_i64().expect("b");
    assert_eq!(
        send(&e, "PATCH", &format!("/api/v1/tasks/{a}"), json!({"depends_on": [b]})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "cycle"
    );
    assert_eq!(
        send(&e, "POST", &format!("/api/v1/tasks/{b}/transition"), json!({"to": "done"})).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(&e, "POST", &format!("/api/v1/tasks/{b}/transition"), json!({"to": "in_progress"})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "dependency unfinished"
    );
    assert_eq!(
        send(&e, "POST", &format!("/api/v1/tasks/{b}/transition"), json!({"to": "blocked"})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no reason"
    );
    let (s, t) = send(
        &e,
        "POST",
        &format!("/api/v1/tasks/{b}/transition"),
        json!({"to": "blocked", "blocked_reason": "waiting for A"}),
    )
    .await;
    assert_eq!((s, t["task"]["blocked_reason"].as_str()), (StatusCode::OK, Some("waiting for A")));
    for to in ["in_progress", "done"] {
        assert_eq!(
            send(&e, "POST", &format!("/api/v1/tasks/{a}/transition"), json!({"to": to})).await.0,
            StatusCode::OK
        );
    }
    let (_, d) = get(&e, &format!("/api/v1/tasks/{b}")).await;
    assert!(d["allowed_moves"].as_array().expect("moves").iter().any(|m| m == "in_progress"));
    let (s, l) = send(
        &e,
        "POST",
        &format!("/api/v1/tasks/{b}/links"),
        json!({"link_kind": "external", "url": "http://insecure.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{l}");
    let (s, l) = send(
        &e,
        "POST",
        &format!("/api/v1/tasks/{b}/links"),
        json!({"link_kind": "external", "url": "https://example.org/x", "label": "spec"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let lid = l["id"].as_i64().expect("link");
    assert_eq!(
        call(&e.admin, "DELETE", &format!("/api/v1/tasks/{b}/links/{lid}"), None, Some(ADMIN), true).await.0,
        StatusCode::NO_CONTENT
    );
}

// ── posts, targets and publishing ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn posts_targets_enqueue_and_operator_moves() {
    let e = need!();
    let (s, p) = send(
        &e,
        "POST",
        "/api/v1/posts",
        json!({"title": "Hello", "body_markdown": "Line one\r\nLine two with https://example.org"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(p["body_markdown"], "Line one\nLine two with https://example.org");
    let post = p["id"].as_i64().expect("post");
    let (_, p2) = send(&e, "PATCH", &format!("/api/v1/posts/{post}"), json!({"body_markdown": "Edited"})).await;
    assert_eq!(p2["revision"], 2);

    // Targets: a secret value is refused; a forum needs its base URL.
    let feed = platform_id(&e, "OWNED_FEED").await;
    let discord = platform_id(&e, "DISCORD_ANNOUNCEMENTS").await;
    let reddit = platform_id(&e, "REDDIT").await;
    let forums = platform_id(&e, "DEV_FORUMS").await;
    let x = platform_id(&e, "X").await;
    let leak = json!({"platform_definition_id": discord, "name": "a", "secret_ref": "https://discord.com/api/webhooks/1/abc", "enabled": true});
    assert_eq!(send(&e, "POST", "/api/v1/targets", leak).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        send(&e, "POST", "/api/v1/targets", json!({"platform_definition_id": forums, "name": "f", "enabled": true}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mk = |def: i64, name: &str| json!({"platform_definition_id": def, "name": name, "enabled": true, "external_ref": "sui"});
    let mut ids = Vec::new();
    for (def, name) in [(feed, "main"), (discord, "announcements"), (reddit, "r-sui"), (x, "brand")] {
        let (s, t) = send(&e, "POST", "/api/v1/targets", mk(def, name)).await;
        assert_eq!(s, StatusCode::CREATED, "{t}");
        ids.push(t["id"].as_i64().expect("target"));
    }
    let (feed_t, discord_t, reddit_t, x_t) = (ids[0], ids[1], ids[2], ids[3]);

    // A draft cannot be enqueued.
    let enq = |targets: Value| json!({"targets": targets});
    assert_eq!(
        send(&e, "POST", &format!("/api/v1/posts/{post}/enqueue"), enq(json!([{"target_id": feed_t}]))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    send(&e, "PATCH", &format!("/api/v1/posts/{post}"), json!({"status": "ready"})).await;
    // X is disabled; Discord with an override that is too long does not fit; nothing is enqueued when any target fails.
    let (s, r) = send(
        &e,
        "POST",
        &format!("/api/v1/posts/{post}/enqueue"),
        enq(json!([{"target_id": feed_t}, {"target_id": x_t}, {"target_id": discord_t, "override_body": "x".repeat(2001)}])),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r["problems"].as_array().expect("problems").len(), 2, "{r}");
    let (_, none) = get(&e, &format!("/api/v1/instances?post_id={post}")).await;
    assert_eq!(none["items"], json!([]));
    // Feed and Discord queue; Reddit (manual) waits for a person, with its intent link.
    let (s, made) = send(
        &e,
        "POST",
        &format!("/api/v1/posts/{post}/enqueue"),
        enq(json!([{"target_id": feed_t}, {"target_id": discord_t}, {"target_id": reddit_t}])),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let statuses: Vec<&str> = made.as_array().expect("made").iter().filter_map(|i| i["status"].as_str()).collect();
    assert_eq!(statuses, vec!["queued", "queued", "manual_required"]);
    assert_eq!(
        send(&e, "POST", &format!("/api/v1/posts/{post}/enqueue"), enq(json!([{"target_id": feed_t}]))).await.0,
        StatusCode::CONFLICT
    );
    let manual = made[2]["id"].as_i64().expect("manual");
    let (_, d) = get(&e, &format!("/api/v1/instances/{manual}")).await;
    assert!(
        d["intent_url"].as_str().expect("intent").starts_with("https://www.reddit.com/r/sui/submit?title=Hello"),
        "{d}"
    );
    // Operator moves: posted needs the URL; posted is final; the worker's moves are not the operator's.
    let mv = |id: i64, body: Value| (format!("/api/v1/instances/{id}/transition"), body);
    let (path, body) = mv(manual, json!({"to": "posted"}));
    assert_eq!(send(&e, "POST", &path, body).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (path, body) =
        mv(manual, json!({"to": "posted", "url_or_external_id": "https://www.reddit.com/r/sui/comments/abc"}));
    assert_eq!(send(&e, "POST", &path, body).await.0, StatusCode::OK);
    let (path, body) = mv(manual, json!({"to": "cancelled"}));
    assert_eq!(send(&e, "POST", &path, body).await.0, StatusCode::CONFLICT);
    let (path, body) = mv(made[0]["id"].as_i64().expect("queued"), json!({"to": "sending"}));
    assert_eq!(send(&e, "POST", &path, body).await.0, StatusCode::CONFLICT);
    let (path, body) = mv(manual, json!({"external_status": "in_review"}));
    assert_eq!(send(&e, "POST", &path, body).await.1["instance"]["external_status"], "in_review");
}

/// This month's usage of one platform, from the budgets view.
async fn used(e: &Env, def: i64) -> i64 {
    let (_, b) = get(e, "/api/v1/budgets").await;
    b["usage"]
        .as_array()
        .expect("usage")
        .iter()
        .find(|u| u["platform_definition_id"] == def)
        .and_then(|u| u["used_usd_micros"].as_i64())
        .unwrap_or(0)
}

#[tokio::test]
async fn paid_sends_reserve_spend_and_every_requeue_revalidates() {
    let Some(e) =
        env_with(Ceilings { total_monthly_usd_micros: 1_000_000, per_platform_monthly_usd_micros: 500_000 }).await
    else {
        return;
    };
    let x = platform_id(&e, "X").await;
    assert_eq!(send(&e, "PATCH", &format!("/api/v1/platforms/{x}"), json!({"enabled": true})).await.0, StatusCode::OK);
    // Credential names carry the PLATFORM_ prefix; a live target needs one.
    let t = |extra: Value| {
        let mut b = json!({"platform_definition_id": x, "name": "brand", "enabled": true});
        b.as_object_mut().expect("obj").extend(extra.as_object().expect("obj").clone());
        b
    };
    assert_eq!(
        send(&e, "POST", "/api/v1/targets", t(json!({"secret_ref": "X_MAIN"}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(send(&e, "POST", "/api/v1/targets", t(json!({"live": true}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, target) =
        send(&e, "POST", "/api/v1/targets", t(json!({"secret_ref": "PLATFORM_X_MAIN", "live": true}))).await;
    assert_eq!(s, StatusCode::CREATED, "{target}");
    assert_eq!(target["live"], true);
    let tid = target["id"].as_i64().expect("target");
    let ready = |body: &str| json!({"title": "News", "body_markdown": body, "status": "ready"});

    // Under the caps: queued, with its cost reserved before anything is sent.
    let (_, p1) = send(&e, "POST", "/api/v1/posts", ready("Plain news")).await;
    let p1 = p1["id"].as_i64().expect("post");
    let (s, made) =
        send(&e, "POST", &format!("/api/v1/posts/{p1}/enqueue"), json!({"targets": [{"target_id": tid}]})).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    assert_eq!(
        (made[0]["status"].as_str(), made[0]["cost_estimate_usd_micros"].as_i64()),
        (Some("queued"), Some(15_000))
    );
    assert_eq!(made[0]["payload_body"], "Plain news");
    assert_eq!(made[0]["payload_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(used(&e, x).await, 15_000);

    // A runtime cap of 20 000 leaves no room for a second post: it waits for a person instead, nothing reserved.
    let cap = json!({"caps": [{"scope": "platform", "platform_definition_id": x, "monthly_limit_usd_micros": 20_000}]});
    assert_eq!(send(&e, "PUT", "/api/v1/budgets", cap).await.0, StatusCode::OK);
    let (_, p2) = send(&e, "POST", "/api/v1/posts", ready("More news")).await;
    let p2 = p2["id"].as_i64().expect("post");
    let (_, over) =
        send(&e, "POST", &format!("/api/v1/posts/{p2}/enqueue"), json!({"targets": [{"target_id": tid}]})).await;
    assert_eq!(over[0]["status"], "manual_required");
    assert!(over[0]["last_error"].as_str().expect("why").contains("monthly spend cap"), "{over}");
    assert_eq!(used(&e, x).await, 15_000);

    // Cancelling releases the reservation; re-queueing reserves again, after re-validating the current post.
    let i1 = made[0]["id"].as_i64().expect("instance");
    let mv = |to: &str| json!({"to": to});
    let path = format!("/api/v1/instances/{i1}/transition");
    assert_eq!(send(&e, "POST", &path, mv("cancelled")).await.0, StatusCode::OK);
    assert_eq!(used(&e, x).await, 0);
    assert_eq!(send(&e, "POST", &path, mv("planned")).await.0, StatusCode::OK);
    // The post was edited meanwhile and no longer fits X: the move into queued is refused and nothing changes.
    send(&e, "PATCH", &format!("/api/v1/posts/{p1}"), json!({"body_markdown": "x".repeat(281)})).await;
    let (s, why) = send(&e, "POST", &path, mv("queued")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{why}");
    assert!(why["message"].as_str().expect("message").contains("does not fit"), "{why}");
    let (_, d) = get(&e, &format!("/api/v1/instances/{i1}")).await;
    assert_eq!(d["instance"]["status"], "planned");
    assert_eq!(used(&e, x).await, 0);
    send(&e, "PATCH", &format!("/api/v1/posts/{p1}"), json!({"body_markdown": "Shorter news"})).await;
    let (s, d) = send(&e, "POST", &path, mv("queued")).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(
        (d["instance"]["payload_body"].as_str(), d["instance"]["post_revision"].as_i64()),
        (Some("Shorter news"), Some(3))
    );
    assert_eq!(used(&e, x).await, 15_000);
}

#[tokio::test]
async fn hosts_the_worker_calls_and_the_stop_switch() {
    let e = need!();
    let m = platform_id(&e, "MASTODON").await;
    let mk = |ext: Value| json!({"platform_definition_id": m, "name": "m", "enabled": true, "external_ref": ext});
    for bad in
        [Value::Null, json!("http://mastodon.example"), json!("https://127.0.0.1"), json!("https://mastodon.local")]
    {
        assert_eq!(
            send(&e, "POST", "/api/v1/targets", mk(bad.clone())).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}"
        );
    }
    assert_eq!(send(&e, "POST", "/api/v1/targets", mk(json!("https://mastodon.example"))).await.0, StatusCode::CREATED);
    let b = platform_id(&e, "BLUESKY").await;
    let pds = json!({"platform_definition_id": b, "name": "b", "external_ref": "a.example", "config": {"pds": "https://10.0.0.5"}});
    assert_eq!(send(&e, "POST", "/api/v1/targets", pds).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(get(&e, "/api/v1/publishing").await.1, json!({"paused": false}));
    assert_eq!(send(&e, "PUT", "/api/v1/publishing", json!({"paused": true})).await.1, json!({"paused": true}));
    assert_eq!(get(&e, "/api/v1/publishing").await.1, json!({"paused": true}));
    assert_eq!(
        send(&e, "PUT", "/api/v1/publishing", json!({"paused": true, "x": 1})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, audit) = get(&e, "/api/v1/audit?entity_type=setting").await;
    assert_eq!(audit["items"][0]["action"], "publishing.update");
}

#[tokio::test]
async fn the_latest_signed_snapshot_is_public_with_a_pinned_shape() {
    let e = need!();
    // None yet: a null snapshot, not an error.
    let (s0, none) = call(&e.public, "GET", "/public/v1/snapshot", None, None, false).await;
    assert_eq!(
        (s0, &none["snapshot"], none["schema"].as_str()),
        (StatusCode::OK, &Value::Null, Some("content-hub.roadmap-snapshot/1"))
    );
    let mut a = e.db.admin().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO hub.snapshots (network, generated_at, schema_version, sha256, byte_size, signature, signer_address, blob_id, object_id, epochs, expires_at)
         VALUES ('testnet', '2026-10-29T12:00:00Z', 1, '{0}', 715, 'c2ln', '0x{1}', 'Qc9aP0V6x3mYk2w1TnqL8sRZb4dEoFhJiUyXvCgA7MM', '0x{2}', 30, '2026-11-27T12:00:00Z'),
                ('mainnet', '2026-10-30T12:00:00Z', 1, '{3}', 700, 'c2ln', '0x{1}', NULL, NULL, NULL, NULL)",
        "a".repeat(64), "b".repeat(64), "c".repeat(64), "d".repeat(64)
    )))
    .execute(&mut a)
    .await
    .expect("snapshots");
    let with_aggregator = public::router(PublicState {
        db: e.db.pool(&e.db.readonly, Access::ReadOnly),
        config: Arc::new(PublicConfig {
            aggregator_url: Some("https://aggregator.example.org/".into()),
            ..PublicConfig::default()
        }),
    });
    let (s, snap) = call(&with_aggregator, "GET", "/public/v1/snapshot?network=testnet", None, None, false).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        key_paths(&snap),
        set(&[
            "blob_url",
            "schema",
            "snapshot",
            "snapshot.blob_id",
            "snapshot.byte_size",
            "snapshot.epochs",
            "snapshot.expires_at",
            "snapshot.generated_at",
            "snapshot.network",
            "snapshot.object_id",
            "snapshot.schema_version",
            "snapshot.sha256",
            "snapshot.signature",
            "snapshot.signer_address",
            "verify",
        ]),
        "the snapshot route's keys are a decision"
    );
    assert_eq!(snap["blob_url"], "https://aggregator.example.org/v1/blobs/Qc9aP0V6x3mYk2w1TnqL8sRZb4dEoFhJiUyXvCgA7MM");
    assert_eq!(snap["snapshot"]["network"], "testnet");
    // Without a network the newest of any is returned; an unstored snapshot has no blob link; a bad network is refused.
    let (_, newest) = call(&with_aggregator, "GET", "/public/v1/snapshot", None, None, false).await;
    assert_eq!((newest["snapshot"]["network"].as_str(), &newest["blob_url"]), (Some("mainnet"), &Value::Null));
    assert_eq!(
        call(&e.public, "GET", "/public/v1/snapshot?network=devnet", None, None, false).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(call(&e.public, "GET", "/public/v1/snapshot?x=1", None, None, false).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn budgets_respect_the_instance_ceilings() {
    let e = need!();
    let cap = json!({"caps": [{"scope": "total", "monthly_limit_usd_micros": 1}]});
    assert_eq!(
        send(&e, "PUT", "/api/v1/budgets", cap).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "default ceiling is 0"
    );
    drop(e);
    let Some(e) =
        env_with(Ceilings { total_monthly_usd_micros: 5_000_000, per_platform_monthly_usd_micros: 2_000_000 }).await
    else {
        return;
    };
    let x = platform_id(&e, "X").await;
    let ok = json!({"caps": [{"scope": "total", "monthly_limit_usd_micros": 5_000_000}, {"scope": "platform", "platform_definition_id": x, "monthly_limit_usd_micros": 2_000_000}]});
    let (s, b) = send(&e, "PUT", "/api/v1/budgets", ok).await;
    assert_eq!((s, b["caps"].as_array().map(Vec::len)), (StatusCode::OK, Some(2)));
    let over =
        json!({"caps": [{"scope": "platform", "platform_definition_id": x, "monthly_limit_usd_micros": 2_000_001}]});
    assert_eq!(send(&e, "PUT", "/api/v1/budgets", over).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_feed_serves_posted_entries_to_the_public() {
    let e = need!();
    let mut a = e.db.admin().await;
    sqlx::raw_sql(
        "INSERT INTO hub.posts (title, body_markdown, status, created_by) VALUES ('News & <more>', 'Body', 'ready', 't');
         INSERT INTO hub.platform_targets (platform_definition_id, name, enabled) SELECT id, 'main', true FROM hub.platform_definitions WHERE code = 'OWNED_FEED';
         INSERT INTO hub.platform_instances (post_id, platform_target_id, status, posted_at) SELECT p.id, t.id, 'posted', now() FROM hub.posts p, hub.platform_targets t",
    )
    .execute(&mut a)
    .await
    .expect("fixture");
    let (s, f) = call(&e.public, "GET", "/public/v1/feed.json", None, None, false).await;
    assert_eq!((s, f["items"][0]["title"].as_str()), (StatusCode::OK, Some("News & <more>")));
    assert_eq!(
        key_paths(&f),
        set(&[
            "feed_url",
            "home_page_url",
            "items",
            "items[].content_text",
            "items[].date_modified",
            "items[].date_published",
            "items[].id",
            "items[].title",
            "title",
            "version",
        ])
    );
    let res = e
        .public
        .clone()
        .oneshot(Request::builder().uri("/public/v1/feed.xml").body(Body::empty()).expect("req"))
        .await
        .expect("ok");
    assert_eq!(res.headers()["content-type"], "application/rss+xml; charset=utf-8");
    let xml =
        String::from_utf8(axum::body::to_bytes(res.into_body(), 1 << 20).await.expect("body").to_vec()).expect("utf8");
    assert!(xml.contains("<title>News &amp; &lt;more&gt;</title>"));
    let (s, o) = call(&e.public, "GET", "/public/v1/openapi.json", None, None, false).await;
    assert_eq!((s, o["openapi"].is_string()), (StatusCode::OK, true));
}
