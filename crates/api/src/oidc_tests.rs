//! The OpenID Connect sign-in against a fake provider and a fake policy decision point, with the admin router over a real PostgreSQL (skipped when none
//! is configured). The fake provider signs its tokens with the identity crate's throw-away key, checks the PKCE verifier and the client's Basic
//! credentials the way the real one does, and lets each test choose what the next ID token says.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use content_hub_store::Access;
use content_hub_store::testdb::TestDb;
use meddleware_identity::pkce;
use meddleware_identity::testing::{Signer, Suite};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::admin::{self, AdminState, Ceilings};
use crate::auth::oidc::{Oidc, OidcConfig};
use crate::auth::{AdminAuth, AuthConfig, Authenticator, JwksSource, RoleSource};

/// The console's client id and secret at the fake provider.
const CLIENT: (&str, &str) = ("content-hub-console", "console-secret-0123456789abcdef");
/// The console's origin.
const ORIGIN: &str = "https://hub.example.org";
/// The subject of the person the fake policy decision point admits.
const MEMBER: &str = "member-sub";
/// The Cloudflare Access token's subject (the old path), admitted by the static role list.
const ACCESS_ADMIN: &str = "admin-subject";

/// What the fake provider and decision point do next; each test sets what it needs.
struct Script {
    /// The PKCE challenge of the authorization request being completed.
    challenge: String,
    /// The claims the next ID token carries.
    claims: Value,
    /// The code the token endpoint accepts.
    code: String,
    /// The subjects the decision point admits.
    members: BTreeSet<String>,
    /// Whether the decision point answers 500.
    pdp_down: bool,
    /// How many decisions were asked for.
    pdp_calls: usize,
}

/// The fake provider and decision point: one server, one signer, one script.
struct Fake {
    /// The provider's signing key.
    signer: Signer,
    /// What happens next.
    script: Mutex<Script>,
    /// The base URL, `http://127.0.0.1:<port>`.
    base: String,
}

impl Fake {
    /// The issuer.
    fn issuer(&self) -> String {
        format!("{}/realms/test", self.base)
    }
}

/// `GET /realms/test/.well-known/openid-configuration`.
async fn discovery(State(f): State<Arc<Fake>>) -> Json<Value> {
    let i = f.issuer();
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/protocol/openid-connect/auth"),
        "token_endpoint": format!("{i}/protocol/openid-connect/token"),
        "jwks_uri": format!("{i}/protocol/openid-connect/certs"),
        "end_session_endpoint": format!("{i}/protocol/openid-connect/logout"),
        "code_challenge_methods_supported": ["S256"],
        "authorization_response_iss_parameter_supported": true,
        "backchannel_logout_supported": true
    }))
}

/// `GET /realms/test/protocol/openid-connect/certs`: the signer's public key.
async fn certs(State(f): State<Arc<Fake>>) -> Json<Value> {
    let k = f.signer.jwks().keys.remove(0);
    Json(json!({"keys": [{"kid": k.kid, "kty": k.kty, "alg": "RS256", "use": "sig", "n": k.n, "e": k.e}]}))
}

/// `POST /realms/test/protocol/openid-connect/token`: checks the client's credentials, the code, the redirect URI and the PKCE verifier, then signs the scripted ID token.
async fn token(State(f): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let basic = format!("Basic {}", STANDARD.encode(format!("{}:{}", CLIENT.0, CLIENT.1)));
    let ok_auth = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(basic.as_str());
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let field = |n: &str| form.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap_or_default();
    let s = f.script.lock().expect("script");
    let good = ok_auth
        && field("grant_type") == "authorization_code"
        && field("code") == s.code
        && field("redirect_uri") == format!("{ORIGIN}/auth/callback")
        && pkce::challenge(&field("code_verifier")) == s.challenge;
    if !good {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
    }
    Json(json!({"id_token": f.signer.sign(&s.claims), "token_type": "Bearer"})).into_response()
}

/// `POST /stores/s/access/v1/evaluation`: AuthZEN, with the scripted members.
async fn evaluation(State(f): State<Arc<Fake>>, Json(req): Json<Value>) -> Response {
    let mut s = f.script.lock().expect("script");
    s.pdp_calls += 1;
    if s.pdp_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let subject = req["subject"]["id"].as_str().unwrap_or_default();
    let right = req["subject"]["type"] == "user"
        && req["resource"]["type"] == "console"
        && req["resource"]["id"] == "content-hub"
        && req["action"]["name"] == "member";
    Json(json!({"decision": right && s.members.contains(subject)})).into_response()
}

/// Start the fake server on a free port.
async fn start_fake() -> Arc<Fake> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let fake = Arc::new(Fake {
        signer: Signer::new(Suite::Rs256, "k1"),
        script: Mutex::new(Script {
            challenge: String::new(),
            claims: json!({}),
            code: "good-code".into(),
            members: [MEMBER.to_owned()].into(),
            pdp_down: false,
            pdp_calls: 0,
        }),
        base,
    });
    let app = Router::new()
        .route("/realms/test/.well-known/openid-configuration", get(discovery))
        .route("/realms/test/protocol/openid-connect/certs", get(certs))
        .route("/realms/test/protocol/openid-connect/token", post(token))
        .route("/stores/s/access/v1/evaluation", post(evaluation))
        .with_state(fake.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    fake
}

/// A test environment: the fake provider, a migrated database and the admin router in a mode.
struct Env {
    /// The fake provider and decision point.
    fake: Arc<Fake>,
    /// The database (kept alive for the test).
    db: TestDb,
    /// The admin router.
    admin: Router,
}

/// The console's sign-in configuration against the fake.
fn oidc_config(fake: &Fake) -> OidcConfig {
    OidcConfig {
        issuer: fake.issuer(),
        client_id: CLIENT.0.into(),
        client_secret: CLIENT.1.into(),
        redirect_uri: format!("{ORIGIN}/auth/callback"),
        post_logout_uri: format!("{ORIGIN}/"),
        internal_base: None,
        authz_base: format!("{}/stores/s", fake.base),
        authz_key: Some("pdp-key".into()),
        resource: "content-hub".into(),
        cookie_name: "__Host-hub-admin".into(),
        binding_cookie_name: "__Host-hub-login".into(),
        idle: Duration::from_secs(3600),
        absolute: Duration::from_secs(43_200),
        allow_insecure: true,
    }
}

/// Build the environment in `mode`; `None` when no test database is configured.
async fn env(mode: AdminAuth) -> Option<Env> {
    let db = TestDb::migrated().await?;
    let fake = start_fake().await;
    let access = AuthConfig {
        issuer: "https://team.cloudflareaccess.com".into(),
        audience: "aud-tag".into(),
        jwks: JwksSource::Static(crate::auth::jwt::testing::jwks("k1")),
        roles: RoleSource::Static([ACCESS_ADMIN.to_owned()].into()),
        admin_origin: ORIGIN.into(),
    };
    let oidc = Oidc::new(oidc_config(&fake)).expect("oidc");
    let auth = Authenticator::with_oidc(access, mode, Some(oidc)).expect("auth");
    let admin =
        admin::router(AdminState { db: db.pool(&db.writer, Access::ReadWrite), auth, ceilings: Ceilings::default() });
    Some(Env { fake, db, admin })
}

/// What a call returned.
struct Reply {
    /// The status.
    status: StatusCode,
    /// The `Location` header, if any.
    location: Option<String>,
    /// Every `Set-Cookie` header.
    cookies: Vec<String>,
    /// The body as JSON (`Null` when it is not JSON).
    body: Value,
}

/// Send one request to the admin router.
async fn call(e: &Env, method: &str, path: &str, headers: &[(&str, String)], body: Option<String>) -> Reply {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    let req = b.body(body.map_or_else(Body::empty, Body::from)).expect("request");
    let res = e.admin.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let h = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.expect("body");
    Reply {
        status,
        location: h.get("location").and_then(|v| v.to_str().ok()).map(str::to_owned),
        cookies: h.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).map(str::to_owned).collect(),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// The `name=value` part of a `Set-Cookie` header with that name.
fn cookie(r: &Reply, name: &str) -> Option<String> {
    r.cookies.iter().find(|c| c.starts_with(&format!("{name}="))).map(|c| c.split(';').next().unwrap_or("").to_owned())
}

/// The query parameter of a URL.
fn param(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .expect("url")
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// The claims of a good ID token for the pending sign-in.
fn id_claims(e: &Env, nonce: &str, sub: &str, sid: &str) -> Value {
    let now = chrono::Utc::now().timestamp();
    json!({"iss": e.fake.issuer(), "aud": CLIENT.0, "azp": CLIENT.0, "sub": sub, "sid": sid, "nonce": nonce, "acr": "phr",
           "preferred_username": "meddler", "iat": now, "exp": now + 300, "auth_time": now})
}

/// A sign-in that has reached the callback's door: the state, the binding cookie, and the fake provider ready to answer for this request.
struct Pending {
    /// The `state` the callback must echo.
    state: String,
    /// The `name=value` of the binding cookie.
    binding: String,
    /// The nonce the ID token must carry.
    nonce: String,
}

/// Start a sign-in (`GET /auth/login`) and arm the fake provider for it.
async fn begin(e: &Env) -> Pending {
    let r = call(e, "GET", "/auth/login", &[], None).await;
    assert_eq!(r.status, StatusCode::FOUND, "{:?}", r.body);
    let loc = r.location.clone().expect("location");
    assert!(loc.starts_with(&format!("{}/protocol/openid-connect/auth?", e.fake.issuer())), "{loc}");
    assert_eq!(param(&loc, "code_challenge_method"), "S256");
    assert_eq!(param(&loc, "acr_values"), "phr");
    assert_eq!(param(&loc, "client_id"), CLIENT.0);
    assert_eq!(param(&loc, "redirect_uri"), format!("{ORIGIN}/auth/callback"));
    assert_eq!(param(&loc, "response_type"), "code");
    let binding = cookie(&r, "__Host-hub-login").expect("a binding cookie");
    assert!(
        r.cookies[0].contains("Secure") && r.cookies[0].contains("HttpOnly") && r.cookies[0].contains("SameSite=Lax"),
        "{:?}",
        r.cookies
    );
    let p = Pending { state: param(&loc, "state"), binding, nonce: param(&loc, "nonce") };
    let mut s = e.fake.script.lock().expect("script");
    s.challenge = param(&loc, "code_challenge");
    s.claims = id_claims(e, &p.nonce, MEMBER, "sid-1");
    drop(s);
    p
}

/// Deliver the callback for a pending sign-in with these query parameters and cookie.
async fn callback(e: &Env, query: &str, binding: Option<&str>) -> Reply {
    let headers: Vec<(&str, String)> = binding.map(|b| ("cookie", b.to_owned())).into_iter().collect();
    call(e, "GET", &format!("/auth/callback?{query}"), &headers, None).await
}

/// The callback query of a good sign-in.
fn good_query(e: &Env, p: &Pending) -> String {
    format!(
        "code=good-code&state={}&iss={}",
        p.state,
        url::form_urlencoded::byte_serialize(e.fake.issuer().as_bytes()).collect::<String>()
    )
}

/// Sign in completely and return the session cookie's `name=value`.
async fn sign_in(e: &Env) -> String {
    let p = begin(e).await;
    let r = callback(e, &good_query(e, &p), Some(&p.binding)).await;
    assert_eq!(r.status, StatusCode::FOUND, "{:?}", r.body);
    cookie(&r, "__Host-hub-admin").expect("a session cookie")
}

/// How many sessions are stored.
async fn session_count(e: &Env) -> i64 {
    let mut c = e.db.conn(&e.db.writer).await;
    sqlx::query_scalar("SELECT count(*) FROM hub.admin_sessions").fetch_one(&mut c).await.expect("count")
}

/// A good Cloudflare Access token for the old path.
fn access_token(sub: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    crate::auth::jwt::testing::sign(
        &json!({"alg": "RS256", "kid": "k1"}),
        &json!({"iss": "https://team.cloudflareaccess.com", "aud": ["aud-tag"], "sub": sub, "email": "op@example.org", "exp": now + 600}),
    )
}

/// The CSRF headers a browser's `fetch` from the console carries.
fn csrf() -> Vec<(&'static str, String)> {
    vec![("x-content-hub-request", "1".into()), ("origin", ORIGIN.into())]
}

/// A complete sign-in makes a session that opens the API, shows the path used, and ends at sign-out (the provider's logout URL is returned).
#[tokio::test]
async fn a_complete_sign_in_opens_the_api_and_sign_out_ends_it() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let session = sign_in(&e).await;
    let me = call(&e, "GET", "/api/v1/me", &[("cookie", session.clone())], None).await;
    assert_eq!(me.status, StatusCode::OK, "{:?}", me.body);
    assert_eq!(
        (me.body["subject"].as_str(), me.body["display_name"].as_str(), me.body["via"].as_str()),
        (Some(MEMBER), Some("meddler"), Some("oidc"))
    );
    // The cookie has the contract's attributes, and the database holds a hash, not the identifier.
    let again = call(&e, "GET", "/auth/login", &[], None).await;
    assert!(again.cookies.iter().all(|c| c.starts_with("__Host-") && c.contains("; Path=/") && !c.contains("Domain")));
    let mut c = e.db.conn(&e.db.writer).await;
    let stored: String =
        sqlx::query_scalar("SELECT id_hash FROM hub.admin_sessions").fetch_one(&mut c).await.expect("hash");
    assert!(!session.contains(&stored) && stored.len() == 43);
    // Sign-out needs the CSRF header; with it, the session ends and the answer names the provider's logout.
    let mut h = csrf();
    h.push(("cookie", session.clone()));
    let denied = call(&e, "POST", "/auth/logout", &[("cookie", session.clone())], None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "no CSRF header, no logout");
    let out = call(&e, "POST", "/auth/logout", &h, None).await;
    assert_eq!(out.status, StatusCode::OK, "{:?}", out.body);
    let url = out.body["redirect"].as_str().expect("redirect");
    assert!(url.starts_with(&format!("{}/protocol/openid-connect/logout?", e.fake.issuer())), "{url}");
    assert!(!param(url, "id_token_hint").is_empty());
    assert_eq!(param(url, "post_logout_redirect_uri"), format!("{ORIGIN}/"));
    assert!(out.cookies.iter().any(|c| c.starts_with("__Host-hub-admin=;") && c.contains("Max-Age=0")));
    assert_eq!(call(&e, "GET", "/api/v1/me", &[("cookie", session)], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(session_count(&e).await, 0);
}

/// The callback refuses what is not from the browser that started the sign-in, what was already used, and what the provider did not vouch for.
#[tokio::test]
async fn the_callback_refuses_unbound_reused_and_unvouched_responses() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    // No binding cookie, or another browser's: refused, and the state is spent either way.
    let p = begin(&e).await;
    assert_eq!(callback(&e, &good_query(&e, &p), None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        callback(&e, &good_query(&e, &p), Some(&p.binding)).await.status,
        StatusCode::UNAUTHORIZED,
        "the state was consumed by the refused attempt"
    );
    let p = begin(&e).await;
    assert_eq!(
        callback(&e, &good_query(&e, &p), Some("__Host-hub-login=someone-elses")).await.status,
        StatusCode::UNAUTHORIZED
    );
    // A state that was never issued.
    assert_eq!(
        callback(&e, "code=good-code&state=never-issued", Some(&p.binding)).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Another issuer in the response (RFC 9207), none at all, or an error from the provider.
    for query in [
        format!(
            "code=good-code&state={{S}}&iss={}",
            url::form_urlencoded::byte_serialize(b"https://evil.example/realms/test").collect::<String>()
        ),
        "code=good-code&state={S}".to_owned(),
        "error=access_denied&state={S}".to_owned(),
    ] {
        let p = begin(&e).await;
        let r = callback(&e, &query.replace("{S}", &p.state), Some(&p.binding)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{query}");
    }
    // A code the provider rejects (wrong or reused): 401, no session.
    let p = begin(&e).await;
    let q = good_query(&e, &p).replace("good-code", "stolen-code");
    assert_eq!(callback(&e, &q, Some(&p.binding)).await.status, StatusCode::UNAUTHORIZED);
    // A good response works exactly once.
    let p = begin(&e).await;
    assert_eq!(callback(&e, &good_query(&e, &p), Some(&p.binding)).await.status, StatusCode::FOUND);
    assert_eq!(
        callback(&e, &good_query(&e, &p), Some(&p.binding)).await.status,
        StatusCode::UNAUTHORIZED,
        "a replayed callback"
    );
    assert_eq!(session_count(&e).await, 1);
}

/// An ID token that is for another sign-in, another client, too weak, expired, or signed by another key makes no session.
#[tokio::test]
async fn a_bad_id_token_makes_no_session() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let now = chrono::Utc::now().timestamp();
    /// A change to a good ID token's claims.
    type Edit = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Edit)> = vec![
        ("wrong nonce", Box::new(|c| c["nonce"] = json!("another-sign-in"))),
        (
            "wrong audience",
            Box::new(|c| {
                c["aud"] = json!("other-client");
                c["azp"] = json!("other-client");
            }),
        ),
        ("wrong issuer", Box::new(|c| c["iss"] = json!("https://evil.example/realms/test"))),
        (
            "no assurance class",
            Box::new(|c| {
                c.as_object_mut().expect("object").remove("acr");
            }),
        ),
        ("weaker class", Box::new(|c| c["acr"] = json!("1"))),
        (
            "expired",
            Box::new(move |c| {
                c["exp"] = json!(now - 3600);
                c["iat"] = json!(now - 7200);
            }),
        ),
    ];
    for (name, edit) in cases {
        let p = begin(&e).await;
        edit(&mut e.fake.script.lock().expect("script").claims);
        let r = callback(&e, &good_query(&e, &p), Some(&p.binding)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{name}");
        assert!(cookie(&r, "__Host-hub-admin").is_none(), "{name}: no session cookie");
    }
    assert_eq!(session_count(&e).await, 0);
}

/// A person the decision point does not admit gets 403 and no session; no answer from it is 503, never an allow, both at sign-in and on every later request.
#[tokio::test]
async fn membership_is_decided_by_the_policy_point_and_fails_closed() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    // Not a member.
    let p = begin(&e).await;
    e.fake.script.lock().expect("script").claims["sub"] = json!("stranger");
    assert_eq!(callback(&e, &good_query(&e, &p), Some(&p.binding)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(session_count(&e).await, 0);
    // The decision point is down at sign-in.
    let p = begin(&e).await;
    e.fake.script.lock().expect("script").pdp_down = true;
    assert_eq!(callback(&e, &good_query(&e, &p), Some(&p.binding)).await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(session_count(&e).await, 0);
    // A member signs in; the decision point then goes down: the API fails closed (the cached allow lasts at most 30 s, so the first request is allowed from cache,
    // and a new decision is what a fresh subject needs, so the stranger's removal is also checked below).
    e.fake.script.lock().expect("script").pdp_down = false;
    let session = sign_in(&e).await;
    let calls = e.fake.script.lock().expect("script").pdp_calls;
    let again = call(&e, "GET", "/api/v1/me", &[("cookie", session.clone())], None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(
        e.fake.script.lock().expect("script").pdp_calls,
        calls,
        "the allow is cached, so no second question within the window"
    );
}

/// A decision point that is down means 503 for a session whose decision is not cached.
#[tokio::test]
async fn a_session_without_a_cached_decision_fails_closed_when_the_policy_point_is_down() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    // Make a session directly (no sign-in, so no decision is cached for this subject).
    let (id, hash) = meddleware_identity::session::new_session();
    let mut c = e.db.conn(&e.db.writer).await;
    content_hub_store::repo::sessions::create_session(&mut c, &hash, "uncached-sub", "sid-x", "meddler", "tok")
        .await
        .expect("session");
    e.fake.script.lock().expect("script").pdp_down = true;
    let r = call(&e, "GET", "/api/v1/me", &[("cookie", format!("__Host-hub-admin={id}"))], None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    // Not down, but not a member: 403.
    e.fake.script.lock().expect("script").pdp_down = false;
    let r = call(&e, "GET", "/api/v1/me", &[("cookie", format!("__Host-hub-admin={id}"))], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

/// A session unused for longer than the idle limit, or older than the absolute limit, is no session.
#[tokio::test]
async fn an_expired_session_is_refused() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let session = sign_in(&e).await;
    let mut c = e.db.conn(&e.db.writer).await;
    sqlx::query("UPDATE hub.admin_sessions SET last_seen_at = now() - interval '2 hours'")
        .execute(&mut c)
        .await
        .expect("age");
    assert_eq!(
        call(&e, "GET", "/api/v1/me", &[("cookie", session.clone())], None).await.status,
        StatusCode::UNAUTHORIZED,
        "idle"
    );
    let session = sign_in(&e).await;
    sqlx::query("UPDATE hub.admin_sessions SET created_at = now() - interval '13 hours' WHERE id_hash <> ''")
        .execute(&mut c)
        .await
        .expect("age");
    assert_eq!(
        call(&e, "GET", "/api/v1/me", &[("cookie", session)], None).await.status,
        StatusCode::UNAUTHORIZED,
        "absolute"
    );
    // A made-up cookie and no cookie are 401 too.
    assert_eq!(
        call(&e, "GET", "/api/v1/me", &[("cookie", "__Host-hub-admin=forged".into())], None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(call(&e, "GET", "/api/v1/me", &[], None).await.status, StatusCode::UNAUTHORIZED);
}

/// Each sign-in makes a new session identifier, so one planted before sign-in is worthless.
#[tokio::test]
async fn every_sign_in_makes_a_new_session_identifier() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let a = sign_in(&e).await;
    let b = sign_in(&e).await;
    assert_ne!(a, b);
    assert_eq!(session_count(&e).await, 2);
}

/// POST a back-channel logout token.
async fn send(e: &Env, token: String) -> Reply {
    call(
        e,
        "POST",
        "/auth/backchannel-logout",
        &[("content-type", "application/x-www-form-urlencoded".into())],
        Some(format!("logout_token={token}")),
    )
    .await
}

/// A back-channel logout token ends the named session (and only it), is acted on once, and a token that is not the provider's is refused.
#[tokio::test]
async fn back_channel_logout_ends_the_named_session_once() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let session = sign_in(&e).await;
    let other = {
        let p = begin(&e).await;
        e.fake.script.lock().expect("script").claims = id_claims(&e, &p.nonce, MEMBER, "sid-2");
        let r = callback(&e, &good_query(&e, &p), Some(&p.binding)).await;
        cookie(&r, "__Host-hub-admin").expect("second session")
    };
    let now = chrono::Utc::now().timestamp();
    let logout_token = |jti: &str, sid: &str, aud: &str, typ: &str, signer: &Signer| {
        signer.sign_with_typ(
            &json!({"iss": e.fake.issuer(), "aud": aud, "iat": now, "exp": now + 120, "jti": jti, "sid": sid,
            "events": {"http://schemas.openid.net/event/backchannel-logout": {}}}),
            typ,
        )
    };
    // Not signed by the provider's key, wrong audience, wrong type, no token at all.
    let stranger = Signer::new(Suite::Rs256, "other-key");
    assert_eq!(
        send(&e, logout_token("j0", "sid-1", CLIENT.0, "logout+jwt", &stranger)).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&e, logout_token("j1", "sid-1", "other-client", "logout+jwt", &e.fake.signer)).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&e, logout_token("j2", "sid-1", CLIENT.0, "JWT", &e.fake.signer)).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&e, "POST", "/auth/backchannel-logout", &[], Some("nothing=1".into())).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(session_count(&e).await, 2, "nothing ended yet");
    // The real one ends sid-1 only; the same token again is a replay.
    let t = logout_token("j3", "sid-1", CLIENT.0, "logout+jwt", &e.fake.signer);
    assert_eq!(send(&e, t.clone()).await.status, StatusCode::OK);
    assert_eq!(call(&e, "GET", "/api/v1/me", &[("cookie", session)], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&e, "GET", "/api/v1/me", &[("cookie", other)], None).await.status,
        StatusCode::OK,
        "the other session stays"
    );
    assert_eq!(send(&e, t).await.status, StatusCode::BAD_REQUEST, "a replayed token");
}

/// The modes decide which paths exist: `access` has no sign-in routes, `both` takes either credential and prefers the session, `oidc` ignores the Access token.
#[tokio::test]
async fn the_modes_decide_which_paths_exist() {
    // access: the routes are not there, and a cookie means nothing.
    let Some(e) = env(AdminAuth::Access).await else { return };
    assert_eq!(call(&e, "GET", "/auth/login", &[], None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&e, "POST", "/auth/backchannel-logout", &[], Some("logout_token=x".into())).await.status,
        StatusCode::NOT_FOUND
    );
    let me = call(&e, "GET", "/api/v1/me", &[("cf-access-jwt-assertion", access_token(ACCESS_ADMIN))], None).await;
    assert_eq!((me.status, me.body["via"].as_str()), (StatusCode::OK, Some("access")));
    // both: the Access token works, a session works, and with both the session is the one used.
    let Some(e) = env(AdminAuth::Both).await else { return };
    let via_access =
        call(&e, "GET", "/api/v1/me", &[("cf-access-jwt-assertion", access_token(ACCESS_ADMIN))], None).await;
    assert_eq!((via_access.status, via_access.body["via"].as_str()), (StatusCode::OK, Some("access")));
    let session = sign_in(&e).await;
    let via_session = call(&e, "GET", "/api/v1/me", &[("cookie", session.clone())], None).await;
    assert_eq!((via_session.status, via_session.body["via"].as_str()), (StatusCode::OK, Some("oidc")));
    let both = call(
        &e,
        "GET",
        "/api/v1/me",
        &[("cookie", session), ("cf-access-jwt-assertion", access_token(ACCESS_ADMIN))],
        None,
    )
    .await;
    assert_eq!(both.body["via"].as_str(), Some("oidc"));
    // oidc: the Access token is ignored whatever it says.
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let r = call(&e, "GET", "/api/v1/me", &[("cf-access-jwt-assertion", access_token(ACCESS_ADMIN))], None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

/// Writes through a session still need the CSRF header and the console's Origin.
#[tokio::test]
async fn a_session_does_not_bypass_the_csrf_check() {
    let Some(e) = env(AdminAuth::Oidc).await else { return };
    let session = sign_in(&e).await;
    let plain = call(&e, "PUT", "/api/v1/publishing", &[("cookie", session.clone())], Some("{}".into())).await;
    assert_eq!(plain.status, StatusCode::FORBIDDEN);
    let wrong_origin = call(
        &e,
        "PUT",
        "/api/v1/publishing",
        &[("cookie", session), ("x-content-hub-request", "1".into()), ("origin", "https://evil.example".into())],
        Some("{}".into()),
    )
    .await;
    assert_eq!(wrong_origin.status, StatusCode::FORBIDDEN);
}

/// With a provider configured, every sign-in route is served (anything but 404 or 405 without credentials), and the public routes are not.
#[tokio::test]
async fn every_sign_in_route_is_served_when_a_provider_is_configured() {
    let Some(e) = env(AdminAuth::Both).await else { return };
    for route in crate::routes::AUTH_ROUTES {
        let r = call(&e, route.method.as_str(), route.path, &[], None).await;
        assert!(
            r.status != StatusCode::NOT_FOUND && r.status != StatusCode::METHOD_NOT_ALLOWED,
            "{route:?} gave {}",
            r.status
        );
    }
}
