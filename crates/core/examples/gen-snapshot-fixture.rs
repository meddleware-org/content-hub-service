//! Writes `tests/contract/snapshot-fixture.json` to stdout: `cargo run -p content-hub-core --example gen-snapshot-fixture`.
use content_hub_core::snapshot as snap;
include!("../fixtures/snapshot_fixture.rs");

/// Print the fixture as pretty JSON with a trailing newline.
fn main() {
    let mut s = serde_json::to_string_pretty(&snapshot_fixture()).expect("serialisable");
    s.push('\n');
    print!("{s}");
}
