//! X API v2 `POST /2/tweets` (docs.x.com: Manage Posts → Create Post) with OAuth 1.0a user context
//! (docs.x.com: Authentication → OAuth 1.0a → Creating a signature / Authorizing a request). The definition ships
//! **disabled**: creating a post is pay-per-use only ($0.015, $0.20 with a URL; docs.x.com pricing page,
//! re-checked 2026-10-29), so until an operator funds it, posts go out by the free web intent.
//!
//! OAuth 2.0 with PKCE is not implemented: its access tokens expire and each refresh issues a new refresh token,
//! which the worker would have to store, and credentials are never stored by the hub (they live only in the
//! deployment's Secrets). OAuth 1.0a user tokens do not expire.
//!
//! A link in a reply instead of the post saves nothing (any created post containing a URL costs the URL price);
//! the cost-free route for links is the intent link.
//!
//! The credential (`secret_ref`) is JSON: `{"consumer_key", "consumer_secret", "access_token",
//! "access_token_secret"}`. The HMAC-SHA1 itself is computed by the HTTP half (ring).

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Reply, by_status, reason};
use crate::delivery::Sent;

pub const CREATE_PATH: &str = "/2/tweets";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub access_token: String,
    pub access_token_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { [redacted] }")
    }
}

pub fn credentials(secret: &str) -> Result<Credentials, String> {
    serde_json::from_str(secret).map_err(|_| {
        "the X credential must be JSON with consumer_key, consumer_secret, access_token, access_token_secret".into()
    })
}

pub fn body(payload: &str) -> Value {
    json!({ "text": payload })
}

/// RFC 3986 percent-encoding: everything but `A-Z a-z 0-9 - . _ ~`, upper-case hex.
pub fn encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The signature base string: `METHOD&enc(url)&enc(sorted, encoded params joined by &)`. `params` are the OAuth
/// parameters plus any query or form parameters (a JSON body is not signed).
pub fn signature_base(method: &str, url: &str, params: &[(&str, &str)]) -> String {
    let mut enc: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    enc.sort();
    let joined = enc.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    format!("{}&{}&{}", method.to_ascii_uppercase(), encode(url), encode(&joined))
}

/// The HMAC key: `enc(consumer_secret)&enc(token_secret)`.
pub fn signing_key(consumer_secret: &str, token_secret: &str) -> String {
    format!("{}&{}", encode(consumer_secret), encode(token_secret))
}

/// The OAuth parameters of a request (without the signature).
pub fn oauth_params<'a>(c: &'a Credentials, nonce: &'a str, timestamp: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![
        ("oauth_consumer_key", c.consumer_key.as_str()),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", timestamp),
        ("oauth_token", c.access_token.as_str()),
        ("oauth_version", "1.0"),
    ]
}

/// The `Authorization` header from the OAuth parameters and the base64 signature.
pub fn authorization(params: &[(&str, &str)], signature: &str) -> String {
    let mut all: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    all.push(("oauth_signature".into(), encode(signature)));
    all.sort();
    format!("OAuth {}", all.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect::<Vec<_>>().join(", "))
}

/// Classify Create Post's reply.
pub fn classify(r: &Reply, now_epoch: i64) -> Sent {
    let v = r.json();
    let said = v.as_ref().and_then(|v| {
        v.get("detail")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| v.pointer("/errors/0/message").and_then(Value::as_str).map(str::to_owned))
    });
    let hint = r
        .header("x-rate-limit-reset")
        .and_then(|h| h.trim().parse::<i64>().ok())
        .map(|reset| std::time::Duration::from_secs(u64::try_from(reset - now_epoch).unwrap_or(0).min(86_400)));
    // 400 and 403 (duplicate content) refuse the post itself; 401 the credentials; 402 the account's credits.
    if let Some(s) = by_status(r, hint, reason(r, said), |s| matches!(s, 400 | 403)) {
        return s;
    }
    match v.as_ref().and_then(|v| v.pointer("/data/id")).and_then(Value::as_str) {
        Some(id) if id.bytes().all(|b| b.is_ascii_digit()) => {
            Sent::Posted { url_or_external_id: format!("https://x.com/i/web/status/{id}") }
        }
        _ => Sent::Posted { url_or_external_id: "x:post (no id in the reply)".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example of "Creating a signature" (developer.x.com, OAuth 1.0a): its documented base string. The
    /// HMAC-SHA1 of it with the example's signing key is `hCtSmYh+iHYCEqBWrE7C7hYmtUk=` (checked in the HTTP half).
    const DOC_BASE: &str = "POST&https%3A%2F%2Fapi.twitter.com%2F1.1%2Fstatuses%2Fupdate.json&include_entities%3Dtrue%26oauth_consumer_key%3Dxvz1evFS4wEEPTGEFPHBog%26oauth_nonce%3DkYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg%26oauth_signature_method%3DHMAC-SHA1%26oauth_timestamp%3D1318622958%26oauth_token%3D370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb%26oauth_version%3D1.0%26status%3DHello%2520Ladies%2520%252B%2520Gentlemen%252C%2520a%2520signed%2520OAuth%2520request%2521";

    #[test]
    fn the_documented_signature_base_string() {
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
        assert_eq!(signature_base("post", "https://api.twitter.com/1.1/statuses/update.json", &params), DOC_BASE);
        assert_eq!(
            signing_key("kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw", "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE"),
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw&LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE"
        );
        assert_eq!(encode("Ladies + Gentlemen!~"), "Ladies%20%2B%20Gentlemen%21~");
    }

    #[test]
    fn header_and_credentials() {
        let h = authorization(&[("oauth_nonce", "a b"), ("oauth_consumer_key", "k")], "s+/=");
        assert_eq!(h, r#"OAuth oauth_consumer_key="k", oauth_nonce="a%20b", oauth_signature="s%2B%2F%3D""#);
        let c =
            credentials(r#"{"consumer_key":"a","consumer_secret":"b","access_token":"c","access_token_secret":"d"}"#)
                .expect("json");
        assert_eq!(format!("{c:?}"), "Credentials { [redacted] }");
        assert!(credentials("not json").expect_err("bad").contains("JSON"));
    }

    /// Fixtures: the documented 201 body and problem-details errors (docs.x.com Create Post; Response codes).
    #[test]
    fn replies() {
        let ok =
            Reply { status: 201, headers: vec![], body: r#"{"data":{"id":"1445880548472328192","text":"Hi"}}"#.into() };
        assert_eq!(
            classify(&ok, 0),
            Sent::Posted { url_or_external_id: "https://x.com/i/web/status/1445880548472328192".into() }
        );
        let dup = Reply {
            status: 403,
            headers: vec![],
            body: r#"{"detail":"You are not allowed to create a Tweet with duplicate content.","type":"about:blank","title":"Forbidden","status":403}"#.into(),
        };
        assert!(matches!(classify(&dup, 0), Sent::Refused { validation: true, .. }));
        let limited =
            Reply { status: 429, headers: vec![("x-rate-limit-reset".into(), "1000".into())], body: "{}".into() };
        match classify(&limited, 940) {
            Sent::Retry { after: Some(d), .. } => assert_eq!(d.as_secs(), 60),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            classify(&Reply { status: 402, headers: vec![], body: "{}".into() }, 0),
            Sent::Refused { validation: false, .. }
        ));
        assert!(matches!(classify(&Reply { status: 503, headers: vec![], body: "".into() }, 0), Sent::Unknown { .. }));
    }
}
