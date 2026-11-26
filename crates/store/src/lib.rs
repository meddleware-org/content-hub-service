//! PostgreSQL for the content hub: connection settings, migrations and (from the next step) every SQL
//! statement. No SQL exists outside this crate.
//!
//! Roles (ARCHITECTURE.md §6): the `migrate` Job connects as the database OWNER (DDL); `admin`, `worker`,
//! `seed` and `export-snapshot` as the WRITER (DML on schema `hub`, no DDL); `public` as the READ-ONLY role,
//! which can read only the `public.public_v1_*` views.

#![forbid(unsafe_code)]

/// Connection settings.
pub mod config;
/// The store's error type.
pub mod error;
/// Running migrations and checking readiness.
pub mod migrate;
/// Every SQL statement, by table.
pub mod repo;

/// Throw-away databases for tests.
#[cfg(any(test, feature = "testing"))]
pub mod testdb;

/// Unit tests for this module.
#[cfg(test)]
mod tests;

pub use config::{Access, ConfigError, DatabaseSettings};
pub use error::{Result, StoreError};
