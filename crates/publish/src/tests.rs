//! Each publisher against a local mock of its platform (documented request and reply shapes; the fixtures in
//! `content_hub_core::platform` name their sources): success, a refused payload, 429, 5xx, a timeout after the
//! request was sent, a connection that was never made, and credential hygiene.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use chrono::{TimeZone, Utc};
use content_hub_core::delivery::Sent;
use content_hub_core::platform::{Kind, x};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;

/// What the mock saw.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Value,
    raw: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A scripted reply: status, extra headers, body, and an optional delay after reading the request.
type Scripted = (u16, Vec<(&'static str, String)>, String, Duration);

#[derive(Clone)]
struct Script(Arc<dyn Fn(&str) -> Scripted + Send + Sync>);

async fn handle(
    State((log, script)): State<(Log, Script)>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
    log.lock().expect("log").push(Seen {
        path: path.clone(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        raw: body.to_vec(),
    });
    let (status, extra, text, delay) = (script.0)(&path);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut h = HeaderMap::new();
    for (k, v) in extra {
        h.insert(k, v.parse().expect("header"));
    }
    h.insert("content-type", "application/json".parse().expect("ct"));
    (StatusCode::from_u16(status).expect("status"), h, text)
}

/// Start a mock; returns its base URL and the request log.
async fn mock(script: impl Fn(&str) -> Scripted + Send + Sync + 'static) -> (String, Log) {
    let log: Log = Arc::default();
    let app = Router::new().fallback(handle).with_state((log.clone(), Script(Arc::new(script))));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(l, app).await.expect("serve") });
    (format!("http://{addr}"), log)
}

fn ok(status: u16, body: &str) -> Scripted {
    (status, vec![], body.to_owned(), Duration::ZERO)
}

fn slow() -> Scripted {
    (200, vec![], "{}".into(), Duration::from_secs(3))
}

fn http() -> Http {
    Http::unchecked_for_tests(Duration::from_millis(600))
}

/// A port nothing listens on.
async fn closed_port() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let a = l.local_addr().expect("addr");
    drop(l);
    format!("http://{a}")
}

const KEY: Uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
const POST: Uuid = Uuid::from_u128(42);

fn ctx<'a>(kind: Kind, base: &'a str, ext: Option<&'a str>, config: &'a Value, secret: Option<&'a str>) -> Context<'a> {
    Context {
        kind,
        base_url: Some(base),
        external_ref: ext,
        config,
        secret,
        crosspost_secret: None,
        title: "Title",
        body: "Hello @everyone https://example.org/a",
        post_public_id: POST,
        idempotency_key: KEY,
        first_attempt_at: Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).single().expect("t"),
        now: Utc.with_ymd_and_hms(2026, 10, 29, 12, 1, 0).single().expect("t"),
    }
}

fn no_secret_in(r: &Report, secret: &str) {
    let all = format!("{} {:?} {:?}", r.request, r.excerpt, r.sent);
    assert!(!all.contains(secret), "the credential leaked: {all}");
}

// ── Discord ──────────────────────────────────────────────────────────────────────────────────────────────

const DISCORD_OK: &str = r#"{"id":"1300000000000000001","channel_id":"1200000000000000002","content":"x"}"#;

#[tokio::test]
async fn discord_posts_without_mentions_and_records_no_token() {
    let (base, log) = mock(|p| if p.contains("crosspost") { ok(200, "{}") } else { ok(200, DISCORD_OK) }).await;
    let hook = format!("{base}/api/webhooks/123/SeCrEtToKeN_value-1");
    let cfg = json!({"guild_id": "1100000000000000003"});
    let mut c = ctx(Kind::Discord, &base, None, &cfg, Some(&hook));
    c.crosspost_secret = Some("BOT.TOKEN.VALUE");
    let r = send(&http(), &c).await;
    assert_eq!(
        r.sent,
        Sent::Posted {
            url_or_external_id:
                "https://discord.com/channels/1100000000000000003/1200000000000000002/1300000000000000001".into()
        }
    );
    let seen = log.lock().expect("log").clone();
    assert_eq!(seen[0].path, "/api/webhooks/123/SeCrEtToKeN_value-1?wait=true");
    assert_eq!(seen[0].body["allowed_mentions"], json!({"parse": []}));
    assert_eq!(seen[0].body["content"], "Hello @everyone https://example.org/a");
    // The crosspost went to the created message, with the bot token.
    assert_eq!(seen[1].path, "/api/v10/channels/1200000000000000002/messages/1300000000000000001/crosspost");
    assert_eq!(seen[1].headers["authorization"], "Bot BOT.TOKEN.VALUE");
    no_secret_in(&r, "SeCrEtToKeN");
    assert_eq!(r.request["url"], format!("{base}/api/webhooks/123/[redacted]?wait=true"));
}

#[tokio::test]
async fn discord_outcomes() {
    let cfg = json!({});
    for (script, want) in [
        (ok(400, r#"{"code":50035,"message":"Invalid Form Body"}"#), "validation"),
        (ok(404, r#"{"code":10015,"message":"Unknown Webhook"}"#), "refused"),
        (
            (
                429,
                vec![],
                r#"{"message":"You are being rate limited.","retry_after":1.5,"global":false}"#.into(),
                Duration::ZERO,
            ),
            "retry",
        ),
        (ok(503, "upstream"), "unknown"),
        (slow(), "unknown"),
    ] {
        let s = script.clone();
        let (base, _) = mock(move |_| s.clone()).await;
        let hook = format!("{base}/api/webhooks/1/tok");
        let r = send(&http(), &ctx(Kind::Discord, &base, None, &cfg, Some(&hook))).await;
        let got = match r.sent {
            Sent::Refused { validation: true, .. } => "validation",
            Sent::Refused { .. } => "refused",
            Sent::Retry { .. } => "retry",
            Sent::Unknown { .. } => "unknown",
            Sent::Posted { .. } => "posted",
        };
        assert_eq!(got, want, "{:?}", script.0);
    }
    // Never connected: provably not sent.
    let base = closed_port().await;
    let hook = format!("{base}/api/webhooks/1/tok");
    assert!(matches!(
        send(&http(), &ctx(Kind::Discord, &base, None, &cfg, Some(&hook))).await.sent,
        Sent::Retry { .. }
    ));
    // A credential that is not a webhook URL on the platform's host is refused without a request.
    let (base, log) = mock(|_| ok(200, DISCORD_OK)).await;
    let r = send(&http(), &ctx(Kind::Discord, &base, None, &cfg, Some("postgres://u:hunter2@db/x"))).await;
    assert!(matches!(r.sent, Sent::Refused { validation: false, .. }));
    no_secret_in(&r, "hunter2");
    assert!(log.lock().expect("log").is_empty());
    assert!(matches!(send(&http(), &ctx(Kind::Discord, &base, None, &cfg, None)).await.sent, Sent::Refused { .. }));
}

// ── Telegram ─────────────────────────────────────────────────────────────────────────────────────────────

const TG_TOKEN: &str = "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11";

#[tokio::test]
async fn telegram_sends_html_and_never_records_the_token() {
    let (base, log) = mock(|_| {
        ok(200, r#"{"ok":true,"result":{"message_id":9,"chat":{"id":-1001,"type":"channel","username":"chan_name"}}}"#)
    })
    .await;
    let cfg = json!({"disable_link_preview": true});
    let r = send(&http(), &ctx(Kind::Telegram, &base, Some("@chan_name"), &cfg, Some(TG_TOKEN))).await;
    assert_eq!(r.sent, Sent::Posted { url_or_external_id: "https://t.me/chan_name/9".into() });
    let seen = log.lock().expect("log").clone();
    assert_eq!(seen[0].path, format!("/bot{TG_TOKEN}/sendMessage"));
    assert_eq!(seen[0].body["parse_mode"], "HTML");
    assert_eq!(seen[0].body["link_preview_options"], json!({"is_disabled": true}));
    no_secret_in(&r, "ABC-DEF1234ghIkl");
}

#[tokio::test]
async fn telegram_outcomes_and_scrubbed_errors() {
    let cfg = json!({});
    let (base, _) = mock(|_| {
        (429, vec![], r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#.into(), Duration::ZERO)
    })
    .await;
    match send(&http(), &ctx(Kind::Telegram, &base, Some("@chan_name"), &cfg, Some(TG_TOKEN))).await.sent {
        Sent::Retry { after: Some(d), rate_limited: true, .. } => assert_eq!(d.as_secs(), 3),
        other => panic!("{other:?}"),
    }
    // A reply that echoes the token is scrubbed before it is kept.
    let (base, _) =
        mock(|_| ok(401, &format!(r#"{{"ok":false,"error_code":401,"description":"Unauthorized {TG_TOKEN}"}}"#))).await;
    let r = send(&http(), &ctx(Kind::Telegram, &base, Some("@chan_name"), &cfg, Some(TG_TOKEN))).await;
    assert!(matches!(r.sent, Sent::Refused { validation: false, .. }));
    no_secret_in(&r, "ABC-DEF1234ghIkl");
    let (base, _) = mock(|_| slow()).await;
    assert!(matches!(
        send(&http(), &ctx(Kind::Telegram, &base, Some("@chan_name"), &cfg, Some(TG_TOKEN))).await.sent,
        Sent::Unknown { .. }
    ));
    let (base, log) = mock(|_| ok(200, "{}")).await;
    let r = send(&http(), &ctx(Kind::Telegram, &base, Some("@chan_name"), &cfg, Some("DATABASE-URL-VALUE"))).await;
    assert!(matches!(r.sent, Sent::Refused { .. }));
    assert!(log.lock().expect("log").is_empty(), "a non-token credential is never put in a URL");
}

// ── Bluesky ──────────────────────────────────────────────────────────────────────────────────────────────

fn bsky(record: Scripted) -> impl Fn(&str) -> Scripted + Send + Sync + 'static {
    move |p: &str| {
        if p.ends_with("createSession") {
            ok(200, r#"{"did":"did:plc:abc","handle":"a.example","accessJwt":"ACCESS.JWT.VALUE","refreshJwt":"r"}"#)
        } else {
            record.clone()
        }
    }
}

#[tokio::test]
async fn bluesky_logs_in_then_writes_a_record_with_facets_under_a_fixed_key() {
    let (base, log) = mock(bsky(ok(200, r#"{"uri":"at://did:plc:abc/app.bsky.feed.post/KEY","cid":"bafy"}"#))).await;
    let cfg = json!({"pds": base});
    let c = ctx(Kind::Bluesky, &base, Some("a.example"), &cfg, Some("app-pass-word-1234"));
    let r = send(&http(), &c).await;
    assert_eq!(r.sent, Sent::Posted { url_or_external_id: "https://bsky.app/profile/did:plc:abc/post/KEY".into() });
    let seen = log.lock().expect("log").clone();
    assert_eq!(seen[0].body, json!({"identifier": "a.example", "password": "app-pass-word-1234"}));
    assert_eq!(seen[1].headers["authorization"], "Bearer ACCESS.JWT.VALUE");
    let rec = &seen[1].body;
    assert_eq!(rec["repo"], "did:plc:abc");
    assert_eq!(
        rec["rkey"],
        content_hub_core::platform::bluesky::rkey(c.first_attempt_at.timestamp_micros() as u64, KEY.as_bytes())
    );
    let start = rec["record"]["facets"][0]["index"]["byteStart"].as_u64().expect("facet") as usize;
    assert_eq!(&c.body[start..], "https://example.org/a");
    no_secret_in(&r, "app-pass-word-1234");
    no_secret_in(&r, "ACCESS.JWT.VALUE");
}

#[tokio::test]
async fn bluesky_failures_before_posting_retry_and_after_are_unknown() {
    let cfg_for = |b: &str| json!({"pds": b});
    // Login refused: nothing to retry.
    let (base, _) =
        mock(|_| ok(401, r#"{"error":"AuthenticationRequired","message":"Invalid identifier or password"}"#)).await;
    let cfg = cfg_for(&base);
    assert!(matches!(
        send(&http(), &ctx(Kind::Bluesky, &base, Some("a"), &cfg, Some("pw"))).await.sent,
        Sent::Refused { .. }
    ));
    // Login timed out: nothing was posted, so a retry is safe.
    let (base, _) = mock(|_| slow()).await;
    let cfg = cfg_for(&base);
    assert!(matches!(
        send(&http(), &ctx(Kind::Bluesky, &base, Some("a"), &cfg, Some("pw"))).await.sent,
        Sent::Retry { .. }
    ));
    // The record step timed out or failed on the server: it may exist.
    let (base, _) = mock(bsky(slow())).await;
    let cfg = cfg_for(&base);
    assert!(matches!(
        send(&http(), &ctx(Kind::Bluesky, &base, Some("a"), &cfg, Some("pw"))).await.sent,
        Sent::Unknown { .. }
    ));
    let (base, _) = mock(bsky(ok(502, ""))).await;
    let cfg = cfg_for(&base);
    assert!(matches!(
        send(&http(), &ctx(Kind::Bluesky, &base, Some("a"), &cfg, Some("pw"))).await.sent,
        Sent::Unknown { .. }
    ));
    let (base, _) = mock(bsky(ok(
        400,
        r#"{"error":"InvalidRequest","message":"Record/text must not be longer than 300 graphemes"}"#,
    )))
    .await;
    let cfg = cfg_for(&base);
    assert!(matches!(
        send(&http(), &ctx(Kind::Bluesky, &base, Some("a"), &cfg, Some("pw"))).await.sent,
        Sent::Refused { validation: true, .. }
    ));
}

// ── Mastodon ─────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mastodon_sends_the_idempotency_key_and_repeats_only_inside_its_window() {
    let (base, log) =
        mock(|_| ok(200, r#"{"id":"1","uri":"https://m.example/users/a/statuses/1","url":"https://m.example/@a/1"}"#))
            .await;
    let cfg = json!({"visibility": "unlisted"});
    let r = send(&http(), &ctx(Kind::Mastodon, &base, Some(&base), &cfg, Some("MASTODON-TOKEN-VALUE"))).await;
    assert_eq!(r.sent, Sent::Posted { url_or_external_id: "https://m.example/@a/1".into() });
    let seen = log.lock().expect("log").clone();
    assert_eq!(seen[0].path, "/api/v1/statuses");
    assert_eq!(seen[0].headers["idempotency-key"], KEY.to_string());
    assert_eq!(seen[0].headers["authorization"], "Bearer MASTODON-TOKEN-VALUE");
    assert_eq!(seen[0].body["visibility"], "unlisted");
    no_secret_in(&r, "MASTODON-TOKEN-VALUE");

    let late = |c: &mut Context<'_>| c.now = c.first_attempt_at + chrono::Duration::minutes(50);
    for (script, inside, outside) in [(ok(503, ""), "retry", "unknown"), (slow(), "retry", "unknown")] {
        let s = script.clone();
        let (base, _) = mock(move |_| s.clone()).await;
        let mut c = ctx(Kind::Mastodon, &base, Some(&base), &cfg, Some("t"));
        let name = |s: &Sent| match s {
            Sent::Retry { .. } => "retry",
            Sent::Unknown { .. } => "unknown",
            _ => "other",
        };
        assert_eq!(name(&send(&http(), &c).await.sent), inside);
        late(&mut c);
        assert_eq!(name(&send(&http(), &c).await.sent), outside);
    }
    let (base, _) = mock(|_| ok(422, r#"{"error":"Validation failed: Text character limit of 500 exceeded"}"#)).await;
    assert!(matches!(
        send(&http(), &ctx(Kind::Mastodon, &base, Some(&base), &cfg, Some("t"))).await.sent,
        Sent::Refused { validation: true, .. }
    ));
}

#[tokio::test]
async fn mastodon_instance_limits() {
    let (base, log) = mock(|_| {
        ok(200, r#"{"configuration":{"statuses":{"max_characters":11000,"characters_reserved_per_url":23}}}"#)
    })
    .await;
    let l = mastodon_limits(&http(), &base).await.expect("limits");
    assert_eq!((l.max_characters, l.characters_reserved_per_url), (11000, 23));
    assert_eq!(log.lock().expect("log")[0].path, "/api/v2/instance");
}

// ── X ────────────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_documented_oauth1_signature() {
    let params = [
        ("status", "Hello Ladies + Gentlemen, a signed OAuth request!"),
        ("include_entities", "true"),
        ("oauth_consumer_key", "xvz1evFS4wEEPTGEFPHBog"),
        ("oauth_nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg"),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", "1318622958"),
        ("oauth_token", "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb"),
        ("oauth_version", "1.0"),
    ];
    let base = x::signature_base("POST", "https://api.twitter.com/1.1/statuses/update.json", &params);
    let key =
        x::signing_key("kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw", "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE");
    assert_eq!(oauth1_signature(&key, &base), "hCtSmYh+iHYCEqBWrE7C7hYmtUk=");
}

#[tokio::test]
async fn x_signs_with_oauth1() {
    let (base, log) = mock(|_| ok(201, r#"{"data":{"id":"1445880548472328192","text":"x"}}"#)).await;
    let cred = r#"{"consumer_key":"ck","consumer_secret":"CONSUMER-SECRET","access_token":"at","access_token_secret":"TOKEN-SECRET"}"#;
    let r = send(&http(), &ctx(Kind::X, &base, None, &json!({}), Some(cred))).await;
    assert_eq!(r.sent, Sent::Posted { url_or_external_id: "https://x.com/i/web/status/1445880548472328192".into() });
    let seen = log.lock().expect("log").clone();
    let auth = seen[0].headers["authorization"].to_str().expect("header");
    assert!(
        auth.starts_with("OAuth ") && auth.contains("oauth_signature=") && auth.contains(r#"oauth_consumer_key="ck""#)
    );
    assert!(!auth.contains("CONSUMER-SECRET") && !auth.contains("TOKEN-SECRET"));
    no_secret_in(&r, "CONSUMER-SECRET");
    let r = send(&http(), &ctx(Kind::X, &base, None, &json!({}), Some("not json"))).await;
    assert!(matches!(r.sent, Sent::Refused { .. }));
}

// ── the feed and dry runs ────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_feed_needs_no_call() {
    let r = send(&http(), &ctx(Kind::Feed, "unused", None, &json!({}), None)).await;
    assert_eq!(r.sent, Sent::Posted { url_or_external_id: format!("urn:uuid:{POST}") });
}

#[test]
fn dry_runs_describe_the_request_without_credentials() {
    let cfg = json!({});
    let hook = "https://discord.com/api/webhooks/77/DRY-RUN-TOKEN";
    let d = dry_run(&ctx(Kind::Discord, "https://discord.com", None, &cfg, Some(hook))).expect("dry run");
    assert_eq!(d["url"], "https://discord.com/api/webhooks/77/[redacted]?wait=true");
    assert!(!d.to_string().contains("DRY-RUN-TOKEN"));
    let d = dry_run(&ctx(Kind::Discord, "https://discord.com", None, &cfg, None)).expect("dry run without the secret");
    assert!(d["url"].as_str().expect("url").contains("credential variable"));
    let d = dry_run(&ctx(Kind::Telegram, "https://api.telegram.org", Some("@chan_name"), &cfg, Some(TG_TOKEN)))
        .expect("tg");
    assert_eq!(d["url"], "https://api.telegram.org/bot[redacted]/sendMessage");
    assert!(dry_run(&ctx(Kind::Mastodon, "x", Some("not a url"), &cfg, None)).is_err());
    let d = dry_run(&ctx(Kind::Bluesky, "https://bsky.social", Some("a.example"), &cfg, Some("pw"))).expect("bsky");
    assert_eq!(d["steps"][0]["body"]["password"], "[redacted]");
    assert!(d["expected_url"].as_str().expect("url").starts_with("https://bsky.app/profile/a.example/post/"));
}

// ── the Walrus publisher (BlobStore) ─────────────────────────────────────────────────────────────────────

mod publisher {
    use super::*;
    use crate::blob::{BlobError, BlobStore, Publisher, StoredBlob};
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    const BLOB: &str = "Qc9aP0V6x3mYk2w1TnqL8sRZb4dEoFhJiUyXvCgA7MM";

    fn created() -> String {
        format!(
            r#"{{"newlyCreated":{{"blobObject":{{"id":"0x{}","blobId":"{BLOB}","size":5,"storage":{{"startEpoch":5,"endEpoch":35}}}}}}}}"#,
            "ab".repeat(32)
        )
    }

    #[tokio::test]
    async fn a_blob_is_sent_raw_with_a_token_bound_to_its_epochs_and_size() {
        let (base, log) = mock(|_| ok(200, &created())).await;
        let p = Publisher::with_client(http(), &base, Some("0x00ff00ff")).expect("publisher");
        let stored = p.put(b"hello", 30).await.expect("stored");
        assert_eq!(
            stored,
            StoredBlob {
                blob_id: BLOB.into(),
                object_id: Some(format!("0x{}", "ab".repeat(32))),
                end_epoch: Some(35),
                already_certified: false
            }
        );
        let seen = log.lock().expect("log").clone();
        assert_eq!(seen[0].path, "/v1/blobs?epochs=30&permanent=true");
        assert_eq!(seen[0].raw, b"hello");
        assert_eq!(seen[0].headers["content-type"], "application/octet-stream");
        let auth = seen[0].headers["authorization"].to_str().expect("header").to_owned();
        let token = auth.strip_prefix("Bearer ").expect("bearer");
        let parts: Vec<&str> = token.split('.').collect();
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("payload")).expect("json");
        assert_eq!((claims["epochs"].as_u64(), claims["size"].as_u64()), (Some(30), Some(5)));
        assert_eq!(claims["exp"].as_i64().expect("exp") - claims["iat"].as_i64().expect("iat"), 60);
        assert!(claims["jti"].as_str().expect("jti").len() >= 16);
        // A second store uses a different token id (a token cannot be replayed).
        p.put(b"hello", 30).await.expect("again");
        let again = log.lock().expect("log")[1].headers["authorization"].to_str().expect("h").to_owned();
        assert_ne!(again, auth);
    }

    #[tokio::test]
    async fn no_secret_means_no_token_and_refusals_are_explained() {
        let (base, log) = mock(|_| ok(200, &created())).await;
        Publisher::with_client(http(), &base, None).expect("p").put(b"x", 1).await.expect("stored");
        assert!(log.lock().expect("log")[0].headers.get("authorization").is_none());
        let (base, _) = mock(|_| ok(401, "invalid token")).await;
        match Publisher::with_client(http(), &base, Some("s")).expect("p").put(b"x", 1).await {
            Err(BlobError::Rejected { status: 401, reason }) => assert!(reason.contains("invalid token")),
            other => panic!("{other:?}"),
        }
        let (base, _) = mock(|_| ok(200, "not json")).await;
        assert!(matches!(
            Publisher::with_client(http(), &base, None).expect("p").put(b"x", 1).await,
            Err(BlobError::Unexpected(_))
        ));
        let base = closed_port().await;
        assert!(matches!(
            Publisher::with_client(http(), &base, None).expect("p").put(b"x", 1).await,
            Err(BlobError::Unreachable(_))
        ));
    }

    #[tokio::test]
    async fn the_internal_client_calls_only_its_own_host() {
        // The production constructor builds a client that refuses every other host before connecting.
        let p = Publisher::new("http://publisher.example.svc:31415", None).expect("publisher");
        let _ = p; // construction is the check; the refusal is tested on the client itself below.
        let client = crate::net::Http::internal("publisher.example.svc").expect("client");
        match client.send(Request::get("http://other.example.svc/v1".into())).await {
            Err(Failure::Refused(m)) => assert!(m.contains("only its configured internal host"), "{m}"),
            other => panic!("{other:?}"),
        }
        match client.send(Request::get("ftp://publisher.example.svc/".into())).await {
            Err(Failure::Refused(_)) => {}
            other => panic!("{other:?}"),
        }
        assert!(crate::net::Http::internal("a/b").is_err());
        assert!(crate::net::Http::internal("").is_err());
    }
}
