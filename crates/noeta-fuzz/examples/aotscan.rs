//! The **AOT** differential over generated programs: `noeta build --native` must produce a binary
//! that behaves byte-identically to `noeta run` on the same module.
//!
//! ```text
//! cargo build --release -p noeta-cli
//! NOETA_BIN=<target>/release/noeta \
//!   cargo run --release -p noeta-fuzz --features jit --example aotscan -- 40
//! ```
//!
//! # Why this is an example and not a test
//!
//! One `cc` link plus two process launches per program — minutes, not seconds. The corpus version is
//! already the gate arm; this is the out-of-band sweep you run after touching codegen.
//!
//! # Why it writes files instead of calling the comparison directly
//!
//! `noeta_conformance::run_aot_differential` walks a directory of `.noe` files, and it carries far
//! more than a comparison: an aborting side is re-run `ABORT_REPEATS` times before it counts as a
//! divergence, a case whose own truth run disagrees with itself is excluded as nondeterministic, a
//! pure line-reordering is told from a codegen difference, and either side is capped by a timeout.
//! Re-deriving that here would mean re-deriving the judgement too, and the harness's judgement is
//! the part worth reusing — the same reason [`noeta_fuzz::run_target`] borrows `reference_run`
//! rather than lowering the IR itself.
//!
//! The programs are **type-directed**, so every one of them compiles and runs. A syntax-generated
//! sweep would spend most of its `cc` links on programs the checker rejects before they ever reach
//! the linker.

#[cfg(feature = "jit")]
fn main() {
    use std::io::Write as _;

    const SEED: u64 = 0xA07;

    // The harness resolves the `noeta` binary *beside itself*, and an example binary lives one
    // directory deeper (`<target>/release/examples/`) than the CLI. Setting `NOETA_BIN` from here
    // would need `std::env::set_var`, which is `unsafe` and which this workspace forbids outright —
    // so the caller exports it, and the hint below spells out the exact path to use.
    if std::env::var_os("NOETA_BIN").is_none()
        && let Ok(exe) = std::env::current_exe()
        && let Some(cli) = exe
            .parent()
            .and_then(|d| d.parent())
            .map(|d| d.join("noeta"))
    {
        eprintln!(
            "note: an example binary does not sit beside the CLI. If setup fails, run with:\n  \
             NOETA_BIN={} cargo run --release -p noeta-fuzz --features jit --example aotscan",
            cli.display()
        );
    }

    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);

    // Beside the harness's own link workdir, and honoring TMPDIR — not a hidden directory, which
    // the fixture-root rule rejects.
    let dir = std::env::temp_dir().join(format!("noeta-fuzz-aot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the generated-case directory");

    for nonce in 0..n {
        let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
        let path = dir.join(format!("gen{nonce:04}.noe"));
        let mut f = std::fs::File::create(&path).expect("write a generated case");
        f.write_all(src.as_bytes()).expect("write a generated case");
    }
    println!("wrote {n} generated programs to {}", dir.display());

    match noeta_conformance::run_aot_differential(&dir, None) {
        Ok(report) => {
            print!("{}", report.to_human());
            if !report.ok() {
                std::process::exit(1);
            }
        }
        Err(setup) => {
            eprintln!("setup failed: {setup}");
            std::process::exit(2);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(feature = "jit"))]
fn main() {
    println!("built without `--features jit` — nothing was compared");
}
