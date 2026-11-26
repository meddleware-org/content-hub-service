//! `/healthz` and `/readyz`, shared by both processes.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use content_hub_store::migrate::{self, Readiness, SCHEMA_VERSION};
use serde::Serialize;
use sqlx::PgPool;

/// The liveness answer.
#[derive(Serialize)]
struct Health {
    /// `ok`.
    status: &'static str,
}

/// Liveness: answers without touching the database, so a database outage does not restart the pods.
pub async fn healthz() -> Response {
    Json(Health { status: "ok" }).into_response()
}

/// The readiness answer.
#[derive(Serialize)]
struct Ready {
    /// `ready` or the reason it is not.
    status: &'static str,
    /// The newest applied migration.
    schema_version: Option<i64>,
    /// The migration this build needs.
    required_schema_version: i64,
}

/// Readiness: the database is reachable and migrated to at least this build's schema. A pod that is not
/// ready receives no traffic; the migrate Job runs before the rollout, so this is normally immediate.
pub async fn readyz(pool: &PgPool) -> Response {
    let (status, label, version) = match migrate::readiness(pool).await {
        Readiness::Ready => (StatusCode::OK, "ready", Some(SCHEMA_VERSION)),
        Readiness::SchemaBehind(v) => (StatusCode::SERVICE_UNAVAILABLE, "schema_behind", v),
        Readiness::Unreachable => (StatusCode::SERVICE_UNAVAILABLE, "database_unavailable", None),
    };
    (status, Json(Ready { status: label, schema_version: version, required_schema_version: SCHEMA_VERSION }))
        .into_response()
}
