//! Telegram Bot API `sendMessage` (core.telegram.org/bots/api: Making requests, sendMessage, Message, Chat,
//! LinkPreviewOptions, ResponseParameters). Requests go to `{base}/bot<token>/sendMessage`; the token is a path
//! segment, so it never appears in a stored request (`redacted_url`). Replies are `{"ok": true, "result": …}` or
//! `{"ok": false, "error_code": …, "description": …, "parameters": {"retry_after": …}}`.
//!
//! The target's `external_ref` is the chat: `@channelusername` or a numeric id. Target configuration:
//! `disable_link_preview` (bool), `disable_notification` (bool).

use serde_json::{Value, json};

use super::{Reply, by_status, reason};
use crate::delivery::Sent;

/// A bot token's shape: `<bot id>:<secret>` (docs example `123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11`).
pub fn is_token(s: &str) -> bool {
    let Some((id, secret)) = s.split_once(':') else { return false };
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit())
        && (20..=100).contains(&secret.len())
        && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The chat id as the API takes it: a number, or `@username` (5 to 32 letters, digits, underscores).
pub fn chat_id(external_ref: Option<&str>) -> Result<Value, String> {
    let r = external_ref.map(str::trim).unwrap_or("");
    if let Some(name) = r.strip_prefix('@') {
        let ok = (5..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        return if ok { Ok(json!(r)) } else { Err("external_ref is not a valid @channelusername".into()) };
    }
    r.parse::<i64>().map(|n| json!(n)).map_err(|_| "external_ref must be @channelusername or a numeric chat id".into())
}

pub fn method_url(base: &str, token: &str, method: &str) -> String {
    format!("{}/bot{token}/{method}", base.trim_end_matches('/'))
}

pub fn redacted_url(base: &str, method: &str) -> String {
    format!("{}/bot[redacted]/{method}", base.trim_end_matches('/'))
}

/// The `sendMessage` body: the payload is already Telegram HTML (rendered and escaped by the text engine).
pub fn body(chat: &Value, payload: &str, config: &Value) -> Value {
    let flag = |k: &str| config.get(k).and_then(Value::as_bool).unwrap_or(false);
    json!({
        "chat_id": chat,
        "text": payload,
        "parse_mode": "HTML",
        "link_preview_options": { "is_disabled": flag("disable_link_preview") },
        "disable_notification": flag("disable_notification"),
    })
}

/// Classify `sendMessage`'s reply.
pub fn classify(r: &Reply, chat: &Value) -> Sent {
    let v = r.json();
    let said = v.as_ref().and_then(|v| v.get("description")).and_then(Value::as_str).map(str::to_owned);
    let hint = v
        .as_ref()
        .and_then(|v| v.pointer("/parameters/retry_after"))
        .and_then(Value::as_u64)
        .map(|s| std::time::Duration::from_secs(s.min(86_400)));
    // 400 is "Bad Request: …" (too long, can't parse entities, chat not found): the content or the destination.
    // Telegram does not separate them by code, so a 400 counts as a refusal of the content.
    if let Some(s) = by_status(r, hint, reason(r, said.clone()), |s| s == 400) {
        return s;
    }
    let ok = v.as_ref().and_then(|v| v.get("ok")).and_then(Value::as_bool) == Some(true);
    if !ok {
        return Sent::Refused { reason: reason(r, said), validation: false };
    }
    let message_id = v.as_ref().and_then(|v| v.pointer("/result/message_id")).and_then(Value::as_i64);
    let username = v
        .as_ref()
        .and_then(|v| v.pointer("/result/chat/username"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| chat.as_str().and_then(|c| c.strip_prefix('@')).map(str::to_owned));
    let url = match (message_id, username) {
        (Some(m), Some(u)) => format!("https://t.me/{u}/{m}"),
        (Some(m), None) => format!("telegram:{chat}/{m}"),
        (None, _) => "telegram:message (no id in the reply)".to_owned(),
    };
    Sent::Posted { url_or_external_id: url }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply { status, headers: vec![], body: body.into() }
    }

    #[test]
    fn shapes() {
        assert!(is_token("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"));
        for bad in ["", "123456", "abc:ABC-DEF1234ghIkl-zyx57W2v1u123ew11", "1:short", "postgres://u:p@h/db"] {
            assert!(!is_token(bad), "{bad}");
        }
        assert_eq!(chat_id(Some("@meddleware_test")), Ok(json!("@meddleware_test")));
        assert_eq!(chat_id(Some("-1001234567890")), Ok(json!(-1001234567890_i64)));
        assert!(chat_id(Some("@ab")).is_err());
        assert!(chat_id(None).is_err());
        assert_eq!(
            redacted_url("https://api.telegram.org", "sendMessage"),
            "https://api.telegram.org/bot[redacted]/sendMessage"
        );
    }

    #[test]
    fn the_body_is_html_mode_with_the_target_flags() {
        let b = body(&json!("@chan_name"), "<b>Hi</b> &amp; bye", &json!({"disable_link_preview": true}));
        assert_eq!(
            b,
            json!({"chat_id": "@chan_name", "text": "<b>Hi</b> &amp; bye", "parse_mode": "HTML",
                   "link_preview_options": {"is_disabled": true}, "disable_notification": false})
        );
    }

    /// Fixtures follow the documented response object and ResponseParameters (core.telegram.org/bots/api).
    #[test]
    fn replies() {
        let chat = json!("@chan_name");
        let ok = reply(
            200,
            r#"{"ok":true,"result":{"message_id":42,"chat":{"id":-1001,"type":"channel","title":"T","username":"chan_name"},"date":1,"text":"Hi"}}"#,
        );
        assert_eq!(classify(&ok, &chat), Sent::Posted { url_or_external_id: "https://t.me/chan_name/42".into() });
        let private =
            reply(200, r#"{"ok":true,"result":{"message_id":7,"chat":{"id":-1002,"type":"channel","title":"T"}}}"#);
        assert_eq!(classify(&private, &json!(-1002)), Sent::Posted { url_or_external_id: "telegram:-1002/7".into() });
        let limited = reply(
            429,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 17","parameters":{"retry_after":17}}"#,
        );
        match classify(&limited, &chat) {
            Sent::Retry { after: Some(d), rate_limited: true, reason } => {
                assert_eq!(d.as_secs(), 17);
                assert!(reason.contains("Too Many Requests"));
            }
            other => panic!("{other:?}"),
        }
        let long = reply(400, r#"{"ok":false,"error_code":400,"description":"Bad Request: message is too long"}"#);
        assert_eq!(
            classify(&long, &chat),
            Sent::Refused { reason: "HTTP 400: Bad Request: message is too long".into(), validation: true }
        );
        let kicked = reply(
            403,
            r#"{"ok":false,"error_code":403,"description":"Forbidden: bot is not a member of the channel chat"}"#,
        );
        assert!(matches!(classify(&kicked, &chat), Sent::Refused { validation: false, .. }));
        assert!(matches!(classify(&reply(500, ""), &chat), Sent::Unknown { .. }));
    }
}
