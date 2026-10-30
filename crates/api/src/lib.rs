//! The content hub's two HTTP surfaces, run as separate processes (ARCHITECTURE.md §3):
//!
//! - [`public::router`] — `public` (roadmap. and feeds.), read-only database role, GET only;
//! - [`admin::router`] — `admin` (hub., behind Cloudflare Access + Keto, CSRF-checked), writer role.
//!
//! Each router takes its own state type ([`public::PublicState`], [`admin::AdminState`]), so a handler
//! written for one cannot be mounted on the other; the route tables in [`routes`] are the contract, the OpenAPI
//! documents are built from them, and the tests prove each router answers exactly its table.

#![forbid(unsafe_code)]

pub mod admin;
pub mod auth;
pub mod error;
pub mod extract;
pub mod openapi;
pub mod probe;
pub mod public;
pub mod routes;

use axum::response::{IntoResponse, Response};

/// Any path outside the process's table.
pub(crate) async fn not_found() -> Response {
    error::ApiError::NotFound.into_response()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod handler_tests;
