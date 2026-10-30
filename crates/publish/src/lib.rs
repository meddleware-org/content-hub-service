//! Platform publishers (ARCHITECTURE.md §10): the HTTP half of each platform. Request shapes and the
//! classification of replies are pure and live in `content_hub_core::platform`; this crate sends, through the checked
//! client in [`net`].
//!
//! Every publisher:
//!
//! - sends exactly the payload it is given (validated and stored at enqueue), once per call;
//! - reports `Unknown` whenever the request may have taken effect without a confirmation, and `Retry` only when it
//!   provably did not (or the platform documents the repeat as idempotent);
//! - never returns a credential: the recorded request is redacted and reply excerpts are scrubbed of the secret.
//!
//! [`dry_run`] builds the same request without sending it, for the attempt record of a target that is not live.

#![forbid(unsafe_code)]

pub mod blob;
pub mod net;

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use content_hub_core::delivery::Sent;
use content_hub_core::platform::{Kind, Reply, bluesky, discord, excerpt, mastodon, scrub_sent, telegram, x};
use serde_json::{Value, json};
use uuid::Uuid;

pub use net::{Failure, Http, Request};

/// Everything one send needs. The worker resolves the credential from the environment by the target's `secret_ref`.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub kind: Kind,
    /// The definition's `base_url` (e.g. `https://discord.com`, `https://api.telegram.org`).
    pub base_url: Option<&'a str>,
    pub external_ref: Option<&'a str>,
    pub config: &'a Value,
    /// The credential's value (never logged, stored or returned).
    pub secret: Option<&'a str>,
    /// Discord only: the crosspost bot token, when the target asks for crossposting.
    pub crosspost_secret: Option<&'a str>,
    pub title: &'a str,
    pub body: &'a str,
    pub post_public_id: Uuid,
    pub idempotency_key: Uuid,
    pub first_attempt_at: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

/// What a send produced, for the attempt record.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub sent: Sent,
    pub http_status: Option<u16>,
    /// At most 500 characters of the last reply, scrubbed of the credential.
    pub excerpt: Option<String>,
    /// The request(s) as sent, with credentials replaced by `[redacted]`.
    pub request: Value,
}

fn refused(why: impl Into<String>, request: Value) -> Report {
    Report { sent: Sent::Refused { reason: why.into(), validation: false }, http_status: None, excerpt: None, request }
}

fn missing_secret() -> String {
    "the target's credential variable is not set in the worker's environment".to_owned()
}

/// The request the send would make, redacted (for a dry run). `Err` when it could not even be built.
pub fn dry_run(ctx: &Context<'_>) -> Result<Value, String> {
    let r = match ctx.kind {
        Kind::Feed => {
            json!({ "feed": "the post appears in the owned feeds", "id": format!("urn:uuid:{}", ctx.post_public_id) })
        }
        Kind::Discord => {
            let base = ctx.base_url.ok_or("the platform has no base_url")?;
            let url = match ctx.secret {
                Some(s) => discord::redacted_url(&discord::webhook_url(base, s)?),
                None => "[the webhook URL from the target's credential variable]".to_owned(),
            };
            let body = discord::body(discord::Mode::from_config(ctx.config), ctx.title, ctx.body, ctx.config);
            json!({ "method": "POST", "url": url, "body": body })
        }
        Kind::Telegram => {
            let base = ctx.base_url.ok_or("the platform has no base_url")?;
            let chat = telegram::chat_id(ctx.external_ref)?;
            json!({ "method": "POST", "url": telegram::redacted_url(base, "sendMessage"),
                    "body": telegram::body(&chat, ctx.body, ctx.config) })
        }
        Kind::Bluesky => {
            let pds = bluesky_pds(ctx)?;
            let id = ctx
                .external_ref
                .filter(|s| !s.trim().is_empty())
                .ok_or("external_ref must be the account's handle or DID")?;
            let rkey = bluesky_rkey(ctx);
            json!({ "steps": [
                { "method": "POST", "url": bluesky::session_url(&pds), "body": { "identifier": id, "password": "[redacted]" } },
                { "method": "POST", "url": bluesky::create_record_url(&pds), "headers": { "authorization": "Bearer [redacted]" },
                  "body": bluesky::record_body("[the account's DID]", &rkey, ctx.body, &rfc3339(ctx.now), ctx.config) },
            ], "expected_url": bluesky::post_url(id, &rkey) })
        }
        Kind::Mastodon => {
            let base = mastodon::base(ctx.external_ref)?;
            json!({ "method": "POST", "url": mastodon::statuses_url(&base),
                    "headers": { "authorization": "Bearer [redacted]", "idempotency-key": ctx.idempotency_key.to_string() },
                    "body": mastodon::body(ctx.body, ctx.config) })
        }
        Kind::X => {
            let base = ctx.base_url.ok_or("the platform has no base_url")?;
            json!({ "method": "POST", "url": format!("{}{}", base.trim_end_matches('/'), x::CREATE_PATH),
                    "headers": { "authorization": "OAuth [redacted]" }, "body": x::body(ctx.body) })
        }
    };
    Ok(r)
}

/// Send the post once.
pub async fn send(http: &Http, ctx: &Context<'_>) -> Report {
    match ctx.kind {
        Kind::Feed => Report {
            sent: Sent::Posted { url_or_external_id: format!("urn:uuid:{}", ctx.post_public_id) },
            http_status: None,
            excerpt: None,
            request: dry_run(ctx).unwrap_or(Value::Null),
        },
        Kind::Discord => send_discord(http, ctx).await,
        Kind::Telegram => send_telegram(http, ctx).await,
        Kind::Bluesky => send_bluesky(http, ctx).await,
        Kind::Mastodon => send_mastodon(http, ctx).await,
        Kind::X => send_x(http, ctx).await,
    }
}

fn report(r: &Reply, sent: Sent, secret: Option<&str>, request: Value) -> Report {
    Report {
        sent: scrub_sent(sent, secret),
        http_status: Some(r.status),
        excerpt: Some(excerpt(&r.body, secret)),
        request,
    }
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn send_discord(http: &Http, ctx: &Context<'_>) -> Report {
    let request = dry_run(ctx).unwrap_or(Value::Null);
    let (Some(base), Some(secret)) = (ctx.base_url, ctx.secret) else { return refused(missing_secret(), request) };
    let url = match discord::webhook_url(base, secret) {
        Ok(u) => u,
        Err(e) => return refused(e, request),
    };
    let body = discord::body(discord::Mode::from_config(ctx.config), ctx.title, ctx.body, ctx.config);
    let reply = match http.send(Request::post(url, body)).await {
        Ok(r) => r,
        Err(f) => return Report { sent: f.to_sent(), http_status: None, excerpt: None, request },
    };
    let sent = discord::classify(&reply, ctx.config);
    // Best effort: publish an announcement-channel message to following servers. The post already went out, so a
    // failure here never changes the outcome; it is logged.
    if let (Sent::Posted { .. }, Some(bot)) = (&sent, ctx.crosspost_secret)
        && let Some((channel, id)) = discord::created(&reply)
    {
        let r = Request::post(discord::crosspost_url(base, &channel, &id), json!({}))
            .header("authorization", format!("Bot {bot}"));
        match http.send(r).await {
            Ok(r) if r.is_success() => tracing::info!(message = %id, "crossposted"),
            Ok(r) => tracing::warn!(status = r.status, "crosspost refused"),
            Err(f) => tracing::warn!(failure = ?f, "crosspost failed"),
        }
    }
    report(&reply, sent, Some(secret), request)
}

async fn send_telegram(http: &Http, ctx: &Context<'_>) -> Report {
    let request = dry_run(ctx).unwrap_or(Value::Null);
    let (Some(base), Some(token)) = (ctx.base_url, ctx.secret) else { return refused(missing_secret(), request) };
    if !telegram::is_token(token) {
        return refused("the credential is not a Telegram bot token", request);
    }
    let chat = match telegram::chat_id(ctx.external_ref) {
        Ok(c) => c,
        Err(e) => return refused(e, request),
    };
    let r =
        Request::post(telegram::method_url(base, token, "sendMessage"), telegram::body(&chat, ctx.body, ctx.config));
    match http.send(r).await {
        Ok(reply) => report(&reply, telegram::classify(&reply, &chat), Some(token), request),
        Err(f) => Report { sent: f.to_sent(), http_status: None, excerpt: None, request },
    }
}

fn bluesky_pds(ctx: &Context<'_>) -> Result<String, String> {
    let pds = ctx.config.get("pds").and_then(Value::as_str).or(ctx.base_url).ok_or("no PDS: set config.pds")?;
    if !pds.starts_with("https://") && !pds.starts_with("http://") {
        return Err("config.pds must be an https URL".into());
    }
    Ok(pds.trim_end_matches('/').to_owned())
}

fn bluesky_rkey(ctx: &Context<'_>) -> String {
    let micros = u64::try_from(ctx.first_attempt_at.timestamp_micros()).unwrap_or(0);
    bluesky::rkey(micros, ctx.idempotency_key.as_bytes())
}

async fn send_bluesky(http: &Http, ctx: &Context<'_>) -> Report {
    let request = dry_run(ctx).unwrap_or(Value::Null);
    let Some(password) = ctx.secret else { return refused(missing_secret(), request) };
    let (pds, id) = match (bluesky_pds(ctx), ctx.external_ref.filter(|s| !s.trim().is_empty())) {
        (Ok(p), Some(i)) => (p, i.trim()),
        (Err(e), _) => return refused(e, request),
        (_, None) => return refused("external_ref must be the account's handle or DID", request),
    };
    // 1. Log in. Nothing is posted by this step, so every failure here is a retry or a refusal.
    let session = match http.send(Request::post(bluesky::session_url(&pds), bluesky::session_body(id, password))).await
    {
        Ok(reply) => match bluesky::classify_session(&reply) {
            Ok(s) => s,
            Err(sent) => return report(&reply, sent, Some(password), request),
        },
        Err(f) => return Report { sent: f.before_posting(), http_status: None, excerpt: None, request },
    };
    // 2. Write the record, under a key fixed for this instance.
    let rkey = bluesky_rkey(ctx);
    let body = bluesky::record_body(&session.did, &rkey, ctx.body, &rfc3339(ctx.now), ctx.config);
    let r = Request::post(bluesky::create_record_url(&pds), body)
        .header("authorization", format!("Bearer {}", session.access_jwt));
    match http.send(r).await {
        Ok(reply) => {
            let sent = bluesky::classify_record(&reply, &session.did, &rkey);
            let mut rep = report(&reply, sent, Some(password), request);
            rep.excerpt = rep.excerpt.map(|e| e.replace(&session.access_jwt, "[redacted]"));
            rep.sent = scrub_sent(rep.sent, Some(&session.access_jwt));
            rep
        }
        Err(f) => Report { sent: f.to_sent(), http_status: None, excerpt: None, request },
    }
}

async fn send_mastodon(http: &Http, ctx: &Context<'_>) -> Report {
    let request = dry_run(ctx).unwrap_or(Value::Null);
    let Some(token) = ctx.secret else { return refused(missing_secret(), request) };
    let base = match mastodon::base(ctx.external_ref) {
        Ok(b) => b,
        Err(e) => return refused(e, request),
    };
    let window = mastodon::window_open(ctx.first_attempt_at, ctx.now);
    let r = Request::post(mastodon::statuses_url(&base), mastodon::body(ctx.body, ctx.config))
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", ctx.idempotency_key.to_string());
    match http.send(r).await {
        Ok(reply) => report(&reply, mastodon::classify(&reply, window, ctx.now), Some(token), request),
        // With the same Idempotency-Key a repeat cannot post twice while the key is kept.
        Err(Failure::Unknown(why)) if window => Report {
            sent: Sent::Retry {
                reason: format!("{why} (repeated with the same Idempotency-Key)"),
                after: None,
                rate_limited: false,
            },
            http_status: None,
            excerpt: None,
            request,
        },
        Err(f) => Report { sent: f.to_sent(), http_status: None, excerpt: None, request },
    }
}

/// A Mastodon instance's limits (`GET /api/v2/instance`, public).
pub async fn mastodon_limits(http: &Http, base: &str) -> Result<mastodon::InstanceLimits, String> {
    let reply = http.send(Request::get(mastodon::instance_url(base))).await.map_err(|f| format!("{f:?}"))?;
    if !reply.is_success() {
        return Err(format!("HTTP {}", reply.status));
    }
    mastodon::parse_instance_limits(&reply.body).ok_or_else(|| "the instance did not describe its limits".into())
}

/// HMAC-SHA1 of `base` under `key`, base64 (OAuth 1.0a).
pub fn oauth1_signature(key: &str, base: &str) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(ring::hmac::sign(&k, base.as_bytes()).as_ref())
}

fn nonce() -> String {
    let mut b = [0u8; 24];
    if getrandom::fill(&mut b).is_err() {
        return Uuid::new_v4().simple().to_string();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn send_x(http: &Http, ctx: &Context<'_>) -> Report {
    let request = dry_run(ctx).unwrap_or(Value::Null);
    let (Some(base), Some(secret)) = (ctx.base_url, ctx.secret) else { return refused(missing_secret(), request) };
    let c = match x::credentials(secret) {
        Ok(c) => c,
        Err(e) => return refused(e, request),
    };
    let url = format!("{}{}", base.trim_end_matches('/'), x::CREATE_PATH);
    let (n, ts) = (nonce(), ctx.now.timestamp().to_string());
    let params = x::oauth_params(&c, &n, &ts);
    let sig = oauth1_signature(
        &x::signing_key(&c.consumer_secret, &c.access_token_secret),
        &x::signature_base("POST", &url, &params),
    );
    let r = Request::post(url, x::body(ctx.body)).header("authorization", x::authorization(&params, &sig));
    match http.send(r).await {
        Ok(reply) => report(&reply, x::classify(&reply, ctx.now.timestamp()), Some(&c.access_token_secret), request),
        Err(f) => Report { sent: f.to_sent(), http_status: None, excerpt: None, request },
    }
}

#[cfg(test)]
mod tests;
