//! The worker's HTTP surface (no ingress; probed and scraped in the cluster): `/healthz`, `/readyz` and `/metrics`
//! (Prometheus text format). The numbers come from the database at scrape time, so every replica reports the same
//! values and nothing is lost on a restart. Alert rules (50 / 80 / 100 % of a cap) belong in the monitoring stack;
//! the optional chat summary is in [`crate::alerts`].

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use content_hub_api::probe;
use content_hub_store::repo::{metrics as m, settings, spend};
use sqlx::PgPool;

use crate::WorkerConfig;

/// The state of the metrics endpoint.
#[derive(Clone)]
pub struct MetricsState {
    /// The database pool (the read side is enough).
    pub db: PgPool,
    /// The worker's configuration.
    pub cfg: Arc<WorkerConfig>,
    /// The wallet readings (empty when no wallet is watched).
    pub watch: Arc<crate::watch::Watch>,
}

/// The metrics process: `/healthz`, `/readyz` and `/metrics` in the Prometheus text format.
pub fn router(state: MetricsState) -> Router {
    Router::new()
        // The probes are the same as the other processes'.
        .route("/healthz", get(probe::healthz))
        .route("/readyz", get(|State(s): State<MetricsState>| async move { probe::readyz(&s.db).await }))
        .route("/metrics", get(metrics))
        // Nothing else is served here.
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state)
}

/// `GET /metrics`: the metrics as text, or a 503 if they cannot be read.
async fn metrics(State(s): State<MetricsState>) -> Response {
    match render(&s.db, &s.cfg).await {
        Ok(mut text) => {
            // The wallet readings are kept in memory, so they are appended to the database figures.
            text.push_str(&s.watch.render());
            ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response()
        }
        Err(e) => {
            // The error goes to the log; the caller only sees a 503.
            tracing::error!(error = %e, "metrics");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// One metric family: its header and samples. Each family is written as a `# HELP` line, a `# TYPE` line and then one
/// line per sample, which is the Prometheus text format.
fn family(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(String, i64)]) {
    // Prometheus text format: a help line and a type line, then one line per sample.
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (labels, v) in samples {
        let _ = writeln!(out, "{name}{labels} {v}");
    }
}

/// The exposition text.
pub async fn render(db: &PgPool, cfg: &WorkerConfig) -> Result<String, content_hub_store::StoreError> {
    // One connection for every query, so the figures describe the same moment.
    let mut c = db.acquire().await?;
    // The text is built in memory and returned whole.
    let mut o = String::new();
    // A gauge: how many instances are in each status right now.
    let by_status: Vec<_> = m::instances_by_status(&mut c)
        .await?
        .into_iter()
        // Statuses are fixed words, so they are safe as label values.
        .map(|r| (format!("{{status=\"{}\"}}", r.status), r.n))
        .collect();
    family(&mut o, "content_hub_instances", "gauge", "Platform instances by status.", &by_status);
    // A counter: finished attempts by platform and outcome.
    let attempts: Vec<_> = m::attempts(&mut c)
        .await?
        .into_iter()
        // Platform codes and outcomes are fixed words too.
        .map(|r| (format!("{{platform=\"{}\",outcome=\"{}\"}}", r.platform, r.outcome), r.n))
        .collect();
    family(
        &mut o,
        "content_hub_delivery_attempts_total",
        "counter",
        "Finished delivery attempts by platform and outcome.",
        &attempts,
    );
    // This month's use by platform, and the caps that apply.
    let mut used = Vec::new();
    // The total cap (the runtime row, bounded by the ceiling), whether or not anything was spent this month.
    let total = spend::applicable(&mut c, 0, 0, cfg.ceiling_total_usd_micros, cfg.ceiling_per_platform_usd_micros)
        .await?
        .into_iter()
        .find(|a| a.scope == "total")
        .map_or(cfg.ceiling_total_usd_micros, |a| a.limit_usd_micros);
    // The total cap is always present, even when nothing has been spent.
    let mut caps = vec![("{scope=\"total\"}".to_owned(), total)];
    for r in m::spend(&mut c).await? {
        // This month's use of each platform.
        used.push((format!("{{platform=\"{}\"}}", r.platform), r.used_usd_micros));
        let applicable = spend::applicable(
            &mut c,
            r.platform_definition_id,
            0,
            cfg.ceiling_total_usd_micros,
            cfg.ceiling_per_platform_usd_micros,
        )
        .await?;
        if let Some(a) = applicable.iter().find(|a| a.scope == "platform") {
            caps.push((format!("{{scope=\"platform\",platform=\"{}\"}}", r.platform), a.limit_usd_micros));
        }
    }
    family(
        &mut o,
        "content_hub_spend_used_usd_micros",
        "gauge",
        "This month's spend (reserved and settled) by platform.",
        &used,
    );
    // The caps, so a dashboard can draw use against limit.
    family(&mut o, "content_hub_spend_cap_usd_micros", "gauge", "The monthly cap that applies, by scope.", &caps);
    // The stop switch as 0 or 1, so an alert can fire while publishing is paused.
    let paused = i64::from(settings::paused(&mut c).await?);
    // The latest snapshot per network, for freshness and expiry alerts.
    let snapshots = m::latest_snapshots(&mut c).await?;
    let generated: Vec<_> =
        snapshots.iter().map(|s| (format!("{{network=\"{}\"}}", s.network), s.generated_at.timestamp())).collect();
    family(
        &mut o,
        // A snapshot that has not been regenerated for too long is itself an alert.
        "content_hub_snapshot_generated_timestamp_seconds",
        "gauge",
        "Unix time the latest signed roadmap snapshot was generated, by network (absent before the first export).",
        &generated,
    );
    let expires: Vec<_> = snapshots
        .iter()
        // Only stored snapshots have an expiry.
        .filter_map(|s| s.expires_at.map(|t| (format!("{{network=\"{}\"}}", s.network), t.timestamp())))
        .collect();
    family(
        &mut o,
        "content_hub_snapshot_storage_expires_timestamp_seconds",
        "gauge",
        "Unix time the latest snapshot's Walrus storage is estimated to end (the export renews it well before).",
        &expires,
    );
    family(
        &mut o,
        "content_hub_publishing_paused",
        "gauge",
        "1 while the stop switch is on.",
        &[(String::new(), paused)],
    );
    family(
        &mut o,
        "content_hub_worker_live",
        "gauge",
        "1 when this worker may send for real (PUBLISHING_LIVE).",
        &[(String::new(), i64::from(cfg.live))],
    );
    Ok(o)
}
