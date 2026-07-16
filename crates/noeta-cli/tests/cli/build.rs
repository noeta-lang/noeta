//! `noeta build`: `.noeb` bundles (P-AOT L1.2), `--exe` self-contained executables (L2), and
//! `--native` machine-code AOT (L3).

use crate::support::*;

// --- `build` / `.noeb` bundles (P-AOT L1.2) -----------------------------------------

#[test]
fn build_then_run_bundle_matches_source_run() {
    // A bundle runs byte-for-byte like its source, but ships no `.noe`.
    let file = temp_program(
        "build_roundtrip",
        "fn sq(n: int): int { return n * n }\nmut t = 0\nfor i in 0..5 {\n    t = t + sq(i)\n}\necho t\n",
    );
    let bundle = file.with_extension("noeb");

    // Source run — the reference output.
    let src_out = lang().arg("run").arg(&file).assert().success();
    let src_stdout = String::from_utf8(src_out.get_output().stdout.clone()).unwrap();
    assert_eq!(src_stdout.trim(), "30");

    // Build the bundle, then run it — identical stdout, and the artifact starts with the magic.
    lang().arg("build").arg(&file).assert().success();
    let blob = std::fs::read(&bundle).expect("bundle written");
    assert_eq!(&blob[0..4], b"NOEB", "artifact carries the .noeb magic");

    lang()
        .arg("run")
        .arg(&bundle)
        .assert()
        .success()
        .stdout(predicate::str::diff(src_stdout));
}

// --- `build --exe` / self-contained executables (P-AOT L2) --------------------------

#[test]
fn build_exe_runs_the_program_with_no_source_or_bundle_alongside() {
    // `noeta build --exe -o app` staples the compiled program onto a copy of the LEAN `noeta-runner`
    // (dev-deps D4a — not the toolchain). The resulting `app` runs the program directly — no `.noe`,
    // no `.noeb`, no `noeta run` — and its stdout matches a source run byte-for-byte. Running it also
    // proves the startup trailer detection fires (the runner would otherwise print usage).
    let Some(runner) = lean_runner_path() else {
        return; // no build toolchain for the lean runner — skip.
    };
    let file = temp_program(
        "build_exe",
        "fn sq(n: int): int { return n * n }\nmut t = 0\nfor i in 0..5 {\n    t = t + sq(i)\n}\necho t\n",
    );
    let app = file.parent().unwrap().join("app");
    let _ = std::fs::remove_file(&app);

    lang()
        .env("NOETA_RUNNER", &runner)
        .arg("build")
        .arg(&file)
        .arg("--exe")
        .arg("-o")
        .arg(&app)
        .assert()
        .success()
        .stderr(predicate::str::contains("self-contained"));

    // The artifact carries neither the `.noeb` magic at its head (it starts with the runtime image)
    // nor the source; running it on its own yields the program's output.
    Command::new(&app).assert().success().stdout("30\n");
    let _ = std::fs::remove_file(&app);
}

#[test]
fn build_exe_reports_a_runtime_abort_from_the_stapled_program() {
    // A panic in a stapled exe surfaces as the same abort a source/bundle run gives — the embedded
    // program runs on the real host through the identical path, just with no source text to quote.
    let Some(runner) = lean_runner_path() else {
        return; // no build toolchain for the lean runner — skip.
    };
    let file = temp_program("build_exe_panic", "echo \"before\"\npanic(\"boom\")\n");
    let app = file.parent().unwrap().join("app_panic");
    let _ = std::fs::remove_file(&app);

    lang()
        .env("NOETA_RUNNER", &runner)
        .arg("build")
        .arg(&file)
        .arg("--exe")
        .arg("-o")
        .arg(&app)
        .assert()
        .success();

    Command::new(&app)
        .assert()
        .failure()
        .stdout("before\n")
        .stderr(predicate::str::contains("panic: boom"));
    let _ = std::fs::remove_file(&app);
}

// --- `build --native` / native AOT executables (P-AOT L3) ---------------------------

/// Build the AOT runtime staticlib (`libnoeta_aot.a`) for `--native` to link against, reusing the
/// workspace's `target/debug` (so its deps are already compiled), and return the archive path plus
/// the native-static-libs link line rustc reports. `None` if the toolchain can't produce it (no
/// `cargo`, or the build failed) — the caller then skips, so the differential is a no-op on a host
/// without a build toolchain rather than a spurious failure.
#[cfg(feature = "jit")]
fn build_aot_archive() -> Option<(PathBuf, String)> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(workspace())
        // The link line is scraped from rustc's `native-static-libs` note; under
        // `CARGO_TERM_COLOR=always` (CI) the note arrives ANSI-colored and a stray `\x1b[0m`
        // ends up inside the last `-l` flag. Force plain output regardless of ambient config.
        .env("CARGO_TERM_COLOR", "never")
        .args([
            "rustc",
            "-p",
            "noeta-aot-runtime",
            "--",
            "--print",
            "native-static-libs",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping native AOT test: building the runtime archive failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let notes = String::from_utf8_lossy(&output.stderr);
    let libs = notes
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| libs.trim().to_string())
        .unwrap_or_default();
    let archive = workspace().join("target/debug/libnoeta_aot.a");
    archive.exists().then_some((archive, libs))
}

#[test]
#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build (it exits 2 otherwise).
fn build_native_matches_a_source_run_byte_for_byte() {
    // P-AOT L3.2b(3), the end-to-end AOT differential: `noeta build --native` compiles the eligible
    // prototypes to machine code, links them into a native binary, and staples the bundle on. That
    // binary — dispatching native bodies for the hot loop / `sq` / `fib` and interpreting the rest —
    // must produce exactly what `noeta run` produces on the same source. This is the linked-binary
    // proof the in-process test (`aot_bound_dispatch_runs_native_in_process`) forecast: only the
    // linker was unproven, and here it runs for real.
    let Some((archive, libs)) = build_aot_archive() else {
        return; // no build toolchain for the runtime archive — skip.
    };
    if !has_cc() {
        eprintln!("skipping native AOT test: no `cc` on PATH");
        return;
    }

    let src = "fn sq(n: int): int { return n * n }\n\
               fn fib(n: int): int { if n < 2 { return n }\n  return fib(n - 1) + fib(n - 2) }\n\
               mut t = 0\nfor i in 0..1000 { t = t + sq(i) }\n\
               echo t\necho fib(20)\necho \"done\"\n";
    let file = temp_program("build_native", src);
    let app = file.parent().unwrap().join("app_native");
    let _ = std::fs::remove_file(&app);

    // Reference: a plain source run.
    let reference = lang().arg("run").arg(&file).assert().success();
    let expected = String::from_utf8(reference.get_output().stdout.clone()).unwrap();

    // Build the native binary, pointing the linker driver at the archive we just built.
    lang()
        .arg("build")
        .arg(&file)
        .arg("--native")
        .arg("-o")
        .arg(&app)
        .env("NOETA_AOT_RUNTIME_LIB", &archive)
        .env("NOETA_AOT_LINK_LIBS", &libs)
        .assert()
        .success()
        .stderr(predicate::str::contains("native AOT"));

    // The native binary runs on its own and matches the source run exactly.
    Command::new(&app).assert().success().stdout(expected);
    let _ = std::fs::remove_file(&app);
}

/// The names of every section in an ELF64 (LE) image — enough to assert a binary was stripped.
/// Hand-rolled so the test needs no `object`/`goblin` dependency; returns `None` for a non-ELF64
/// input (e.g. a macOS Mach-O host), letting the caller skip rather than false-fail.
#[cfg(feature = "jit")]
fn elf_section_names(bytes: &[u8]) -> Option<Vec<String>> {
    let u16le = |o: usize| -> Option<usize> {
        Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?) as usize)
    };
    let u32le = |o: usize| -> Option<usize> {
        Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as usize)
    };
    let u64le = |o: usize| -> Option<usize> {
        Some(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?) as usize)
    };
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 {
        return None; // not ELF64 — skip on this host.
    }
    let (shoff, shentsize, shnum, shstrndx) = (u64le(40)?, u16le(58)?, u16le(60)?, u16le(62)?);
    // The section-name string table blob, located via the shstrndx'th section header.
    let strh = shoff + shstrndx * shentsize;
    let (str_off, str_size) = (u64le(strh + 24)?, u64le(strh + 32)?);
    let strtab = bytes.get(str_off..str_off + str_size)?;
    let mut names = Vec::with_capacity(shnum);
    for i in 0..shnum {
        let name_off = u32le(shoff + i * shentsize)?;
        let end = strtab[name_off..].iter().position(|&b| b == 0)? + name_off;
        names.push(String::from_utf8_lossy(&strtab[name_off..end]).into_owned());
    }
    Some(names)
}

#[test]
#[cfg(feature = "jit")] // `--native` exists only in the JIT-enabled build.
fn build_native_strips_debug_info_from_the_shipped_binary() {
    // A shipped `--native` artifact carries no native debug symbols (`-s` at link, native-size slice
    // 1): its panic tracebacks come from the bundle's own line table, not DWARF, so stripping is free
    // and halves the image (~11 MB → ~5.8 MB on a core program). Guard it structurally — assert the
    // ELF has no `.debug_*` or `.symtab`/`.strtab` sections — rather than by a brittle size ceiling.
    let Some((archive, libs)) = build_aot_archive() else {
        return; // no build toolchain for the runtime archive — skip.
    };
    if !has_cc() {
        eprintln!("skipping native strip test: no `cc` on PATH");
        return;
    }
    let file = temp_program("build_native_strip", "echo \"ok\"\n");
    let app = file.parent().unwrap().join("app_native_strip");
    let _ = std::fs::remove_file(&app);
    lang()
        .arg("build")
        .arg(&file)
        .arg("--native")
        .arg("-o")
        .arg(&app)
        .env("NOETA_AOT_RUNTIME_LIB", &archive)
        .env("NOETA_AOT_LINK_LIBS", &libs)
        .assert()
        .success();

    // Still runs (the stapled bundle survived the strip — it is appended *after* the linked ELF that
    // `-s` stripped) …
    Command::new(&app).assert().success().stdout("ok\n");

    // … and carries no debug/symbol sections.
    let bytes = std::fs::read(&app).unwrap();
    if let Some(sections) = elf_section_names(&bytes) {
        // `.debug_gdb_scripts` is a ~40-byte gdb-autoload stub cranelift emits into the AOT object,
        // not DWARF — `-s` leaves it and it costs nothing, so it is not what "stripped" is about here.
        let leftover: Vec<_> = sections
            .iter()
            .filter(|n| {
                (n.starts_with(".debug") && n.as_str() != ".debug_gdb_scripts")
                    || n.as_str() == ".symtab"
                    || n.as_str() == ".strtab"
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "shipped --native binary should be stripped, but found sections: {leftover:?}"
        );
    }
    let _ = std::fs::remove_file(&app);
}

#[test]
fn aot_runtime_does_not_link_the_compiler_frontend() {
    // native-size slice 2 (structural guard): a shipped `--native` binary runs a *pre-compiled*
    // stapled bundle and must never carry the type checker / compiler / IR pipeline — dead weight in
    // a run-only artifact AND reachable attack surface (the same property the dev-deps arc gave dev
    // tooling, one layer down: L2 out of a shipped L1). The AOT runtime opts out of noeta-vm's
    // `compile` feature; assert the front-end is absent from its (non-dev) dependency graph, so a
    // future edit that re-links it (a new default feature, a compiler dep on noeta-runtime) fails HERE
    // rather than silently re-bloating and re-arming the artifact.
    let out = Command::new(env!("CARGO"))
        .current_dir(workspace())
        .args([
            "tree",
            "-p",
            "noeta-aot-runtime",
            "-e",
            "no-dev",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    for forbidden in [
        "noeta-compiler v",
        "noeta-check v",
        "noeta-ir v",
        "noeta-ir-passes v",
    ] {
        assert!(
            !tree.contains(forbidden),
            "the AOT runtime must not link `{}` — a run-only artifact carries no compiler front-end \
             (native-size slice 2). Graph:\n{tree}",
            forbidden.trim_end_matches(" v")
        );
    }
}

#[test]
fn bundle_run_rejects_build_time_flags() {
    // Tiers are baked at build time; passing them to a bundle run is a usage error, not a silent
    // no-op.
    let file = temp_program("build_flag_reject", "echo 1\n");
    lang().arg("build").arg(&file).assert().success();
    let bundle = file.with_extension("noeb");
    lang()
        .arg("run")
        .arg(&bundle)
        .arg("--tier")
        .arg("debug")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("apply at build time"));
}

#[test]
fn repl_check_skips_an_ill_typed_entry_and_keeps_the_session_usable() {
    // Under --check (session-checker C2): entry 2 retypes a mut binding → E0007 printed, entry
    // SKIPPED (x keeps its value); entry 3 still runs against the intact session.
    lang()
        .arg("repl")
        .write_stdin("mut x = 5\nx = \"s\"\necho x + 1;\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0007"))
        .stdout(predicate::str::contains("6"));
}

#[test]
fn repl_check_applies_static_rules_the_unchecked_repl_defers() {
    // A required-signature violation (E0022) is static-only: the unchecked REPL would run it.
    lang()
        .arg("repl")
        .write_stdin("fn f(n) { return n }\necho 1 + 1;\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0022"))
        .stdout(predicate::str::contains("2"));
}

#[test]
fn repl_check_toggles_at_the_prompt() {
    // ON by default: the retype is rejected (E0007). After `:check off`, the same shape runs
    // (checkerless semantics — the pre-C2 behavior).
    lang()
        .arg("repl")
        .write_stdin("mut a = 1\na = \"s\"\n:check off\nmut b = 2\nb = \"t\"\necho b ~ \"!\";\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E0007"))
        .stdout(predicate::str::contains("t!"));
}

#[test]
fn repl_no_check_flag_restores_checkerless_sessions() {
    lang()
        .arg("repl")
        .arg("--no-check")
        .write_stdin("mut a = 1\na = \"s\"\necho a ~ \"!\";\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("s!"));
}

#[test]
fn repl_checked_codegen_gives_type_of_full_fidelity() {
    // C5: a fully-checked session compiles entries with the checker's site bundle — `type_of` on a
    // list literal recovers the element type, exactly as `noeta run` does...
    lang()
        .arg("repl")
        .write_stdin("echo type_of([1, 2]);\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Type.List(Type.Int)"));
    // ...while a checkerless session stays on the conservative head-only codegen.
    lang()
        .arg("repl")
        .arg("--no-check")
        .write_stdin("echo type_of([1, 2]);\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Type.List(Type.Dyn)"));
}

#[test]
fn repl_load_bootstraps_a_session_with_the_programs_context() {
    // The "tinker" shape: the bootstrap runs to completion (its output first), then the prompt
    // opens with its functions, types, and bindings live — and entries check against them.
    let path = temp_program(
        "repl_load",
        "fn twice(n: int): int { return n * 2 }\nmut base = 21\necho \"booted\"\n",
    );
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&path)
        .write_stdin("echo twice(base);\nbase = 5\ntwice(base)\n")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("booted")
                .and(predicate::str::contains("42"))
                .and(predicate::str::contains("10")),
        );
}

#[test]
fn repl_load_entries_type_check_against_the_bootstrap() {
    // A wrong-arity call against a BOOTSTRAP function is a static error at the prompt.
    let path = temp_program(
        "repl_load_check",
        "fn twice(n: int): int { return n * 2 }\n",
    );
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&path)
        .write_stdin("twice(1, 2)\necho twice(3);\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("E00"))
        .stdout(predicate::str::contains("6"));
}

#[test]
fn repl_load_fails_fast_on_a_broken_bootstrap() {
    // A bootstrap that does not type-check exits with its diagnostics — no broken prompt.
    let bad_check = temp_program("repl_load_bad_check", "mut x: int = \"s\"\n");
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&bad_check)
        .write_stdin("echo 1;\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0007"));
    // A bootstrap that panics at run time exits with the abort — same rule.
    let panics = temp_program("repl_load_panics", "panic(\"boom\")\n");
    lang()
        .arg("repl")
        .arg("--load")
        .arg(&panics)
        .write_stdin("echo 1;\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("boom"));
}
