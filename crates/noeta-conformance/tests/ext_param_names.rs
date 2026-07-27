//! **Native parameter-name invariant.** Every `ExtFn` in every registered module declares either no
//! parameter names at all, or exactly one name per parameter.
//!
//! `ExtFn::param_names` is what a `name:` label at a call site binds against, so it is public API:
//! a named signature lets callers write `math.pow(base: 2.0, exp: 3.0)` and reorder the arguments,
//! and the checker binds them through the same path a declared Noeta function uses. An empty list
//! means "this function takes no named arguments" and a label on it is refused rather than ignored.
//!
//! A **partially** named list is the one state that is neither: binding would zip a short name list
//! against a longer parameter list, so a label could name a parameter that positionally is not the
//! one it lands on — silently, which is the whole failure mode named arguments exist to remove.
//! Nothing in the type system prevents writing one, so this test does.
//!
//! It also pins the names themselves to a shape callers can rely on: a label is a compatibility
//! surface, and `arg0` or a stray capital would be a poor one to be stuck with.

use noeta_stdlib::registry::ExtFn;

/// Every `ExtFn` reachable from the default registry, tagged with where it came from so a failure
/// names the table to fix.
fn all_fns() -> Vec<(String, &'static ExtFn)> {
    // The stdlib's own extension units, read directly — no installed process registry needed, so
    // this test says nothing about install order and cannot be perturbed by one.
    let mut out = Vec::new();
    for unit in noeta_stdlib::registry::std_units() {
        for module in unit.modules() {
            for (table, fns) in [
                ("functions", module.functions),
                ("ctx_functions", module.ctx_functions),
                ("typed_functions", module.typed_functions),
            ] {
                for f in fns {
                    out.push((format!("{}.{} ({table})", module.name, f.name), f));
                }
            }
        }
    }
    out
}

#[test]
fn param_names_are_absent_or_complete() {
    let mut bad = Vec::new();
    for (where_, f) in all_fns() {
        if f.param_names.is_empty() {
            continue; // opts out of named arguments — the honest default
        }
        if f.param_names.len() != f.params.len() {
            bad.push(format!(
                "{where_}: {} name(s) for {} parameter(s) — name every parameter or none",
                f.param_names.len(),
                f.params.len()
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "partially-named signatures:\n{}",
        bad.join("\n")
    );
}

#[test]
fn param_names_are_usable_as_labels() {
    // A label is written at the call site, so a name has to be a plain lower-case identifier: a
    // caller cannot write `f(Arg 0: 1)`. Duplicates are worse than useless — two parameters with
    // one name make the second unreachable by label.
    let mut bad = Vec::new();
    for (where_, f) in all_fns() {
        for n in f.param_names {
            let usable = !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !n.starts_with(|c: char| c.is_ascii_digit());
            if !usable {
                bad.push(format!("{where_}: `{n}` is not a usable label"));
            }
        }
        let mut seen: Vec<&str> = f.param_names.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        if seen.len() != before {
            bad.push(format!("{where_}: repeats a parameter name"));
        }
    }
    assert!(
        bad.is_empty(),
        "unusable parameter names:\n{}",
        bad.join("\n")
    );
}
