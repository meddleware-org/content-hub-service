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
//! - [`seed`] — the instance roadmap seed file: its shape and its validation.
//! - [`snapshot`] — the trustless roadmap snapshot: canonical JSON and the Sui personal-message signature.
//! - [`vectors`] — the golden vectors shared with `@meddleware/content-hub-client`.

#![forbid(unsafe_code)]

/// What the worker does with each send outcome.
pub mod delivery;
/// Identifier shapes: keys, platform codes and credential variable names.
pub mod ident;
/// The platform-instance state machine.
pub mod instance_state;
/// The data model's enumerations.
pub mod model;
/// Which hosts and addresses are public.
pub mod net;
/// The pure half of each publisher.
pub mod platform;
/// The generic platform definitions.
pub mod platforms;
/// The roadmap seed file.
pub mod seed;
/// The signed roadmap snapshot.
pub mod snapshot;
/// The task state machine.
pub mod task_state;
/// Canonical content, rendering, counting and segmentation.
pub mod text;
/// The golden vectors.
pub mod vectors;

/// Tests that run the golden vectors against the Rust implementation.
#[cfg(test)]
mod vectors_tests;
