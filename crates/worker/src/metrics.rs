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

#[derive(Clone)]
pub struct MetricsState {
    pub db: PgPool,
    pub cfg: Arc<WorkerConfig>,
}

pub fn router(state: MetricsState) -> Router {
    Router::new()
        .route("/healthz", get(probe::healthz))
        .route("/readyz", get(|State(s): State<MetricsState>| async move { probe::readyz(&s.db).await }))
        .route("/metrics", get(metrics))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state)
}

async fn metrics(State(s): State<MetricsState>) -> Response {
    match render(&s.db, &s.cfg).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "metrics");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// One metric family: its header and samples.
fn family(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(String, i64)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (labels, v) in samples {
        let _ = writeln!(out, "{name}{labels} {v}");
    }
}

/// The exposition text.
pub async fn render(db: &PgPool, cfg: &WorkerConfig) -> Result<String, content_hub_store::StoreError> {
    let mut c = db.acquire().await?;
    let mut o = String::new();
    let by_status: Vec<_> = m::instances_by_status(&mut c)
        .await?
        .into_iter()
        .map(|r| (format!("{{status=\"{}\"}}", r.status), r.n))
        .collect();
    family(&mut o, "content_hub_instances", "gauge", "Platform instances by status.", &by_status);
    let attempts: Vec<_> = m::attempts(&mut c)
        .await?
        .into_iter()
        .map(|r| (format!("{{platform=\"{}\",outcome=\"{}\"}}", r.platform, r.outcome), r.n))
        .collect();
    family(
        &mut o,
        "content_hub_delivery_attempts_total",
        "counter",
        "Finished delivery attempts by platform and outcome.",
        &attempts,
    );
    let mut used = Vec::new();
    // The total cap (the runtime row, bounded by the ceiling), whether or not anything was spent this month.
    let total = spend::applicable(&mut c, 0, 0, cfg.ceiling_total_usd_micros, cfg.ceiling_per_platform_usd_micros)
        .await?
        .into_iter()
        .find(|a| a.scope == "total")
        .map_or(cfg.ceiling_total_usd_micros, |a| a.limit_usd_micros);
    let mut caps = vec![("{scope=\"total\"}".to_owned(), total)];
    for r in m::spend(&mut c).await? {
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
    family(&mut o, "content_hub_spend_cap_usd_micros", "gauge", "The monthly cap that applies, by scope.", &caps);
    let paused = i64::from(settings::paused(&mut c).await?);
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
