//! Discord webhooks: Execute Webhook (`POST /webhooks/{id}/{token}?wait=true`), docs.discord.com → Resources →
//! Webhook, and the Message resource for limits and the error shape. `wait=true` makes Discord confirm the message
//! and return it (without it an unsaved message returns no error). `allowed_mentions` is always
//! `{"parse": []}`: a post can never ping `@everyone`, a role or a user.
//!
//! Target configuration: `mode` (`content`, the default, or `embed`: title and body in one embed), `guild_id`
//! (digits; makes the stored link a message URL), `username` (overrides the webhook's name),
//! `crosspost_secret_ref` (a bot token's variable: publish an announcement-channel message to followers).

use serde_json::{Value, json};

use super::{Reply, by_status, reason, seconds};
use crate::delivery::Sent;

/// How the post is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `content`: up to 2000 characters, the title is not sent.
    Content,
    /// One embed: the title, and the body as its description (up to 4096).
    Embed,
}

impl Mode {
    pub fn from_config(config: &Value) -> Mode {
        if config.get("mode").and_then(Value::as_str) == Some("embed") { Mode::Embed } else { Mode::Content }
    }
}

fn digits(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
}

/// The webhook URL to call: the credential must be `{base}/api[/v<n>]/webhooks/<id>/<token>` on the definition's
/// host (`https://discord.com`), and `?wait=true` is added. Anything else is refused without echoing it.
pub fn webhook_url(base: &str, secret: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let bad = || "the webhook URL is not a Discord webhook URL on the platform's host".to_owned();
    let rest = secret.trim().strip_prefix(base).ok_or_else(bad)?;
    let rest = rest.strip_prefix("/api").ok_or_else(bad)?;
    let rest = match rest.strip_prefix("/v") {
        Some(r) => {
            let (v, after) = r.split_once('/').ok_or_else(bad)?;
            if !digits(v, 2) {
                return Err(bad());
            }
            format!("/{after}")
        }
        None => rest.to_owned(),
    };
    let rest = rest.strip_prefix("/webhooks/").ok_or_else(bad)?;
    let (id, token) = rest.split_once('/').ok_or_else(bad)?;
    let token_ok = !token.is_empty()
        && token.len() <= 100
        && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !digits(id, 25) || !token_ok {
        return Err(bad());
    }
    Ok(format!("{base}/api/webhooks/{id}/{token}?wait=true"))
}

/// The webhook URL with its token replaced, for the attempt record.
pub fn redacted_url(url: &str) -> String {
    match url.find("/webhooks/") {
        Some(i) => {
            let tail = &url[i + "/webhooks/".len()..];
            let id = tail.split('/').next().unwrap_or("");
            format!("{}/webhooks/{id}/[redacted]?wait=true", &url[..i])
        }
        None => "[redacted]".to_owned(),
    }
}

/// The request body.
pub fn body(mode: Mode, title: &str, payload: &str, config: &Value) -> Value {
    let mut b = match mode {
        Mode::Content => json!({ "content": payload }),
        Mode::Embed => json!({ "embeds": [{ "title": title, "description": payload }] }),
    };
    b["allowed_mentions"] = json!({ "parse": [] });
    if let Some(u) = config.get("username").and_then(Value::as_str).filter(|u| !u.trim().is_empty()) {
        b["username"] = json!(u);
    }
    b
}

/// The link to a message: a URL when the guild is known, otherwise `discord:<channel>/<message>`.
pub fn message_link(config: &Value, channel_id: &str, message_id: &str) -> String {
    match config.get("guild_id").and_then(Value::as_str).filter(|g| digits(g, 25)) {
        Some(g) => format!("https://discord.com/channels/{g}/{channel_id}/{message_id}"),
        None => format!("discord:{channel_id}/{message_id}"),
    }
}

/// The created message's (channel id, message id), if the reply carries them.
pub fn created(r: &Reply) -> Option<(String, String)> {
    let v = r.json()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| digits(x, 25)).map(str::to_owned);
    Some((s("channel_id")?, s("id")?))
}

/// Classify Execute Webhook's reply.
pub fn classify(r: &Reply, config: &Value) -> Sent {
    let v = r.json();
    let said = v.as_ref().and_then(|v| {
        let m = v.get("message").and_then(Value::as_str)?;
        Some(match v.get("code").and_then(Value::as_i64) {
            Some(c) if c != 0 => format!("{m} (code {c})"),
            _ => m.to_owned(),
        })
    });
    // 429: `retry_after` (seconds, decimal) in the body; the Retry-After header as a fallback.
    let hint = v
        .as_ref()
        .and_then(|v| v.get("retry_after"))
        .and_then(Value::as_f64)
        .and_then(|f| seconds(&f.to_string()))
        .or_else(|| r.header("retry-after").and_then(seconds));
    // 400 refuses the message itself (empty, too long, bad embed); 401/403/404 the webhook.
    if let Some(s) = by_status(r, hint, reason(r, said), |s| matches!(s, 400 | 413)) {
        return s;
    }
    match created(r) {
        Some((channel, id)) => Sent::Posted { url_or_external_id: message_link(config, &channel, &id) },
        None => Sent::Posted { url_or_external_id: "discord:message (no id in the reply)".into() },
    }
}

/// The crosspost endpoint for a message in an announcement channel (needs a bot with Manage Messages there).
pub fn crosspost_url(base: &str, channel_id: &str, message_id: &str) -> String {
    format!("{}/api/v10/channels/{channel_id}/messages/{message_id}/crosspost", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply { status, headers: vec![], body: body.into() }
    }

    #[test]
    fn webhook_urls_are_checked_and_never_echoed() {
        let base = "https://discord.com";
        assert_eq!(
            webhook_url(base, "https://discord.com/api/webhooks/123456789012345678/AbC-_x9").as_deref(),
            Ok("https://discord.com/api/webhooks/123456789012345678/AbC-_x9?wait=true")
        );
        assert!(webhook_url(base, "https://discord.com/api/v10/webhooks/1/tok").is_ok());
        for bad in [
            "https://evil.example/api/webhooks/1/tok",
            "https://discord.com.evil.example/api/webhooks/1/tok",
            "https://discord.com/api/webhooks/abc/tok",
            "https://discord.com/api/webhooks/1/tok?x=1",
            "https://discord.com/api/webhooks/1/",
            "http://discord.com/api/webhooks/1/tok",
            "postgres://user:pw@db/x",
        ] {
            let e = webhook_url(base, bad).expect_err(bad);
            assert!(!e.contains("tok") && !e.contains("pw"), "{e}");
        }
        assert_eq!(
            redacted_url("https://discord.com/api/webhooks/42/secret-token?wait=true"),
            "https://discord.com/api/webhooks/42/[redacted]?wait=true"
        );
    }

    #[test]
    fn bodies_never_allow_mentions() {
        let c = json!({"username": "Hub"});
        let b = body(Mode::Content, "T", "hi @everyone", &c);
        assert_eq!(b, json!({"content": "hi @everyone", "allowed_mentions": {"parse": []}, "username": "Hub"}));
        let b = body(Mode::Embed, "Title", "Body", &json!({}));
        assert_eq!(b["embeds"][0], json!({"title": "Title", "description": "Body"}));
        assert_eq!(b["allowed_mentions"], json!({"parse": []}));
        assert_eq!(Mode::from_config(&json!({"mode": "embed"})), Mode::Embed);
        assert_eq!(Mode::from_config(&json!({})), Mode::Content);
    }

    /// Fixtures: the Message object returned with `wait=true`, the 429 body and the JSON error body as documented
    /// (docs.discord.com: Webhook → Execute Webhook; Topics → Rate Limits; Topics → Opcodes and Status Codes).
    #[test]
    fn replies() {
        let ok = reply(
            200,
            r#"{"id":"1300000000000000001","channel_id":"1200000000000000002","webhook_id":"9","content":"hi"}"#,
        );
        assert_eq!(
            classify(&ok, &json!({"guild_id": "1100000000000000003"})),
            Sent::Posted {
                url_or_external_id:
                    "https://discord.com/channels/1100000000000000003/1200000000000000002/1300000000000000001".into()
            }
        );
        assert_eq!(
            classify(&ok, &json!({})),
            Sent::Posted { url_or_external_id: "discord:1200000000000000002/1300000000000000001".into() }
        );
        let limited = reply(429, r#"{"message":"You are being rate limited.","retry_after":64.57,"global":false}"#);
        match classify(&limited, &json!({})) {
            Sent::Retry { after: Some(d), rate_limited: true, .. } => assert_eq!(d.as_millis(), 64_570),
            other => panic!("{other:?}"),
        }
        let empty = reply(400, r#"{"code":50006,"message":"Cannot send an empty message"}"#);
        assert_eq!(
            classify(&empty, &json!({})),
            Sent::Refused { reason: "HTTP 400: Cannot send an empty message (code 50006)".into(), validation: true }
        );
        let gone = reply(404, r#"{"code":10015,"message":"Unknown Webhook"}"#);
        assert!(matches!(classify(&gone, &json!({})), Sent::Refused { validation: false, .. }));
        assert!(matches!(classify(&reply(502, "bad gateway"), &json!({})), Sent::Unknown { .. }));
        assert!(matches!(classify(&reply(200, "not json"), &json!({})), Sent::Posted { .. }));
    }
}
