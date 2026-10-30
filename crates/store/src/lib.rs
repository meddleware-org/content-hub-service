//! PostgreSQL for the content hub: connection settings, migrations and (from the next step) every SQL
//! statement. No SQL exists outside this crate.
//!
//! Roles (ARCHITECTURE.md §6): the `migrate` Job connects as the database OWNER (DDL); `admin`, `worker`,
//! `seed` and `export-snapshot` as the WRITER (DML on schema `hub`, no DDL); `public` as the READ-ONLY role,
//! which can read only the `public.public_v1_*` views.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod migrate;
pub mod repo;

#[cfg(any(test, feature = "testing"))]
pub mod testdb;

#[cfg(test)]
mod tests;

pub use config::{Access, ConfigError, DatabaseSettings};
pub use error::{Result, StoreError};
