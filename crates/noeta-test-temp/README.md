# noeta-test-temp

The machine-shared resources a test needs, built in one place: hermetic per-process fixture directories, free loopback ports, and the wait for a spawned server to come up.

- **Takes in:** nothing (a standalone helper; depends on no other crate).
- **Emits:** `TempDir` (a fixture directory guard), `TempPath` (a path inside one, carrying the guard), `unique_path` (a guardless unique path, for fixtures handed to a type that takes ownership of the directory), `free_port`, and the readiness waits `wait_until_listening` / `wait_until_listening_or_child_exits` / `wait_until_closed` / `settle_closed` (budget: `readiness_budget`, knob: `NOETA_TEST_READY_SECS`).

## Why it exists

A fixture path built from a fixed name under the system temp dir — `/tmp/noeta_lsp_crosspkg`, `/tmp/noeta_prof_test_hot` — is shared by every checkout and every concurrently-running test binary on the machine, and tests of this shape open by `remove_dir_all`ing that path. Two test processes racing one name delete each other's tree mid-setup, and the failures that follow name anything but the cause: `git: fatal: cannot copy … No such file or directory`, a `noeta.toml` that vanished between write and read, a module directory missing half its siblings. Because the sharing is between *processes*, a test mutex or `--test-threads=1` does not fix it.

This bug class was fixed three separate times before this crate existed — the CLI's integration fixtures (which had accumulated 169 stray `/tmp/noeta_cli_test_*` directories), then `noeta-pm`'s unit tests (24 tests vulnerable; 8–14 failed per concurrent run), then the same shape surviving in six more crates — and each fix rolled its own helper, which is why there was a next time. One implementation, reachable by `dev-dependency`, is the fix for the recurrence rather than for the instance.

## How

`CARGO_TARGET_TMPDIR` — cargo's own answer — is set only for integration tests and benches, never for the unit tests inside `src/` where most of these fixtures live. So the root is derived at runtime from the test binary's own path (`<target-dir>/tmp/noeta-tests/`), which tracks `CARGO_TARGET_DIR` the same way, keeps fixtures off the small `/tmp` tmpfs, and puts them where `cargo clean` already looks. Under it each *process* gets `p<pid>.<binary>/`, and a counter keeps repeated calls within a process distinct. Roots belonging to pids that no longer exist are pruned on first use, so a run killed mid-flight leaves nothing behind.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
