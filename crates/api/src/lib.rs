//! The content hub's two HTTP surfaces, run as separate processes (ARCHITECTURE.md §3):
//!
//! - [`public::router`] — `public` (roadmap. and feeds.), read-only database role, GET only;
//! - [`admin::router`] — `admin` (hub., behind Cloudflare Access + Keto, CSRF-checked), writer role.
//!
//! Each router takes its own state type ([`public::PublicState`], [`admin::AdminState`]), so a handler
//! written for one cannot be mounted on the other; the route tables in [`routes`] are the contract, the OpenAPI
//! documents are built from them, and the tests prove each router answers exactly its table.

#![forbid(unsafe_code)]

/// The admin surface.
pub mod admin;
/// Token and role checks.
pub mod auth;
/// The API's error type.
pub mod error;
/// Request extractors.
pub mod extract;
/// The OpenAPI documents.
pub mod openapi;
/// Liveness and readiness.
pub mod probe;
/// The public surface.
pub mod public;
/// The route tables.
pub mod routes;

use axum::response::{IntoResponse, Response};

/// Any path outside the process's table.
pub(crate) async fn not_found() -> Response {
    error::ApiError::NotFound.into_response()
}

/// Unit tests for this module.
#[cfg(test)]
mod tests;

/// Handler tests against a real database (skipped when none is configured).
#[cfg(test)]
mod handler_tests;

/// The OpenID Connect sign-in against a fake provider, over a real database.
#[cfg(test)]
mod oidc_tests;
