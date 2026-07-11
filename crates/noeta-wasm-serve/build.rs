//! Embed the served program (P-WASM W4, interim): `NOETA_SERVE_BUNDLE` names the `.noeb` to
//! bake in; unset builds an empty placeholder (the component then answers 500 with a build hint,
//! rather than failing to compile — so `cargo check`/CI cover the crate without an app). The
//! staple-into-component rewrite (the no-cargo path, like `noeta build --wasm`'s) is a recorded
//! follow-up in `plans/wasm/`.

fn main() {
    println!("cargo::rerun-if-env-changed=NOETA_SERVE_BUNDLE");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dest = out.join("bundle.noeb");
    match std::env::var_os("NOETA_SERVE_BUNDLE") {
        Some(path) => {
            println!("cargo::rerun-if-changed={}", path.to_string_lossy());
            std::fs::copy(&path, &dest).unwrap_or_else(|e| {
                panic!(
                    "NOETA_SERVE_BUNDLE={}: cannot copy: {e}",
                    path.to_string_lossy()
                )
            });
        }
        None => {
            std::fs::write(&dest, []).expect("write placeholder bundle");
        }
    }
}
