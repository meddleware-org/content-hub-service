//! Bluesky (AT Protocol): `com.atproto.server.createSession` with an app password, then
//! `com.atproto.repo.createRecord` of an `app.bsky.feed.post` (atproto.com lexicons: com.atproto.server.createSession,
//! com.atproto.repo.createRecord, app.bsky.feed.post, app.bsky.richtext.facet; atproto.com/specs/tid,
//! /specs/record-key, /specs/xrpc for the error shape `{"error": …, "message": …}`).
//!
//! - Links are clickable only through richtext facets whose indices are **UTF-8 byte** offsets into the text.
//! - The record key is a TID derived from the first attempt's time and the instance's idempotency key, so every
//!   attempt of one instance writes the same record, and the post's URL is known before the call (it is in the
//!   attempt record, which helps an operator resolve an `unknown`).
//! - The login step posts nothing: any failure there is `Retry` (or `Refused` for bad credentials).
//!
//! The target's `external_ref` is the account (handle or DID); configuration: `pds` (https base URL; default the
//! definition's `https://bsky.social`), `langs` (up to 3 BCP-47 tags).

use serde_json::{Value, json};

use super::{Reply, by_status, reason, seconds};
use crate::delivery::Sent;
use crate::text::markup::scan_url;

const B32: &[u8; 32] = b"234567abcdefghijklmnopqrstuvwxyz";

/// A TID: 64 bits, top bit 0, 53 bits of microseconds since the epoch, 10 bits of clock id; base32-sortable,
/// always 13 characters (atproto.com/specs/tid).
pub fn tid(micros: u64, clock_id: u16) -> String {
    let v = ((micros & ((1 << 53) - 1)) << 10) | u64::from(clock_id & 0x3ff);
    (0..13).rev().map(|i| B32[((v >> (5 * i)) & 31) as usize] as char).collect()
}

/// The record key of an instance's post: its first attempt's time and 10 bits of its idempotency key.
pub fn rkey(first_attempt_micros: u64, idempotency_key: &[u8; 16]) -> String {
    tid(first_attempt_micros, (u16::from(idempotency_key[0]) << 8) | u16::from(idempotency_key[1]))
}

/// Link facets for every bare URL in `text` (the plain renderer writes each link as its URL).
pub fn link_facets(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with("http")
            && let Some(end) = scan_url(text, i)
        {
            out.push(json!({
                "index": { "byteStart": i, "byteEnd": end },
                "features": [{ "$type": "app.bsky.richtext.facet#link", "uri": &text[i..end] }],
            }));
            i = end;
            continue;
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

pub fn session_url(pds: &str) -> String {
    format!("{}/xrpc/com.atproto.server.createSession", pds.trim_end_matches('/'))
}

pub fn create_record_url(pds: &str) -> String {
    format!("{}/xrpc/com.atproto.repo.createRecord", pds.trim_end_matches('/'))
}

pub fn session_body(identifier: &str, app_password: &str) -> Value {
    json!({ "identifier": identifier, "password": app_password })
}

/// The `createRecord` body. `created_at` is RFC 3339 (UTC).
pub fn record_body(repo: &str, rkey: &str, text: &str, created_at: &str, config: &Value) -> Value {
    let mut record = json!({ "$type": "app.bsky.feed.post", "text": text, "createdAt": created_at });
    let facets = link_facets(text);
    if !facets.is_empty() {
        record["facets"] = Value::Array(facets);
    }
    if let Some(langs) = config.get("langs").and_then(Value::as_array) {
        let tags: Vec<&Value> = langs.iter().filter(|l| l.as_str().is_some_and(|s| !s.is_empty())).take(3).collect();
        if !tags.is_empty() {
            record["langs"] = json!(tags);
        }
    }
    json!({ "repo": repo, "collection": "app.bsky.feed.post", "rkey": rkey, "record": record })
}

/// The web URL of a post.
pub fn post_url(repo: &str, rkey: &str) -> String {
    format!("https://bsky.app/profile/{repo}/post/{rkey}")
}

fn said(v: &Option<Value>) -> Option<String> {
    let v = v.as_ref()?;
    let e = v.get("error").and_then(Value::as_str);
    let m = v.get("message").and_then(Value::as_str);
    match (e, m) {
        (Some(e), Some(m)) => Some(format!("{e}: {m}")),
        (Some(x), None) | (None, Some(x)) => Some(x.to_owned()),
        (None, None) => None,
    }
}

/// A session: the account's DID and the access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub did: String,
    pub access_jwt: String,
}

/// Classify `createSession`. Nothing has been posted yet, so a server error or a rate limit is `Retry`.
pub fn classify_session(r: &Reply) -> Result<Session, Sent> {
    let v = r.json();
    let why = reason(r, said(&v));
    if r.is_success() {
        let s = |k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(Value::as_str).map(str::to_owned);
        return match (s("did"), s("accessJwt")) {
            (Some(did), Some(access_jwt)) if did.starts_with("did:") => Ok(Session { did, access_jwt }),
            _ => Err(Sent::Retry { reason: "login reply without a session".into(), after: None, rate_limited: false }),
        };
    }
    Err(match r.status {
        429 => Sent::Retry { reason: why, after: r.header("retry-after").and_then(seconds), rate_limited: true },
        500..=599 => Sent::Retry { reason: format!("login: {why}"), after: None, rate_limited: false },
        _ => Sent::Refused {
            reason: format!("login refused (check the identifier and app password): {why}"),
            validation: false,
        },
    })
}

/// Classify `createRecord`.
pub fn classify_record(r: &Reply, repo: &str, rkey: &str) -> Sent {
    let v = r.json();
    let err = v.as_ref().and_then(|v| v.get("error")).and_then(Value::as_str).unwrap_or("");
    // An expired or refused token means the record was not written.
    if matches!(r.status, 400 | 401)
        && matches!(err, "ExpiredToken" | "InvalidToken" | "AuthRequired" | "AuthenticationRequired")
    {
        return Sent::Retry { reason: reason(r, said(&v)), after: None, rate_limited: false };
    }
    if let Some(s) = by_status(r, r.header("retry-after").and_then(seconds), reason(r, said(&v)), |s| s == 400) {
        return s;
    }
    let rkey_back = v
        .as_ref()
        .and_then(|v| v.get("uri"))
        .and_then(Value::as_str)
        .and_then(|u| u.rsplit('/').next())
        .map(str::to_owned);
    Sent::Posted { url_or_external_id: post_url(repo, rkey_back.as_deref().unwrap_or(rkey)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply { status, headers: vec![], body: body.into() }
    }

    /// atproto.com/specs/tid: 13 characters of `234567abcdefghijklmnopqrstuvwxyz`, the first one of `234567abcdefghij`.
    #[test]
    fn tids() {
        assert_eq!(tid(0, 0), "2222222222222");
        let t = tid(1_761_730_000_000_000, 0x3ff);
        assert_eq!(t.len(), 13);
        assert!("234567abcdefghij".contains(t.chars().next().expect("first")));
        // Sortable: a later time sorts later.
        assert!(tid(1_761_730_000_000_001, 0) > tid(1_761_730_000_000_000, 1023));
        // Deterministic per instance.
        let k = [7u8; 16];
        assert_eq!(rkey(1_761_730_000_000_000, &k), rkey(1_761_730_000_000_000, &k));
        assert_ne!(rkey(1_761_730_000_000_000, &k), rkey(1_761_730_000_000_000, &[8u8; 16]));
    }

    #[test]
    fn facets_use_utf8_byte_offsets() {
        let text = "Café ☕ news: https://example.org/a. More (https://example.org/b)";
        let f = link_facets(text);
        assert_eq!(f.len(), 2);
        let s = f[0]["index"]["byteStart"].as_u64().expect("start") as usize;
        let e = f[0]["index"]["byteEnd"].as_u64().expect("end") as usize;
        assert_eq!(&text[s..e], "https://example.org/a");
        assert_eq!(s, "Café ☕ news: ".len());
        assert_eq!(f[1]["features"][0]["uri"], "https://example.org/b");
        assert_eq!(f[0]["features"][0]["$type"], "app.bsky.richtext.facet#link");
        assert!(link_facets("no links, just httpish words").is_empty());
    }

    #[test]
    fn bodies() {
        let b = record_body(
            "did:plc:abc",
            "3abc",
            "Hi https://x.org",
            "2026-10-29T12:00:00.000Z",
            &json!({"langs": ["en", "", "de", "fr", "es"]}),
        );
        assert_eq!(b["collection"], "app.bsky.feed.post");
        assert_eq!(b["rkey"], "3abc");
        assert_eq!(b["record"]["$type"], "app.bsky.feed.post");
        assert_eq!(b["record"]["langs"], json!(["en", "de", "fr"]));
        assert_eq!(b["record"]["facets"][0]["index"], json!({"byteStart": 3, "byteEnd": 16}));
        assert!(record_body("did:plc:abc", "k", "plain", "t", &json!({}))["record"].get("facets").is_none());
    }

    /// Fixtures follow the lexicons' output schemas and the XRPC error shape.
    #[test]
    fn replies() {
        let s = classify_session(&reply(
            200,
            r#"{"did":"did:plc:abc","handle":"a.bsky.social","accessJwt":"jwt","refreshJwt":"r"}"#,
        ));
        assert_eq!(s, Ok(Session { did: "did:plc:abc".into(), access_jwt: "jwt".into() }));
        let bad = classify_session(&reply(
            401,
            r#"{"error":"AuthenticationRequired","message":"Invalid identifier or password"}"#,
        ));
        assert!(matches!(bad, Err(Sent::Refused { validation: false, .. })));
        assert!(matches!(classify_session(&reply(503, "")), Err(Sent::Retry { .. })));
        let limited = Reply { status: 429, headers: vec![("retry-after".into(), "120".into())], body: "{}".into() };
        assert!(matches!(classify_session(&limited), Err(Sent::Retry { rate_limited: true, after: Some(_), .. })));

        let ok = reply(200, r#"{"uri":"at://did:plc:abc/app.bsky.feed.post/3lkey","cid":"bafy"}"#);
        assert_eq!(
            classify_record(&ok, "did:plc:abc", "3lkey"),
            Sent::Posted { url_or_external_id: "https://bsky.app/profile/did:plc:abc/post/3lkey".into() }
        );
        let invalid = reply(
            400,
            r#"{"error":"InvalidRequest","message":"Invalid app.bsky.feed.post record: Record/text must not be longer than 300 graphemes"}"#,
        );
        assert!(matches!(classify_record(&invalid, "d", "k"), Sent::Refused { validation: true, .. }));
        let expired = reply(400, r#"{"error":"ExpiredToken","message":"Token has expired"}"#);
        assert!(matches!(classify_record(&expired, "d", "k"), Sent::Retry { .. }));
        assert!(matches!(classify_record(&reply(500, "{}"), "d", "k"), Sent::Unknown { .. }));
    }
}
