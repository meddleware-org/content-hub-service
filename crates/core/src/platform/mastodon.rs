//! Mastodon `POST /api/v1/statuses` (docs.joinmastodon.org/methods/statuses) with an `Idempotency-Key` header:
//! "Prevent duplicate submissions of the same status. Idempotency keys are stored for up to 1 hour". Within that
//! window a request that may or may not have arrived can be repeated with the same key without a second post, so a
//! timeout or a 5xx is `Retry` while the window is open (we use 45 minutes, leaving a margin) and `Unknown` after.
//! Rate limits: 429 with `X-RateLimit-Reset`, an ISO 8601 time (docs.joinmastodon.org/api/rate-limits; mastodon
//! `app/controllers/concerns/api/rate_limit_headers.rb`: `iso8601(6)`). Instance limits: `GET /api/v2/instance`,
//! `configuration.statuses.max_characters` and `characters_reserved_per_url`.
//!
//! The target's `external_ref` is the instance's https base URL; configuration: `visibility` (`public`, the
//! default, `unlisted` or `private`), `language` (ISO 639-1). The worker caches the instance's limits in the
//! target's configuration as `instance_limits` (refreshed daily), which the enqueue step validates against.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{Reply, reason};
use crate::delivery::Sent;

/// How long after the first attempt a repeat with the same key is still safe.
pub const IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(45 * 60);

/// The instance base: a URL with a host and nothing after it. (The admin API requires https and a public host; the
/// worker's client refuses anything else again.)
pub fn base(external_ref: Option<&str>) -> Result<String, String> {
    let r = external_ref.map(str::trim).unwrap_or("").trim_end_matches('/');
    let host = r
        .strip_prefix("https://")
        .or_else(|| r.strip_prefix("http://"))
        .ok_or("external_ref must be the instance's https URL")?;
    if host.is_empty() || host.contains(['/', '?', '#', '@', ' ']) {
        return Err("external_ref must be the instance's base URL (https://host)".into());
    }
    Ok(r.to_owned())
}

pub fn statuses_url(base: &str) -> String {
    format!("{base}/api/v1/statuses")
}

pub fn instance_url(base: &str) -> String {
    format!("{base}/api/v2/instance")
}

/// The status body. Unknown visibilities fall back to `public`; `direct` is never used.
pub fn body(payload: &str, config: &Value) -> Value {
    let visibility = config
        .get("visibility")
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "unlisted" | "private"))
        .unwrap_or("public");
    let mut b = json!({ "status": payload, "visibility": visibility });
    if let Some(l) = config.get("language").and_then(Value::as_str)
        && l.len() == 2
        && l.bytes().all(|c| c.is_ascii_lowercase())
    {
        b["language"] = json!(l);
    }
    b
}

/// An instance's posting limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceLimits {
    pub max_characters: u64,
    pub characters_reserved_per_url: u64,
}

pub fn parse_instance_limits(body: &str) -> Option<InstanceLimits> {
    let v: Value = serde_json::from_str(body).ok()?;
    let s = v.pointer("/configuration/statuses")?;
    let n = |k: &str| s.get(k).and_then(Value::as_u64).filter(|n| (1..=1_000_000).contains(n));
    Some(InstanceLimits {
        max_characters: n("max_characters")?,
        characters_reserved_per_url: n("characters_reserved_per_url")?,
    })
}

/// The delay until an `X-RateLimit-Reset` time.
pub fn reset_delay(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let at = DateTime::parse_from_rfc3339(value.trim()).ok()?.with_timezone(&Utc);
    Some((at - now).to_std().unwrap_or_default())
}

/// Whether a repeat with the same key is still safe.
pub fn window_open(first_attempt_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    (now - first_attempt_at).to_std().is_ok_and(|d| d < IDEMPOTENCY_WINDOW) || now <= first_attempt_at
}

/// Classify the reply. `window` is [`window_open`] for this attempt.
pub fn classify(r: &Reply, window: bool, now: DateTime<Utc>) -> Sent {
    let v = r.json();
    let said = v.as_ref().and_then(|v| v.get("error")).and_then(Value::as_str).map(str::to_owned);
    let why = reason(r, said);
    match r.status {
        200..=299 => {
            let s = |k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(Value::as_str).map(str::to_owned);
            let id = s("url").or_else(|| s("uri")).or_else(|| s("id").map(|i| format!("mastodon:{i}")));
            Sent::Posted { url_or_external_id: id.unwrap_or_else(|| "mastodon:status (no id in the reply)".into()) }
        }
        429 => Sent::Retry {
            reason: why,
            after: r.header("x-ratelimit-reset").and_then(|h| reset_delay(h, now)),
            rate_limited: true,
        },
        500..=599 if window => Sent::Retry {
            reason: format!("{why} (repeated with the same Idempotency-Key)"),
            after: None,
            rate_limited: false,
        },
        500..=599 => Sent::Unknown { reason: why },
        422 => Sent::Refused { reason: why, validation: true },
        _ => Sent::Refused { reason: why, validation: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn reply(status: u16, body: &str) -> Reply {
        Reply { status, headers: vec![], body: body.into() }
    }

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 29, h, m, 0).single().expect("time")
    }

    #[test]
    fn bases_and_bodies() {
        assert_eq!(base(Some("https://mastodon.social/")).as_deref(), Ok("https://mastodon.social"));
        for bad in [None, Some("m.example"), Some("https://m.example/@me"), Some("https://")] {
            assert!(base(bad).is_err(), "{bad:?}");
        }
        assert_eq!(body("Hi", &json!({})), json!({"status": "Hi", "visibility": "public"}));
        assert_eq!(
            body("Hi", &json!({"visibility": "unlisted", "language": "en"})),
            json!({"status": "Hi", "visibility": "unlisted", "language": "en"})
        );
        assert_eq!(body("Hi", &json!({"visibility": "direct"}))["visibility"], "public");
    }

    /// Fixture: the documented /api/v2/instance shape (docs.joinmastodon.org/methods/instance).
    #[test]
    fn instance_limits() {
        let b = r#"{"domain":"mastodon.social","configuration":{"statuses":{"max_characters":500,"max_media_attachments":4,"characters_reserved_per_url":23}}}"#;
        assert_eq!(
            parse_instance_limits(b),
            Some(InstanceLimits { max_characters: 500, characters_reserved_per_url: 23 })
        );
        assert_eq!(parse_instance_limits(r#"{"configuration":{}}"#), None);
    }

    #[test]
    fn the_idempotency_window() {
        assert!(window_open(t(12, 0), t(12, 44)));
        assert!(!window_open(t(12, 0), t(12, 46)));
        assert!(window_open(t(12, 0), t(11, 59)));
    }

    #[test]
    fn replies() {
        let ok = reply(
            200,
            r#"{"id":"113","uri":"https://m.example/users/a/statuses/113","url":"https://m.example/@a/113"}"#,
        );
        assert_eq!(
            classify(&ok, true, t(12, 0)),
            Sent::Posted { url_or_external_id: "https://m.example/@a/113".into() }
        );
        let long = reply(422, r#"{"error":"Validation failed: Text character limit of 500 exceeded"}"#);
        assert_eq!(
            classify(&long, true, t(12, 0)),
            Sent::Refused {
                reason: "HTTP 422: Validation failed: Text character limit of 500 exceeded".into(),
                validation: true
            }
        );
        let token = reply(401, r#"{"error":"The access token is invalid"}"#);
        assert!(matches!(classify(&token, true, t(12, 0)), Sent::Refused { validation: false, .. }));
        let limited = Reply {
            status: 429,
            headers: vec![("x-ratelimit-reset".into(), "2026-10-29T12:05:00.000000Z".into())],
            body: r#"{"error":"Too many requests"}"#.into(),
        };
        match classify(&limited, true, t(12, 0)) {
            Sent::Retry { after: Some(d), rate_limited: true, .. } => assert_eq!(d.as_secs(), 300),
            other => panic!("{other:?}"),
        }
        assert!(matches!(classify(&reply(503, ""), true, t(12, 0)), Sent::Retry { rate_limited: false, .. }));
        assert!(matches!(classify(&reply(503, ""), false, t(12, 0)), Sent::Unknown { .. }));
    }
}
