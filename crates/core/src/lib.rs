//! Pure content-hub logic, shared by every process: no I/O, no async runtime, no database.
//!
//! - [`model`] — the data model's enumerations (each stored as check-constrained text).
//! - [`task_state`] and [`instance_state`] — the server-side state machines.
//! - [`ident`] — identifier shapes (keys, platform codes, secret names).
//! - [`platforms`] — the generic platform definitions every instance is seeded with.
//!
//! - [`text`] — the canonical post format, renderers, counters, segmentation and intent links.
//! - [`delivery`] — the publishing worker's decisions (state, retry, spend) for each send outcome.
//! - [`platform`] — the pure half of each publisher: request bodies and reply classification.
//! - [`net`] — which hosts and addresses are public (the SSRF rules).
//! - [`snapshot`] — the trustless roadmap snapshot: canonical JSON and the Sui personal-message signature.
//! - [`vectors`] — the golden vectors shared with `@meddleware/content-hub-client`.

#![forbid(unsafe_code)]

pub mod delivery;
pub mod ident;
pub mod instance_state;
pub mod model;
pub mod net;
pub mod platform;
pub mod platforms;
pub mod seed;
pub mod snapshot;
pub mod task_state;
pub mod text;
pub mod vectors;

#[cfg(test)]
mod vectors_tests;
