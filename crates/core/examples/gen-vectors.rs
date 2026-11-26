//! Print the golden vectors: `cargo run -p content-hub-core --example gen-vectors > vectors.json`.
/// Print the vectors document.
fn main() {
    print!("{}", content_hub_core::vectors::generate_text());
}
