//! Render and validate one instance's payload (ARCHITECTURE.md §10, step 1), and reserve its spend. The enqueue step
//! and an operator's re-queue both go through here, so nothing reaches `queued` without a payload that fits its
//! target's limits and a reservation under every cap that applies. The worker sends exactly this payload.
//!
//! The payload's hash is the SHA-256 of the title, a NUL byte and the rendered body (`delivery_attempts` records it
//! for every attempt).

use std::collections::BTreeMap;

use content_hub_core::model::{CountingMethod, Markup, PostingMode};
use content_hub_core::text::limits::{Limits, check, for_target};
use content_hub_core::text::render::render_source;
use content_hub_store::repo::instances::Payload;
use content_hub_store::repo::platforms::PlatformDefinition;
use content_hub_store::repo::posts::Post;
use content_hub_store::repo::spend;
use content_hub_store::repo::targets::PlatformTarget;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;

use super::{Ceilings, normalise_body};

/// A payload ready to queue.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The rendered and validated payload.
    pub payload: Payload,
    /// The estimated cost of sending it (micro-US-dollars; 0 on a free platform).
    pub cost: i64,
    /// How the platform delivers a post.
    pub mode: PostingMode,
}

/// The per-post cost of a platform in micro-US-dollars (0 for free platforms).
pub fn cost_of(cost_model: &serde_json::Value, payload_has_url: bool) -> i64 {
    // A cost figure that is missing or negative counts as zero.
    let get = |k: &str| cost_model.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0).max(0);
    if payload_has_url {
        // A post with a link can cost more, never less, than one without.
        get("per_post_with_url_usd_micros").max(get("per_post_usd_micros"))
    } else {
        get("per_post_usd_micros")
    }
}

/// The SHA-256 (hex) of the title, a zero byte and the body.
pub fn payload_sha256(title: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(title.as_bytes());
    // A zero byte between the title and the body, so moving text from one to the other changes the hash.
    h.update([0u8]);
    h.update(body.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Render the post (or the instance's overrides) for the target and check it. `Err` says why it cannot be queued.
pub fn prepare(
    post: &Post,
    override_title: Option<&str>,
    override_body: Option<&str>,
    target: &PlatformTarget,
    def: &PlatformDefinition,
) -> Result<Prepared, String> {
    // A disabled target or platform is never queued.
    if !target.enabled || !def.enabled {
        return Err(format!("{} ({}) is disabled", target.name, def.code));
    }
    // The mode decides who sends the post.
    let mode: PostingMode = def.posting_mode.parse().map_err(|_| "unknown posting mode".to_owned())?;
    // A tracking-only platform is not a destination.
    if mode == PostingMode::None {
        return Err(format!("{} is not a posting destination", def.code));
    }
    // The instance's override wins over the canonical text.
    let title = override_title.unwrap_or(&post.title);
    let body = override_body.map(normalise_body).unwrap_or_else(|| post.body_markdown.clone());
    // An unknown markup falls back to plain text, which is always safe.
    let markup: Markup = def.markup.parse().unwrap_or(Markup::Plain);
    let secondary: BTreeMap<String, serde_json::Value> =
        serde_json::from_value(def.secondary_limits.clone()).unwrap_or_default();
    // How the platform counts characters, if it limits length.
    let method: Option<CountingMethod> = def.counting_method.as_deref().and_then(|m| m.parse().ok());
    let base = Limits::from_definition(method, def.primary_limit.map(i64::from), &secondary);
    // The target's own configuration can adjust the limits (for example a Mastodon instance's).
    let payload = match for_target(base, &secondary, &target.config) {
        Some(limits) => {
            let c = check(markup, &limits, Some(title), &body);
            // A post that does not fit is refused here, with the numbers, instead of failing at the platform.
            if !c.fits() {
                return Err(format!(
                    "{} does not fit: {} of {} ({}), {} bytes of {}, title {} of {}",
                    def.code,
                    c.counted,
                    c.limit.map_or("none".into(), |l| l.to_string()),
                    def.counting_method.as_deref().unwrap_or("-"),
                    c.bytes,
                    c.byte_limit.map_or("none".into(), |l| l.to_string()),
                    c.title_counted.map_or("-".into(), |l| l.to_string()),
                    c.title_limit.map_or("none".into(), |l| l.to_string()),
                ));
            }
            c.payload
        }
        // A platform with no limits is rendered without a check.
        None => render_source(markup, &body).payload,
    };
    // A Discord embed carries its text in the title, so an empty body is allowed there.
    let embed = target.config.get("mode").and_then(serde_json::Value::as_str) == Some("embed");
    if payload.trim().is_empty() && !embed && matches!(mode, PostingMode::Api | PostingMode::Webhook) {
        return Err(format!("{}: the post body is empty", def.code));
    }
    // Some platforms charge more for a post with a link.
    let has_url = payload.contains("https://") || payload.contains("http://");
    Ok(Prepared {
        payload: Payload {
            post_revision: post.revision,
            title: title.to_owned(),
            // The hash of exactly what will be sent, stored with the payload.
            sha256: payload_sha256(title, &payload),
            body: payload,
        },
        cost: cost_of(&def.cost_model, has_url),
        mode,
    })
}

/// Check `cost` against every cap that applies to the target, holding the ledger's lock until the transaction ends
/// (so the reservation that follows cannot race another). `Some(reason)` names the cap it would pass. Free sends
/// pass without a check.
pub async fn over_cap(
    conn: &mut PgConnection,
    target: &PlatformTarget,
    cost: i64,
    ceilings: &Ceilings,
) -> Result<Option<String>, content_hub_store::StoreError> {
    // A free send needs no cap check.
    if cost <= 0 {
        return Ok(None);
    }
    // Held until the transaction ends, so the reservation that follows cannot race another one.
    spend::lock(conn).await?;
    let caps = spend::applicable(
        conn,
        target.platform_definition_id,
        target.id,
        ceilings.total_monthly_usd_micros,
        ceilings.per_platform_monthly_usd_micros,
    )
    .await?;
    // The first cap the cost would pass, named in the answer.
    Ok(caps.iter().find(|c| !c.fits(cost)).map(|c| {
        format!(
            "the {} monthly spend cap would be passed ({} + {} > {} micro-USD): post it by hand from the intent link, or raise the cap",
            c.scope, c.used_usd_micros, cost, c.limit_usd_micros
        )
    }))
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;

    /// Costs and hashes.
    #[test]
    fn costs_and_hashes() {
        let m = serde_json::json!({"per_post_usd_micros": 15000, "per_post_with_url_usd_micros": 200000});
        assert_eq!((cost_of(&m, false), cost_of(&m, true)), (15000, 200000));
        assert_eq!(cost_of(&serde_json::json!({}), true), 0);
        assert_eq!(cost_of(&serde_json::json!({"per_post_usd_micros": -5}), false), 0);
        assert_eq!(payload_sha256("t", "b").len(), 64);
        assert_ne!(payload_sha256("t", "b"), payload_sha256("tb", ""));
    }
}
