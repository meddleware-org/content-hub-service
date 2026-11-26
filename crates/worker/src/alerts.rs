//! The optional alert summary (ARCHITECTURE.md §10 "Alerting"): at most once per interval, a short message to an
//! internal Discord channel (its webhook URL is the `ALERT_DISCORD_WEBHOOK_URL` secret) with
//!
//! - how many instances became `failed` or `unknown` since the last summary, and
//! - every spend cap that crossed 50, 80 or 100 % this month (each level once per month).
//!
//! Counts only: never a post's text, a target's name or a credential. The state (when the last summary went out,
//! which levels were reported) is the `alerts.state` setting, locked while it is decided, so replicas and restarts
//! do not repeat a summary. Mentions are disabled.

use std::time::Duration;

use chrono::{DateTime, Utc};
use content_hub_core::platform::discord;
use content_hub_publish::{Http, Request};
use content_hub_store::repo::{metrics as m, settings, spend};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{WorkerConfig, WorkerError};

/// Where and how often the worker sends an alert.
#[derive(Clone)]
pub struct AlertConfig {
    /// The webhook URL (a secret).
    pub webhook_url: String,
    /// The platform's host (`https://discord.com`); the URL must be a webhook on it.
    pub base_url: String,
    /// How often the alert conditions are checked.
    pub every: Duration,
}

impl std::fmt::Debug for AlertConfig {
    /// Print the interval only: the webhook URL is the credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertConfig").field("webhook_url", &"[redacted]").field("every", &self.every).finish()
    }
}

/// The highest of 50 / 80 / 100 reached by `used` against `cap` (a cap of 0 with any use is 100).
pub fn level(used: i64, cap: i64) -> u8 {
    // No use, no level.
    if used <= 0 {
        return 0;
    }
    // A cap of zero with any use is already at 100 %.
    if cap <= 0 {
        return 100;
    }
    // Whole percentages, saturating so a huge amount cannot overflow.
    match used.saturating_mul(100) / cap {
        p if p >= 100 => 100,
        p if p >= 80 => 80,
        p if p >= 50 => 50,
        _ => 0,
    }
}

/// Send a summary if one is due and there is something to say. Returns whether one was sent.
pub async fn maybe_send(db: &PgPool, http: &Http, a: &AlertConfig, cfg: &WorkerConfig) -> Result<bool, WorkerError> {
    let mut tx = db.begin().await?;
    // The alert state is locked for the transaction, so two workers cannot send the same summary.
    let state = settings::lock(&mut tx, settings::ALERTS_STATE, &json!({})).await?;
    let now = Utc::now();
    // An interval too large to convert falls back to an hour.
    let every = chrono::Duration::from_std(a.every).unwrap_or(chrono::Duration::hours(1));
    let last: Option<DateTime<Utc>> = state
        .get("last_sent_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    // A summary is sent at most once per interval.
    if last.is_some_and(|l| now - l < every) {
        return Ok(false);
    }
    // The first summary covers one interval.
    let since = last.unwrap_or(now - every);
    let mut lines = Vec::new();
    // Instances that ended failed, unknown or manual-required since the last summary.
    let problems = m::problems_since(&mut tx, since).await?;
    if !problems.is_empty() {
        let parts: Vec<String> = problems.iter().map(|p| format!("{} {}", p.n, p.status)).collect();
        lines.push(format!(
            "{} since {} (resolve them in the admin hub).",
            parts.join(", "),
            since.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    // Spend levels are remembered per month, so each level is reported once a month.
    let month = now.format("%Y-%m").to_string();
    let mut reported = state.pointer(&format!("/spend_levels/{month}")).cloned().unwrap_or_else(|| json!({}));
    for r in m::spend(&mut tx).await? {
        let caps = spend::applicable(
            &mut tx,
            r.platform_definition_id,
            0,
            cfg.ceiling_total_usd_micros,
            cfg.ceiling_per_platform_usd_micros,
        )
        .await?;
        // The platform's own cap decides its level.
        let Some(cap) = caps.iter().find(|c| c.scope == "platform") else { continue };
        let lv = level(r.used_usd_micros, cap.limit_usd_micros);
        let before = reported.get(&r.platform).and_then(Value::as_u64).unwrap_or(0);
        // Only a new, higher level is reported.
        if u64::from(lv) > before {
            lines.push(format!("Spend: {} reached {lv} % of its monthly cap.", r.platform));
            reported[&r.platform] = json!(lv);
        }
    }
    // Nothing to say, nothing sent (and nothing recorded).
    if lines.is_empty() {
        return Ok(false);
    }
    let text = format!("Content hub: {}", lines.join(" "));
    let sent = match discord::webhook_url(&a.base_url, &a.webhook_url) {
        Ok(url) => {
            // An alert can never ping anyone, whatever the text holds.
            let body = json!({ "content": text, "allowed_mentions": { "parse": [] } });
            matches!(http.send(Request::post(url, body)).await, Ok(r) if r.is_success())
        }
        // A bad webhook URL is a failed delivery, reported without repeating it.
        Err(_) => false,
    };
    if !sent {
        tracing::warn!("the alert summary could not be delivered");
    }
    // Recorded either way, so an unreachable channel is not retried every minute; the metrics still show it all.
    let mut next = state.clone();
    // Recorded whether or not delivery worked, so a dead channel is not retried every minute.
    next["last_sent_at"] = json!(now.to_rfc3339());
    next["spend_levels"] = json!({ month: reported });
    settings::set(&mut tx, settings::ALERTS_STATE, &next, crate::ACTOR).await?;
    tx.commit().await?;
    Ok(sent)
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::level;

    /// Levels.
    #[test]
    fn levels() {
        assert_eq!(level(0, 100), 0);
        assert_eq!(level(49, 100), 0);
        assert_eq!(level(50, 100), 50);
        assert_eq!(level(80, 100), 80);
        assert_eq!(level(100, 100), 100);
        assert_eq!(level(1, 0), 100);
        assert_eq!(level(0, 0), 0);
    }
}
