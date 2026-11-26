//! Canonical content, rendering, counting and segmentation (ARCHITECTURE.md §8–9). Rust is authoritative; the
//! TypeScript implementation in `@meddleware/content-hub-client` is for previews; `vectors.json` pins both.

/// Counting a text the way a platform does.
pub mod count;
/// Filling manual-post link templates.
pub mod intent;
/// A platform's limits and the check of a post against them.
pub mod limits;
/// The canonical format's parser.
pub mod markup;
/// Rendering a parsed post for a platform.
pub mod render;
/// Splitting a text where each target's limit allows.
pub mod segment;
