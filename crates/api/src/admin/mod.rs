//! The admin process's router: every `/api/v1` route behind `auth::require_admin`, as the WRITER role. Every change
//! commits with its audit-log row in one transaction.

pub(crate) mod ops;
pub(crate) mod posts;
pub(crate) mod prepare;
pub(crate) mod publishing;
pub(crate) mod roadmap;
pub(crate) mod tasks;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router, middleware};
use serde::Serialize;
use sqlx::PgPool;

use crate::auth::{AuthState, Authenticator, Identity, require_admin};
use crate::error::{ApiError, ApiResult};
use crate::{not_found, openapi, probe};

/// The instance's spend ceilings (layer 2 of the cost caps): configuration, never data. Runtime caps must stay at or
/// below them. Every default is 0.
#[derive(Debug, Clone, Copy, Default, Serialize, utoipa::ToSchema)]
pub struct Ceilings {
    pub total_monthly_usd_micros: i64,
    pub per_platform_monthly_usd_micros: i64,
}

/// The admin process's state.
#[derive(Clone)]
pub struct AdminState {
    /// A pool of the writer role (`Access::ReadWrite`).
    pub db: PgPool,
    pub auth: Arc<Authenticator>,
    pub ceilings: Ceilings,
}

impl AuthState for AdminState {
    fn authenticator(&self) -> &Arc<Authenticator> {
        &self.auth
    }
    fn db(&self) -> &PgPool {
        &self.db
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Me {
    pub subject: String,
    pub display_name: String,
}

async fn me(Extension(id): Extension<Identity>) -> Json<Me> {
    Json(Me { subject: id.subject, display_name: id.display_name })
}

async fn admin_openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi::admin())
}

pub fn router(state: AdminState) -> Router {
    let api: Router<AdminState> = Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/openapi.json", get(admin_openapi))
        .route("/api/v1/phases", get(roadmap::list_phases).post(roadmap::create_phase))
        .route("/api/v1/phases/{id}", get(roadmap::get_phase).patch(roadmap::patch_phase))
        .route("/api/v1/items", get(roadmap::list_items).post(roadmap::create_item))
        .route("/api/v1/items/{id}", get(roadmap::get_item).patch(roadmap::patch_item))
        .route("/api/v1/tasks", get(tasks::list).post(tasks::create))
        .route("/api/v1/tasks/{id}", get(tasks::get_one).patch(tasks::patch))
        .route("/api/v1/tasks/{id}/transition", post(tasks::transition))
        .route("/api/v1/tasks/{id}/links", post(tasks::add_link))
        .route("/api/v1/tasks/{id}/links/{link_id}", delete(tasks::delete_link))
        .route("/api/v1/posts", get(posts::list).post(posts::create))
        .route("/api/v1/posts/{id}", get(posts::get_one).patch(posts::patch))
        .route("/api/v1/posts/{id}/enqueue", post(posts::enqueue))
        .route("/api/v1/instances", get(publishing::list_instances))
        .route("/api/v1/instances/{id}", get(publishing::get_instance))
        .route("/api/v1/instances/{id}/transition", post(publishing::transition_instance))
        .route("/api/v1/platforms", get(publishing::list_platforms))
        .route("/api/v1/platforms/{id}", axum::routing::patch(publishing::patch_platform))
        .route("/api/v1/targets", get(publishing::list_targets).post(publishing::create_target))
        .route("/api/v1/targets/{id}", axum::routing::patch(publishing::patch_target))
        .route("/api/v1/budgets", get(ops::get_budgets).put(ops::put_budgets))
        .route("/api/v1/publishing", get(ops::get_publishing).put(ops::put_publishing))
        .route("/api/v1/preview/roadmap", get(ops::preview_roadmap))
        .route("/api/v1/schedule", get(ops::schedule))
        .route("/api/v1/stats", get(ops::stats))
        .route("/api/v1/audit", get(ops::audit))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin::<AdminState>));
    Router::new()
        .route("/healthz", get(probe::healthz))
        .route("/readyz", get(|State(s): State<AdminState>| async move { probe::readyz(&s.db).await }))
        .merge(api)
        .fallback(not_found)
        // A post body is at most 100,000 characters (about 400 KB of UTF-8); 1 MiB bounds every request.
        .layer(axum::extract::DefaultBodyLimit::max(1 << 20))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            std::time::Duration::from_secs(30),
        ))
        .with_state(state)
}

// ── shared validation helpers ───────────────────────────────────────────────────────────────────────────

pub(crate) fn text(field: &str, s: &str, min: usize, max: usize) -> ApiResult<()> {
    let n = s.chars().count();
    if n < min || n > max {
        return Err(ApiError::Invalid(format!("{field} must be {min} to {max} characters")));
    }
    Ok(())
}

pub(crate) fn key(field: &str, s: &str) -> ApiResult<()> {
    if content_hub_core::ident::is_key(s) {
        Ok(())
    } else {
        Err(ApiError::Invalid(format!("{field} must match ^[a-z0-9][a-z0-9-]{{0,62}}$")))
    }
}

/// An `https` URL with a public host name or address. Names that only resolve inside a network (`localhost`,
/// `.local`, `.internal`, `.localhost`, single labels) and private, loopback, link-local, shared, documentation and
/// unspecified addresses are refused. The admin process never fetches these URLs; whatever does checks the resolved
/// address again before connecting.
pub(crate) fn https_url(field: &str, s: &str) -> ApiResult<()> {
    let bad = || ApiError::Invalid(format!("{field} must be an https URL to a public host, without spaces"));
    if !s.starts_with("https://") || s.chars().count() > 2000 || s.contains(char::is_whitespace) {
        return Err(bad());
    }
    let u = url::Url::parse(s).map_err(|_| bad())?;
    let public = match u.host() {
        Some(url::Host::Domain(d)) => content_hub_core::net::is_public_name(d),
        Some(url::Host::Ipv4(a)) => content_hub_core::net::is_public_ip(a.into()),
        Some(url::Host::Ipv6(a)) => content_hub_core::net::is_public_ip(a.into()),
        None => false,
    };
    if public && u.username().is_empty() && u.password().is_none() { Ok(()) } else { Err(bad()) }
}

pub(crate) fn parse_enum<T: std::str::FromStr>(field: &str, s: &str) -> ApiResult<T> {
    s.parse::<T>().map_err(|_| ApiError::Invalid(format!("{field}: unknown value {s:?}")))
}

/// Canonical text uses `\n` line endings only.
pub(crate) fn normalise_body(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub(crate) fn json<T: Serialize>(v: &T) -> Option<serde_json::Value> {
    serde_json::to_value(v).ok()
}

pub(crate) fn found<T>(o: Option<T>) -> ApiResult<T> {
    o.ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::https_url;

    #[test]
    fn https_urls_must_name_public_hosts() {
        for ok in
            ["https://example.org/a?b=c", "https://forum.example.org/", "https://8.8.8.8/", "https://[2606:4700::1]/"]
        {
            assert!(https_url("u", ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.org/",
            "https://",
            "https://localhost/",
            "https://intranet/",
            "https://printer.local/",
            "https://db.internal/",
            "https://postgres.database.svc.cluster.local/",
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://172.16.5.4/",
            "https://192.168.1.20/",
            "https://169.254.169.254/latest/meta-data/",
            "https://100.64.0.1/",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[fd00::1]/",
            "https://[::ffff:10.0.0.1]/",
            "https://user:pw@example.org/",
            "https://example.org/a b",
        ] {
            assert!(https_url("u", bad).is_err(), "{bad}");
        }
    }
}
