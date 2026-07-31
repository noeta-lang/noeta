//! **What this process has actually linked** — the composition a running `noeta` carries, and the
//! native packages a given project needs that it does not.
//!
//! A package's native extension is *statically linked Rust*: the only way a process gains it is to
//! **be** the composed toolchain (`noeta-cli`'s `compose` module builds a shim and `exec`s it). A
//! one-shot verb can therefore simply delegate and forget. A **server** cannot: `noeta lsp` and
//! `noeta mcp` are launched once and then answer about whatever file the editor or the agent names,
//! which need not be the project the process composed for — or any project at all.
//!
//! So the composition has to be *inspectable from inside the process*, not merely performed. The
//! delegating binary stamps the identities it composed into [`COMPOSED_ENV`] (the same variable that
//! already guards against re-composition), and every surface that turns a file into a program asks
//! [`uncomposed`] whether the answer it is about to give is trustworthy. When it is not, the surface
//! says so instead of reporting the unresolved-import wreckage that a missing extension causes —
//! silence and confident wrongness are the two failure modes a tool an agent is told to trust cannot
//! have.

use crate::graph::NativeCrate;

/// The environment variable a delegating `noeta` sets on the composed toolchain it `exec`s. Its
/// presence is the re-composition guard; its **value** is the composition manifest — the sorted,
/// comma-separated identities of the runtime native packages that binary links.
pub const COMPOSED_ENV: &str = "NOETA_COMPOSED";

/// What the current process is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composition {
    /// The stock binary: no package's native extension is linked.
    Stock,
    /// A composed toolchain that named the packages it carries.
    Packages(Vec<String>),
    /// Marked composed, but by something that did not name its packages — a hand-set
    /// `NOETA_COMPOSED=1`, or a binary composed by an older toolchain. Fails **open**: we assume it
    /// carries whatever it is asked for, because the alternative is warning a user who is already
    /// running the right binary.
    Opaque,
}

impl Composition {
    /// Whether this composition can link `identity`'s native extension.
    fn carries(&self, identity: &str) -> bool {
        match self {
            Composition::Stock => false,
            Composition::Opaque => true,
            Composition::Packages(list) => list.iter().any(|p| p == identity),
        }
    }
}

/// The composition of the running process, read from the environment.
pub fn current() -> Composition {
    from_env(std::env::var_os(COMPOSED_ENV).as_deref().map(|s| {
        s.to_str()
            .map(str::to_string)
            .unwrap_or_else(|| s.to_string_lossy().into_owned())
    }))
}

/// [`current`]'s pure half, so the parsing is testable without touching process state.
fn from_env(value: Option<String>) -> Composition {
    match value {
        None => Composition::Stock,
        Some(raw) => {
            let trimmed = raw.trim();
            // The legacy/manual guard value, and the empty string a composition with no packages
            // would write (which cannot happen — a composition exists only because there were
            // packages) both mean "composed, contents unstated".
            if trimmed.is_empty() || trimmed == "1" {
                return Composition::Opaque;
            }
            Composition::Packages(
                trimmed
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
    }
}

/// The value a delegating binary stamps into [`COMPOSED_ENV`] for `identities`.
pub fn env_value(identities: &[String]) -> String {
    let mut sorted: Vec<&str> = identities.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        // Never write the empty string — it would read back as `Opaque` rather than `Stock`, and a
        // composition is only ever performed because there was something to compose.
        return "1".to_string();
    }
    sorted.join(",")
}

/// The native packages a resolved graph needs whose extensions this process does **not** carry —
/// sorted, deduplicated, and empty whenever the answers derived from that graph are trustworthy.
///
/// Only **runtime** native crates count, exactly as
/// [`ResolvedGraph::runtime_native_crates`](crate::graph::ResolvedGraph::runtime_native_crates)
/// decides for the CLI: a `dev-native` formatter contributes no modules, types or directives to a
/// program, so its absence cannot make an import fail to resolve.
///
/// The crates come from a graph the caller already resolved for its own reasons (the editor resolves
/// it to find dependency modules; the MCP surface resolves it to build a workspace), so this check
/// costs a vector scan, not a resolve.
pub fn uncomposed(native_crates: &[NativeCrate]) -> Vec<String> {
    uncomposed_in(&current(), native_crates)
}

/// [`uncomposed`] against an explicit composition — the testable form.
pub fn uncomposed_in(composition: &Composition, native_crates: &[NativeCrate]) -> Vec<String> {
    let mut missing: Vec<String> = native_crates
        .iter()
        .filter(|nc| !nc.dev_only)
        .map(|nc| nc.identity.clone())
        .filter(|identity| !composition.carries(identity))
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// The one sentence a surface shows when [`uncomposed`] is non-empty: what is missing, why the
/// answer would otherwise be wrong, and the exact command that fixes it.
///
/// Naming the fix is the whole point. The composition is a **cache** — one `noeta check` in the
/// project builds it, and every later server start finds it and delegates in milliseconds — so the
/// user or agent is one command away from a correct tool, and telling them that is worth more than
/// any number of diagnostics.
pub fn explain(missing: &[String], project: &std::path::Path) -> String {
    let names = missing
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let plural = if missing.len() == 1 { "s" } else { "" };
    format!(
        "{names} ship{plural} a native extension that this `noeta` has not composed, so the modules, \
         types and directives {it} provide{plural} do not exist here and nothing that imports them \
         can resolve. Diagnostics for this file are withheld rather than reported wrongly. Build the \
         composed toolchain once with `noeta check {}` (it is cached afterwards), then restart the \
         server.",
        project.display(),
        it = if missing.len() == 1 { "it" } else { "they" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with(native: &[(&str, bool)]) -> Vec<NativeCrate> {
        native
            .iter()
            .map(|(identity, dev_only)| NativeCrate {
                identity: (*identity).to_string(),
                crate_dir: std::path::PathBuf::from("/nonexistent"),
                content_hash: String::new(),
                dev_only: *dev_only,
            })
            .collect()
    }

    #[test]
    fn a_stock_binary_is_missing_every_runtime_native_package() {
        let graph = graph_with(&[("para/api", false), ("para/db", false)]);
        assert_eq!(
            uncomposed_in(&Composition::Stock, &graph),
            vec!["para/api".to_string(), "para/db".to_string()]
        );
    }

    #[test]
    fn a_dev_only_crate_is_never_missing() {
        // A `dev-native` formatter contributes nothing a program can import, so its absence must
        // not make a surface withhold diagnostics — the same rule `runtime_native_crates` applies
        // to whether the CLI composes at all.
        let graph = graph_with(&[("para/html", true)]);
        assert!(uncomposed_in(&Composition::Stock, &graph).is_empty());
    }

    #[test]
    fn a_composition_covers_only_what_it_names() {
        let composed = Composition::Packages(vec!["para/api".to_string()]);
        let graph = graph_with(&[("para/api", false), ("para/db", false)]);
        assert_eq!(
            uncomposed_in(&composed, &graph),
            vec!["para/db".to_string()]
        );
    }

    #[test]
    fn an_unnamed_composition_fails_open() {
        let graph = graph_with(&[("para/api", false)]);
        assert!(uncomposed_in(&Composition::Opaque, &graph).is_empty());
    }

    #[test]
    fn the_stamp_round_trips() {
        let identities = vec!["para/db".to_string(), "para/api".to_string()];
        assert_eq!(env_value(&identities), "para/api,para/db");
        assert_eq!(
            from_env(Some(env_value(&identities))),
            Composition::Packages(vec!["para/api".to_string(), "para/db".to_string()])
        );
        assert_eq!(from_env(None), Composition::Stock);
        assert_eq!(from_env(Some("1".to_string())), Composition::Opaque);
    }
}
