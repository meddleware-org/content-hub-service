//! A platform's limits, read from its definition (`counting_method`, `primary_limit`, `secondary_limits`).
//!
//! Recognised `secondary_limits` keys: `url_weight` (each URL counts that many), `mention_local_part` (1: remote
//! mentions count as their local part), `bytes` (a UTF-8 byte cap on the payload, e.g. Bluesky 3000), `title`
//! (a cap on the title, counted with the same method). Other keys are informational (embed limits, `limit_source`).

use std::collections::BTreeMap;

use super::count::{CountRules, count};
use super::render::render;
use crate::model::{CountingMethod, Markup};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub rules: CountRules,
    pub primary: Option<u64>,
    pub bytes: Option<u64>,
    pub title: Option<u64>,
}

impl Limits {
    /// From a definition's columns. Without a counting method there is no limit to check.
    pub fn from_definition(
        method: Option<CountingMethod>,
        primary: Option<i64>,
        secondary: &BTreeMap<String, serde_json::Value>,
    ) -> Option<Limits> {
        let method = method?;
        let n = |k: &str| secondary.get(k).and_then(serde_json::Value::as_u64);
        Some(Limits {
            rules: CountRules {
                method,
                url_weight: n("url_weight").map(|w| w.min(u64::from(u32::MAX)) as u32),
                mention_local_part: n("mention_local_part") == Some(1),
            },
            primary: primary.and_then(|p| u64::try_from(p).ok()),
            bytes: n("bytes"),
            title: n("title"),
        })
    }
}

/// The limits that apply to one target: its definition's, adjusted by the target's configuration. A Discord
/// target in `embed` mode is bounded by `embed_description` instead of the 2000-character content; a Mastodon
/// target uses its instance's `instance_limits` (cached by the worker) when known.
pub fn for_target(
    base: Option<Limits>,
    secondary: &BTreeMap<String, serde_json::Value>,
    target_config: &serde_json::Value,
) -> Option<Limits> {
    let mut l = base?;
    let n = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_u64).filter(|n| *n > 0);
    if target_config.get("mode").and_then(serde_json::Value::as_str) == Some("embed")
        && let Some(d) = n(secondary.get("embed_description"))
    {
        l.primary = Some(d);
    }
    if let Some(il) = target_config.get("instance_limits") {
        if let Some(m) = n(il.get("max_characters")) {
            l.primary = Some(m);
        }
        if let Some(u) = n(il.get("characters_reserved_per_url")) {
            l.rules.url_weight = Some(u.min(u64::from(u32::MAX)) as u32);
        }
    }
    Some(l)
}

/// The result of checking one rendered post against a platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub payload: String,
    pub counted: u64,
    pub limit: Option<u64>,
    pub bytes: u64,
    pub byte_limit: Option<u64>,
    pub title_counted: Option<u64>,
    pub title_limit: Option<u64>,
}

impl Check {
    pub fn fits(&self) -> bool {
        self.limit.is_none_or(|l| self.counted <= l)
            && self.byte_limit.is_none_or(|l| self.bytes <= l)
            && match (self.title_counted, self.title_limit) {
                (Some(c), Some(l)) => c <= l,
                _ => true,
            }
    }
}

/// Render `body` (canonical format) for `markup` and count it under `limits`; count `title` against the title cap.
pub fn check(markup: Markup, limits: &Limits, title: Option<&str>, body: &str) -> Check {
    let r = render(markup, &super::markup::parse(body));
    let counted_text = if markup == Markup::TelegramHtml { &r.visible } else { &r.payload };
    Check {
        counted: count(&limits.rules, counted_text),
        limit: limits.primary,
        bytes: r.payload.len() as u64,
        byte_limit: limits.bytes,
        title_counted: title.map(|t| count(&limits.rules, t)),
        title_limit: limits.title,
        payload: r.payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_map_to_limits() {
        let sec: BTreeMap<String, serde_json::Value> = serde_json::from_str(
            r#"{"url_weight": 23, "mention_local_part": 1, "bytes": 3000, "title": 300, "limit_source": "instance"}"#,
        )
        .expect("json");
        let l = Limits::from_definition(Some(CountingMethod::Graphemes), Some(500), &sec).expect("limits");
        assert_eq!(l.rules.url_weight, Some(23));
        assert!(l.rules.mention_local_part);
        assert_eq!((l.primary, l.bytes, l.title), (Some(500), Some(3000), Some(300)));
        assert!(Limits::from_definition(None, None, &sec).is_none());
    }

    #[test]
    fn targets_adjust_their_platform_limits() {
        let sec: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(r#"{"embed_description": 4096, "url_weight": 23}"#).expect("json");
        let base = Limits::from_definition(Some(CountingMethod::Codepoints), Some(2000), &sec);
        let embed = for_target(base, &sec, &serde_json::json!({"mode": "embed"})).expect("limits");
        assert_eq!(embed.primary, Some(4096));
        assert_eq!(for_target(base, &sec, &serde_json::json!({})).expect("l").primary, Some(2000));
        let inst = serde_json::json!({"instance_limits": {"max_characters": 11000, "characters_reserved_per_url": 30}});
        let m = for_target(base, &sec, &inst).expect("limits");
        assert_eq!((m.primary, m.rules.url_weight), (Some(11000), Some(30)));
        assert!(for_target(None, &sec, &inst).is_none());
    }

    #[test]
    fn telegram_counts_the_visible_text() {
        let l = Limits { rules: CountRules::plain(CountingMethod::Utf16), primary: Some(4), bytes: None, title: None };
        // Payload "<b>abcd</b>" is 11 units; Telegram counts "abcd".
        assert!(check(Markup::TelegramHtml, &l, None, "**abcd**").fits());
        assert!(!check(Markup::Markdown, &l, None, "**abcd**").fits());
    }
}
