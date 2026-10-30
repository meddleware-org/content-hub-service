//! Canonical content, rendering, counting and segmentation (ARCHITECTURE.md §8–9). Rust is authoritative; the
//! TypeScript implementation in `@meddleware/content-hub-client` is for previews; `vectors.json` pins both.

pub mod count;
pub mod intent;
pub mod limits;
pub mod markup;
pub mod render;
pub mod segment;
