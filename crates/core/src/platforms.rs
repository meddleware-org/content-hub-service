//! The generic platform definitions (`seed/platform-definitions.json`), which every instance is seeded
//! with. They carry no operator data: destinations (channels, handles, directory listings) and their
//! credentials are an instance's `platform_targets`.
//!
//! Each row was checked against the platform's documentation on the date in `verified_at`; facts that
//! could not be confirmed say "(verify)" in their notes and are not enforced as limits.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::ident::is_platform_code;
use crate::model::{CountingMethod, Markup, PlatformCategory, PostingMode};

/// The file, embedded at build time.
pub const GENERIC_DEFINITIONS_JSON: &str = include_str!("../../../seed/platform-definitions.json");

/// The placeholders an URL template may use. Each is replaced by the URL-encoded value.
pub const TEMPLATE_PLACEHOLDERS: &[&str] = &["text", "url", "title", "body", "subreddit", "target_base"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionsFile {
    pub schema_version: u32,
    pub verified_at: String,
    pub definitions: Vec<PlatformDefinitionSeed>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformDefinitionSeed {
    pub code: String,
    pub name: String,
    pub category: PlatformCategory,
    pub posting_mode: PostingMode,
    #[serde(default)]
    pub counting_method: Option<CountingMethod>,
    #[serde(default)]
    pub primary_limit: Option<u32>,
    #[serde(default)]
    pub secondary_limits: BTreeMap<String, serde_json::Value>,
    pub markup: Markup,
    #[serde(default)]
    pub supports_media: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub url_templates: BTreeMap<String, String>,
    #[serde(default)]
    pub cost_model: BTreeMap<String, serde_json::Value>,
    pub enabled: bool,
    pub notes: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DefinitionsError {
    #[error("the platform definitions file is not valid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("the platform definitions file is not valid:\n  {}", .0.join("\n  "))]
    Invalid(Vec<String>),
}

/// Parse and validate a definitions file.
pub fn parse(json: &str) -> Result<DefinitionsFile, DefinitionsError> {
    let file: DefinitionsFile = serde_json::from_str(json)?;
    let problems = validate(&file);
    if problems.is_empty() { Ok(file) } else { Err(DefinitionsError::Invalid(problems)) }
}

/// The embedded generic definitions.
pub fn generic() -> Result<DefinitionsFile, DefinitionsError> {
    parse(GENERIC_DEFINITIONS_JSON)
}

/// Check one URL template: it starts with `https://` or `{target_base}/` and uses only known placeholders.
pub fn check_template(t: &str) -> Result<(), String> {
    if !(t.starts_with("https://") || t.starts_with("{target_base}/")) {
        return Err("a template must start with https:// or {target_base}/".into());
    }
    for x in template_placeholders(t)? {
        if !TEMPLATE_PLACEHOLDERS.contains(&x) {
            return Err(format!("unknown placeholder {{{x}}}"));
        }
    }
    Ok(())
}

fn template_placeholders(t: &str) -> Result<Vec<&str>, String> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| format!("unclosed placeholder in {t:?}"))?;
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("stray '}}' in {t:?}"));
    }
    Ok(out)
}

/// Every problem with the file (empty when it is valid).
pub fn validate(file: &DefinitionsFile) -> Vec<String> {
    let mut p = Vec::new();
    if file.schema_version != 1 {
        p.push(format!("schema_version {} (expected 1)", file.schema_version));
    }
    let mut codes = BTreeSet::new();
    for d in &file.definitions {
        let c = &d.code;
        if !is_platform_code(c) {
            p.push(format!("{c}: not a platform code"));
        }
        if !codes.insert(c.as_str()) {
            p.push(format!("{c}: duplicate code"));
        }
        if d.name.trim().is_empty() || d.name.chars().count() > 100 {
            p.push(format!("{c}: name must be 1 to 100 characters"));
        }
        if d.notes.chars().count() > 4000 {
            p.push(format!("{c}: notes longer than 4000 characters"));
        }
        if d.primary_limit.is_some() && d.counting_method.is_none() {
            p.push(format!("{c}: a primary_limit needs a counting_method"));
        }
        if d.primary_limit == Some(0) {
            p.push(format!("{c}: primary_limit must be positive"));
        }
        for (k, v) in &d.secondary_limits {
            let ok = v.as_u64().is_some_and(|n| n > 0) || v.as_str().is_some_and(|s| !s.is_empty());
            if !ok {
                p.push(format!("{c}: secondary limit {k} must be a positive integer or a non-empty string"));
            }
        }
        if let Some(u) = &d.base_url
            && (!u.starts_with("https://") || u.contains(char::is_whitespace))
        {
            p.push(format!("{c}: base_url must be an https URL"));
        }
        for (name, t) in &d.url_templates {
            if !(t.starts_with("https://") || t.starts_with("{target_base}/")) {
                p.push(format!("{c}: template {name} must start with https:// or {{target_base}}/"));
            }
            match template_placeholders(t) {
                Ok(ph) => {
                    for x in ph {
                        if !TEMPLATE_PLACEHOLDERS.contains(&x) {
                            p.push(format!("{c}: template {name} uses unknown placeholder {{{x}}}"));
                        }
                    }
                }
                Err(e) => p.push(format!("{c}: {e}")),
            }
        }
        // Only platforms the worker sends to carry a cost per post; everything else is a budget note.
        for (k, v) in &d.cost_model {
            if k.ends_with("_usd_micros") && v.as_u64().is_none() {
                p.push(format!("{c}: cost {k} must be a non-negative integer of micro-US-dollars"));
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_code(f: &DefinitionsFile, code: &str) -> PlatformDefinitionSeed {
        f.definitions.iter().find(|d| d.code == code).cloned().unwrap_or_else(|| panic!("no {code}"))
    }

    #[test]
    fn the_generic_file_is_valid() {
        let f = generic().expect("valid");
        assert_eq!(f.definitions.len(), 38);
    }

    #[test]
    fn the_rows_the_prompts_require_have_their_documented_shape() {
        use CountingMethod::*;
        use PostingMode::{Api, Feed, Manual, Webhook};
        let f = generic().expect("valid");
        type Shape = (&'static str, PostingMode, Option<CountingMethod>, Option<u32>, Markup);
        let rows: &[Shape] = &[
            ("DISCORD_ANNOUNCEMENTS", Webhook, Some(Codepoints), Some(2000), Markup::DiscordMd),
            ("TELEGRAM_BROADCAST", Api, Some(Utf16), Some(4096), Markup::TelegramHtml),
            ("X", Api, Some(XWeighted), Some(280), Markup::Plain),
            ("BLUESKY", Api, Some(Graphemes), Some(300), Markup::Plain),
            ("MASTODON", Api, Some(Graphemes), Some(500), Markup::Plain),
            ("FARCASTER", Api, Some(Bytes), Some(320), Markup::Plain),
            ("REDDIT", Manual, Some(Codepoints), Some(40000), Markup::Markdown),
            ("BITCOINTALK", Manual, None, None, Markup::Bbcode),
            ("YOUTUBE", Manual, Some(Codepoints), Some(5000), Markup::Plain),
            ("OWNED_FEED", Feed, None, None, Markup::Markdown),
        ];
        for &(code, mode, counting, limit, markup) in rows {
            let d = by_code(&f, code);
            assert_eq!(
                (d.posting_mode, d.counting_method, d.primary_limit, d.markup),
                (mode, counting, limit, markup),
                "{code}"
            );
        }
        assert_eq!(by_code(&f, "BLUESKY").secondary_limits["bytes"], 3000);
        assert_eq!(by_code(&f, "REDDIT").secondary_limits["title"], 300);
        assert_eq!(by_code(&f, "YOUTUBE").secondary_limits["title"], 100);
        assert_eq!(by_code(&f, "MASTODON").secondary_limits["url_weight"], 23);
    }

    #[test]
    fn paid_and_unevaluated_publishers_ship_disabled() {
        let f = generic().expect("valid");
        assert!(!by_code(&f, "X").enabled);
        assert!(!by_code(&f, "FARCASTER").enabled);
        // The free-tier publishers ship enabled (sending still needs a target, its secret and DRY_RUN off).
        for code in ["OWNED_FEED", "DISCORD_ANNOUNCEMENTS", "TELEGRAM_BROADCAST", "BLUESKY", "MASTODON"] {
            assert!(by_code(&f, code).enabled, "{code}");
        }
    }

    #[test]
    fn validation_reports_every_problem() {
        let bad = r#"{"schema_version": 2, "verified_at": "x", "definitions": [
          {"code": "bad", "name": "", "category": "social", "posting_mode": "api", "primary_limit": 0,
           "markup": "plain", "base_url": "http://x", "url_templates": {"intent": "https://x/{evil}"},
           "secondary_limits": {"n": -1}, "cost_model": {"per_post_usd_micros": "1"}, "enabled": true, "notes": ""},
          {"code": "bad", "name": "n", "category": "social", "posting_mode": "api", "markup": "plain", "enabled": true, "notes": ""}
        ]}"#;
        let Err(DefinitionsError::Invalid(p)) = parse(bad) else { panic!("expected invalid") };
        let all = p.join("\n");
        for needle in [
            "schema_version 2",
            "bad: not a platform code",
            "bad: duplicate code",
            "name must be",
            "needs a counting_method",
            "must be positive",
            "secondary limit n",
            "base_url must be an https URL",
            "unknown placeholder {evil}",
            "cost per_post_usd_micros",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
    }

    #[test]
    fn unknown_fields_and_values_are_refused() {
        let extra = r#"{"schema_version": 1, "verified_at": "x", "definitions": [
          {"code": "A", "name": "n", "category": "social", "posting_mode": "api", "markup": "plain", "enabled": true, "notes": "", "secret": "x"}]}"#;
        assert!(matches!(parse(extra), Err(DefinitionsError::Parse(_))));
        let value = r#"{"schema_version": 1, "verified_at": "x", "definitions": [
          {"code": "A", "name": "n", "category": "tiktok", "posting_mode": "api", "markup": "plain", "enabled": true, "notes": ""}]}"#;
        assert!(matches!(parse(value), Err(DefinitionsError::Parse(_))));
    }

    #[test]
    fn placeholders_parse_strictly() {
        assert_eq!(template_placeholders("https://a/?t={text}&u={url}").expect("ok"), vec!["text", "url"]);
        assert!(template_placeholders("https://a/{text").is_err());
        assert!(template_placeholders("https://a/}").is_err());
    }
}
