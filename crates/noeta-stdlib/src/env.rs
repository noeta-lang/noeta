//! The `env`/`args` Ring 2 host-introspection surface (M2.2). Imported with
//! `use std.{env}` / `use std.{args}` and called `env.get("HOME")`, `env.keys()`,
//! `args.all()`.
//!
//! ## Determinism: a fixed sandbox fixture, not the real environment
//!
//! Reading the real process environment is non-deterministic and host-coupled, so
//! — exactly like the logical clock starting at 0 and the PRNG's `DEFAULT_SEED` —
//! the sandbox presents a small, **fixed** environment and argument vector. Both
//! backends construct the identical fixture, so `env`/`args` programs are
//! reproducible and stay inside the differential by construction. The *real* host
//! environment is read only by a real host (later M2 slices), constructed by the
//! CLI/REPL/server and never exercised in the differential.
//!
//! `env.keys()` is sorted (the backing store is a `BTreeMap`), so iteration is
//! deterministic, mirroring `fs.list()`.

use crate::{ErrorKind, StdError};
use std::collections::BTreeMap;

/// The deterministic environment the sandbox presents. A small fixed fixture so
/// the success path of `env.get`/`env.keys` is testable and identical across
/// backends.
pub fn sandbox_vars() -> BTreeMap<String, String> {
    [("HOME", "/home/sandbox"), ("USER", "noeta")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The deterministic argument vector the sandbox presents (program name + a
/// representative argument).
pub fn sandbox_args() -> Vec<String> {
    vec!["noeta".to_string(), "run".to_string()]
}

/// The canonical "no such environment variable" error for `env.get` (→ `E0021`),
/// mirroring `fs`'s missing-file error: reading absent host state is an IO failure.
pub fn not_found_error(key: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("no such environment variable: `{key}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_vars_are_sorted_and_fixed() {
        let vars = sandbox_vars();
        let keys: Vec<&String> = vars.keys().collect();
        assert_eq!(keys, vec!["HOME", "USER"]);
        assert_eq!(vars.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn missing_var_is_an_io_error() {
        assert_eq!(not_found_error("NOPE").kind, ErrorKind::Io);
    }
}
