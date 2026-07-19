fn main() {
    // The language guide (docs/*.md) is embedded at compile time via `include_dir!` in
    // `src/guide.rs` — a dependency cargo cannot see, so a docs-only edit would silently ship a
    // stale guide in every binary embedding this crate. Declare it.
    println!("cargo:rerun-if-changed=../../docs");
}
