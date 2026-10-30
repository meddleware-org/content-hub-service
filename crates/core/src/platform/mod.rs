//! The pure half of each publisher (ARCHITECTURE.md §10): request bodies, URL and credential shapes, and the
//! classification of a platform's reply into a [`Sent`](crate::delivery::Sent). The HTTP half lives in
//! `content-hub-publish`. Every fixture in the tests names the documentation it follows; facts only a live call
//! can confirm are listed in `docs/PUBLISHING.md` (decision D85 of the operator's log).
//!
//! Classification rules shared by every platform:
//!
//! - a 2xx is `Posted` (an unparsable body still means it went out; the id is then a placeholder);
//! - a 429 is `Retry` with the platform's hint;
//! - a 4xx that refuses the content is `Refused { validation: true }`, one that refuses the credentials or the
//!   destination is `Refused { validation: false }`;
//! - a 5xx is `Unknown` (the platform may have created the post before failing), except where the platform
//!   documents the request as idempotent with the key we sent (Mastodon) or nothing could have been posted
//!   (a login step);
//! - transport failures are classified by the HTTP half: a connection never made is `Retry`, anything after the
//!   request may have been written is `Unknown`.

use std::time::Duration;

use serde_json::Value;

use crate::delivery::{Sent, clip};
use crate::model::PostingMode;

pub mod bluesky;
pub mod discord;
pub mod mastodon;
pub mod telegram;
pub mod x;

/// The publishers that exist. Platform definitions are generic data; these codes are their stable identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The hub's own feeds: publishing makes the post appear in them (no external call).
    Feed,
    Discord,
    Telegram,
    Bluesky,
    Mastodon,
    X,
}

impl Kind {
    /// The publisher for a definition, if one exists. Manual and `none` platforms have none.
    pub fn for_definition(code: &str, mode: PostingMode) -> Option<Kind> {
        match (mode, code) {
            (PostingMode::Feed, _) => Some(Kind::Feed),
            (PostingMode::Webhook, c) if c.starts_with("DISCORD_") => Some(Kind::Discord),
            (PostingMode::Api, "TELEGRAM_BROADCAST") => Some(Kind::Telegram),
            (PostingMode::Api, "BLUESKY") => Some(Kind::Bluesky),
            (PostingMode::Api, "MASTODON") => Some(Kind::Mastodon),
            (PostingMode::Api, "X") => Some(Kind::X),
            _ => None,
        }
    }

    /// Whether sending needs a credential (`secret_ref`).
    pub const fn needs_secret(self) -> bool {
        !matches!(self, Kind::Feed)
    }
}

/// A platform's HTTP reply, reduced to what classification needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    /// At most the first 64 KiB.
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// `text` with every occurrence of the secret, and of each `:`/`/`/`.`-separated part of it longer than 8
/// characters, replaced by `[redacted]`.
pub fn scrub(text: &str, secret: Option<&str>) -> String {
    let mut out = text.to_owned();
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        let mut parts: Vec<&str> = vec![s];
        parts.extend(s.split([':', '/', '.']).filter(|p| p.len() > 8));
        for p in parts {
            out = out.replace(p, "[redacted]");
        }
    }
    out
}

/// The text kept from a reply for the attempt record: scrubbed of the secret first, then at most 500 characters.
pub fn excerpt(body: &str, secret: Option<&str>) -> String {
    clip(&scrub(body, secret), 500)
}

/// The outcome with the secret scrubbed from its reason (reasons quote what the platform said, and become the
/// instance's `last_error`).
pub fn scrub_sent(sent: Sent, secret: Option<&str>) -> Sent {
    match sent {
        Sent::Refused { reason, validation } => Sent::Refused { reason: scrub(&reason, secret), validation },
        Sent::Retry { reason, after, rate_limited } => {
            Sent::Retry { reason: scrub(&reason, secret), after, rate_limited }
        }
        Sent::Unknown { reason } => Sent::Unknown { reason: scrub(&reason, secret) },
        posted @ Sent::Posted { .. } => posted,
    }
}

/// A `Retry-After`-style value in seconds (integer or decimal), at most a day.
pub fn seconds(v: &str) -> Option<Duration> {
    let f: f64 = v.trim().parse().ok()?;
    (f.is_finite() && f >= 0.0).then(|| Duration::from_secs_f64(f.min(86_400.0)))
}

/// The common part of a classification: 429 → retry with `hint`; 5xx → unknown; other 4xx → refused (validation
/// when `validation(status)`); `None` for 2xx so the caller reads its body.
pub(crate) fn by_status(
    r: &Reply,
    hint: Option<Duration>,
    reason: String,
    validation: fn(u16) -> bool,
) -> Option<Sent> {
    match r.status {
        200..=299 => None,
        429 => Some(Sent::Retry { reason, after: hint, rate_limited: true }),
        500..=599 => Some(Sent::Unknown { reason }),
        s => Some(Sent::Refused { reason, validation: validation(s) }),
    }
}

/// `HTTP <status>: <what the platform said>`.
pub(crate) fn reason(r: &Reply, said: Option<String>) -> String {
    match said.filter(|s| !s.trim().is_empty()) {
        Some(s) => format!("HTTP {}: {}", r.status, clip(s.trim(), 300)),
        None => format!("HTTP {}", r.status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        use PostingMode::{Api, Feed, Manual, Webhook};
        assert_eq!(Kind::for_definition("OWNED_FEED", Feed), Some(Kind::Feed));
        assert_eq!(Kind::for_definition("DISCORD_PER_PACKAGE", Webhook), Some(Kind::Discord));
        assert_eq!(Kind::for_definition("TELEGRAM_BROADCAST", Api), Some(Kind::Telegram));
        assert_eq!(Kind::for_definition("X", Api), Some(Kind::X));
        assert_eq!(Kind::for_definition("FARCASTER", Api), None);
        assert_eq!(Kind::for_definition("REDDIT", Manual), None);
        assert!(!Kind::Feed.needs_secret() && Kind::Discord.needs_secret());
    }

    #[test]
    fn excerpts_never_carry_the_secret() {
        let token = "123456789:AAH-secretpart_abcdefghijklmnopqrstu";
        let body = format!("bad token {token} and half AAH-secretpart_abcdefghijklmnopqrstu");
        let e = excerpt(&body, Some(token));
        assert!(!e.contains("AAH-secretpart"), "{e}");
        assert!(e.contains("[redacted]"));
        assert_eq!(excerpt(&"x".repeat(900), None).len(), 500);
        // Scrubbed before clipping: a secret cut at the boundary does not survive in part.
        let near_end = format!("{}{token}", "x".repeat(490));
        assert!(!excerpt(&near_end, Some(token)).contains("123456789:AAH"));
        let s = scrub_sent(Sent::Refused { reason: format!("Unauthorized {token}"), validation: false }, Some(token));
        assert_eq!(s, Sent::Refused { reason: "Unauthorized [redacted]".into(), validation: false });
    }

    #[test]
    fn seconds_parse() {
        assert_eq!(seconds("2"), Some(Duration::from_secs(2)));
        assert_eq!(seconds(" 1.5 "), Some(Duration::from_millis(1500)));
        assert_eq!(seconds("999999"), Some(Duration::from_secs(86_400)));
        assert_eq!(seconds("-1"), None);
        assert_eq!(seconds("soon"), None);
    }
}
