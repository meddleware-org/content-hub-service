//! The publishing worker (ARCHITECTURE.md §10). One loop, any number of replicas:
//!
//! 1. **Janitor:** a `sending` row older than the claim lease becomes `unknown` (never `queued`).
//! 2. **Claim:** the next due `queued` instance, `FOR UPDATE SKIP LOCKED`. In the same transaction it is either
//!    - stopped (`manual_required`, spend released): platform or target switched off, no publisher, a manual platform;
//!    - deferred (stays `queued`): the target's posts-per-day limit is reached, until midnight UTC;
//!    - dry-run (`manual_required` with a `dry_run` attempt holding the exact redacted request): the send would not
//!      be live (see [`live`]);
//!    - or set to `sending` with an open attempt, and **committed before the external call**.
//! 3. **Send** exactly the stored payload, once.
//! 4. **Finish** in a new transaction: the attempt's outcome and the instance's next state per
//!    `content_hub_core::delivery::decide`, and the spend ledger (settle, release or keep). If the janitor took the
//!    row meanwhile, the instance stays `unknown` and the late result goes to the audit log for the operator.
//!
//! Nothing ever retries an `unknown` send. The stop switch (`publishing.paused`) stops claims; a shutdown lets the
//! send in progress finish.

#![forbid(unsafe_code)]

pub mod alerts;
pub mod metrics;
pub mod snapshot;
pub mod watch;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use content_hub_core::delivery::{self, Decision, Ledger, RetryPolicy};
use content_hub_core::ident::is_secret_ref;
use content_hub_core::instance_state::{Actor, check};
use content_hub_core::model::{InstanceStatus, PostingMode};
use content_hub_core::platform::{Kind, mastodon};
use content_hub_publish::{Context, Http};
use content_hub_store::repo::delivery::{Claim, Finish};
use content_hub_store::repo::{audit, delivery as q, settings, spend, targets};
use serde_json::{Value, json};
use sqlx::PgPool;

/// The audit-log actor of the worker's own moves.
pub const ACTOR: &str = "system:worker";

/// The worker's configuration (from the environment; README → Configuration).
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// `PUBLISHING_LIVE=1`: real sends are possible at all. Off (the default), every send is a dry run.
    pub live: bool,
    /// `CONTENT_HUB_ENVIRONMENT=production`: any live target may be sent to; otherwise only sandbox targets.
    pub production: bool,
    /// Pause between polls when nothing is due.
    pub poll: Duration,
    /// A `sending` row older than this becomes `unknown`. Must exceed the longest send (30 s per request).
    pub lease: Duration,
    pub retry: RetryPolicy,
    /// The posts-per-day ceiling per target (runtime caps can only lower it).
    pub daily_post_limit: i32,
    /// The spend ceilings (for alert levels and metrics).
    pub ceiling_total_usd_micros: i64,
    pub ceiling_per_platform_usd_micros: i64,
    /// How often Mastodon instance limits are refreshed.
    pub limits_every: Duration,
    pub alerts: Option<alerts::AlertConfig>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            live: false,
            production: true,
            poll: Duration::from_secs(5),
            lease: Duration::from_secs(600),
            retry: RetryPolicy::default(),
            daily_post_limit: 5,
            ceiling_total_usd_micros: 0,
            ceiling_per_platform_usd_micros: 0,
            limits_every: Duration::from_secs(24 * 3600),
            alerts: None,
        }
    }
}

/// Why a send would not be live, or `Ok` when it is: the worker's `PUBLISHING_LIVE`, the target's `live`, and
/// outside production the target's `is_sandbox`.
pub fn live(cfg: &WorkerConfig, target_live: bool, is_sandbox: bool) -> Result<(), &'static str> {
    if !cfg.live {
        Err("the worker runs with PUBLISHING_LIVE off")
    } else if !target_live {
        Err("the target is not live")
    } else if !cfg.production && !is_sandbox {
        Err("outside production only sandbox targets receive real sends")
    } else {
        Ok(())
    }
}

/// Looks a credential up by its variable's name.
pub type Secrets = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, for names of the credential shape only.
pub fn env_secrets() -> Secrets {
    Arc::new(|name: &str| if is_secret_ref(name) { std::env::var(name).ok().filter(|v| !v.is_empty()) } else { None })
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Store(#[from] content_hub_store::StoreError),
    #[error("invariant: {0}")]
    Invariant(String),
}

impl From<sqlx::Error> for WorkerError {
    fn from(e: sqlx::Error) -> Self {
        WorkerError::Store(e.into())
    }
}

/// What one tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    Idle,
    Paused,
    Deferred(i64),
    Stopped(i64),
    DryRun(i64),
    Sent(i64, InstanceStatus),
}

pub struct Worker {
    pub db: PgPool,
    pub http: Http,
    pub cfg: WorkerConfig,
    pub secrets: Secrets,
    /// Tests only: replaces every definition's `base_url` (the database requires https; mocks are plain http).
    /// The binary always sets `None`.
    #[doc(hidden)]
    pub base_override: Option<String>,
}

fn jitter() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

impl Worker {
    /// Move stale `sending` rows to `unknown`.
    pub async fn janitor(&self) -> Result<Vec<i64>, WorkerError> {
        let mut tx = self.db.begin().await?;
        let moved = q::janitor(&mut tx, self.cfg.lease.as_secs_f64()).await?;
        for id in &moved {
            audit::record(
                &mut tx,
                ACTOR,
                "instance.janitor",
                "instance",
                Some(id.to_string()),
                Some(json!({ "status": "sending" })),
                Some(json!({ "status": "unknown" })),
            )
            .await?;
        }
        tx.commit().await?;
        if !moved.is_empty() {
            tracing::warn!(instances = ?moved, "stale sends moved to unknown");
        }
        Ok(moved)
    }

    /// Claim and handle at most one due instance.
    pub async fn tick(&self) -> Result<Tick, WorkerError> {
        let mut tx = self.db.begin().await?;
        if settings::paused(&mut tx).await? {
            return Ok(Tick::Paused);
        }
        let Some(c) = q::next_due(&mut tx).await? else { return Ok(Tick::Idle) };
        let mode: Option<PostingMode> = c.posting_mode.parse().ok();
        let kind = mode.and_then(|m| Kind::for_definition(&c.platform_code, m));
        let stop = if !c.platform_enabled || !c.target_enabled {
            Some(format!("{} or its target {} was switched off", c.platform_code, c.target_name))
        } else if matches!(mode, Some(PostingMode::Manual | PostingMode::None) | None) {
            Some(format!("{} is posted by hand", c.platform_code))
        } else if kind.is_none() {
            Some(format!("there is no publisher for {} (post it by hand)", c.platform_code))
        } else {
            None
        };
        if let Some(why) = stop {
            let d = delivery::stopped(&why);
            self.stop(&mut tx, &c, &d, None).await?;
            tx.commit().await?;
            return Ok(Tick::Stopped(c.id));
        }
        let Some(kind) = kind else { return Ok(Tick::Idle) };
        let limit =
            spend::daily_post_limit(&mut tx, c.platform_definition_id, c.target_id, self.cfg.daily_post_limit).await?;
        if q::sent_today(&mut tx, c.target_id).await? >= i64::from(limit) {
            let until = delivery::next_utc_midnight(Utc::now());
            q::defer(
                &mut tx,
                c.id,
                until,
                &format!("waiting for tomorrow: {limit} posts a day to {} (the daily limit)", c.target_name),
            )
            .await?;
            tx.commit().await?;
            return Ok(Tick::Deferred(c.id));
        }
        let secret =
            if kind.needs_secret() { c.target_secret_ref.as_deref().and_then(|n| (self.secrets)(n)) } else { None };
        let crosspost =
            c.target_config.get("crosspost_secret_ref").and_then(Value::as_str).and_then(|n| (self.secrets)(n));
        let now = Utc::now();
        if let Err(why) = live(&self.cfg, c.target_live, c.target_is_sandbox) {
            let ctx = context(&c, self.base(&c), kind, secret.as_deref(), crosspost.as_deref(), now, now);
            let request = content_hub_publish::dry_run(&ctx).unwrap_or_else(|e| json!({ "error": e }));
            let d = delivery::dry_run(why);
            self.stop(&mut tx, &c, &d, Some(&request)).await?;
            tx.commit().await?;
            tracing::info!(instance = c.id, platform = %c.platform_code, "dry run recorded");
            return Ok(Tick::DryRun(c.id));
        }
        check(Actor::Worker, InstanceStatus::Queued, InstanceStatus::Sending)
            .map_err(|e| WorkerError::Invariant(e.to_string()))?;
        let attempt_id = q::begin_send(&mut tx, c.id, &c.payload_sha256).await?;
        let first = q::first_attempt_at(&mut tx, c.id).await?.unwrap_or(now);
        tx.commit().await?; // `sending` is durable before anything leaves the process.

        let ctx = context(&c, self.base(&c), kind, secret.as_deref(), crosspost.as_deref(), first, Utc::now());
        let report = content_hub_publish::send(&self.http, &ctx).await;
        let attempts = u32::try_from(c.attempt_count).unwrap_or(0).saturating_add(1);
        let d = delivery::decide(&report.sent, attempts, &self.cfg.retry, jitter());
        self.finish(&c, attempt_id, &d, &report).await?;
        tracing::info!(instance = c.id, platform = %c.platform_code, to = %d.to, "sent");
        Ok(Tick::Sent(c.id, d.to))
    }

    fn base<'a>(&'a self, c: &'a Claim) -> Option<&'a str> {
        self.base_override.as_deref().or(c.base_url.as_deref())
    }

    async fn stop(
        &self,
        tx: &mut sqlx::PgConnection,
        c: &Claim,
        d: &Decision,
        dry_run_request: Option<&Value>,
    ) -> Result<(), WorkerError> {
        let why = d.last_error.as_deref().unwrap_or("stopped");
        q::stop(tx, c.id, why, dry_run_request.map(|r| (r, c.payload_sha256.as_str()))).await?;
        spend::release(tx, c.id, why).await?;
        audit::record(
            tx,
            ACTOR,
            if dry_run_request.is_some() { "instance.dry_run" } else { "instance.stop" },
            "instance",
            Some(c.id.to_string()),
            Some(json!({ "status": "queued" })),
            Some(json!({ "status": d.to.as_str(), "why": why })),
        )
        .await?;
        Ok(())
    }

    async fn finish(
        &self,
        c: &Claim,
        attempt_id: i64,
        d: &Decision,
        report: &content_hub_publish::Report,
    ) -> Result<(), WorkerError> {
        let mut tx = self.db.begin().await?;
        let next = d.retry_in.and_then(|r| chrono::Duration::from_std(r).ok()).map(|r| Utc::now() + r);
        let outcome = d.outcome.map_or("unknown", |o| o.as_str());
        let f = Finish {
            to: d.to.as_str(),
            outcome,
            url_or_external_id: d.url_or_external_id.as_deref(),
            next_attempt_at: next,
            last_error: d.last_error.as_deref(),
            http_status: report.http_status.map(i32::from),
            response_excerpt: report.excerpt.as_deref(),
            request_redacted: &report.request,
        };
        if q::finish(&mut tx, c.id, attempt_id, &f).await? {
            match d.ledger {
                Ledger::Settle => spend::settle(&mut tx, c.id, c.cost_estimate_usd_micros, "sent").await?,
                Ledger::Release => {
                    spend::release(&mut tx, c.id, "not sent").await?;
                }
                Ledger::Keep => {}
            }
        } else {
            // The janitor already moved it to `unknown`: leave it for an operator, with what actually happened.
            audit::record(
                &mut tx,
                ACTOR,
                "instance.late_result",
                "instance",
                Some(c.id.to_string()),
                None,
                Some(
                    json!({ "outcome": outcome, "would_be": d.to.as_str(), "url_or_external_id": d.url_or_external_id,
                              "http_status": report.http_status }),
                ),
            )
            .await?;
            tracing::warn!(instance = c.id, "a send finished after the janitor moved it to unknown");
        }
        tx.commit().await?;
        Ok(())
    }

    /// Refresh each enabled Mastodon target's instance limits when they are older than `limits_every`.
    pub async fn refresh_mastodon_limits(&self) -> Result<usize, WorkerError> {
        let mut c = self.db.acquire().await?;
        let mut n = 0;
        for t in targets::enabled_for_code(&mut c, "MASTODON").await? {
            let fresh = t
                .config
                .pointer("/instance_limits/fetched_at")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|at| {
                    (Utc::now() - at.with_timezone(&Utc)).to_std().is_ok_and(|age| age < self.cfg.limits_every)
                });
            if fresh {
                continue;
            }
            let Ok(base) = mastodon::base(t.external_ref.as_deref()) else { continue };
            match content_hub_publish::mastodon_limits(&self.http, &base).await {
                Ok(l) => {
                    let v = json!({ "max_characters": l.max_characters,
                                    "characters_reserved_per_url": l.characters_reserved_per_url,
                                    "fetched_at": Utc::now().to_rfc3339() });
                    targets::set_config_key(&mut c, t.id, "instance_limits", &v).await?;
                    n += 1;
                }
                Err(e) => tracing::warn!(target = t.id, error = %e, "instance limits not refreshed"),
            }
        }
        Ok(n)
    }

    /// Run until `stop` turns true: janitor, claims, and the periodic jobs. A send in progress always finishes.
    pub async fn run(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        let mut last_limits: Option<std::time::Instant> = None;
        let mut last_alert: Option<std::time::Instant> = None;
        loop {
            if *stop.borrow() {
                break;
            }
            if let Err(e) = self.janitor().await {
                tracing::error!(error = %e, "janitor");
            }
            if last_limits.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
                last_limits = Some(std::time::Instant::now());
                if let Err(e) = self.refresh_mastodon_limits().await {
                    tracing::error!(error = %e, "instance limits");
                }
            }
            if let Some(a) = &self.cfg.alerts
                && last_alert.is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
            {
                last_alert = Some(std::time::Instant::now());
                if let Err(e) = alerts::maybe_send(&self.db, &self.http, a, &self.cfg).await {
                    tracing::error!(error = %e, "alerts");
                }
            }
            let busy = match self.tick().await {
                Ok(Tick::Idle | Tick::Paused) => false,
                Ok(_) => true,
                Err(e) => {
                    tracing::error!(error = %e, "worker tick");
                    false
                }
            };
            if !busy {
                tokio::select! {
                    _ = tokio::time::sleep(self.cfg.poll) => {}
                    _ = stop.changed() => {}
                }
            }
        }
        tracing::info!("worker stopped");
    }
}

fn context<'a>(
    c: &'a Claim,
    base_url: Option<&'a str>,
    kind: Kind,
    secret: Option<&'a str>,
    crosspost: Option<&'a str>,
    first_attempt_at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
) -> Context<'a> {
    Context {
        kind,
        base_url,
        external_ref: c.target_external_ref.as_deref(),
        config: &c.target_config,
        secret,
        crosspost_secret: crosspost,
        title: &c.payload_title,
        body: &c.payload_body,
        post_public_id: c.post_public_id,
        idempotency_key: c.idempotency_key,
        first_attempt_at,
        now,
    }
}

#[cfg(test)]
mod tests;
