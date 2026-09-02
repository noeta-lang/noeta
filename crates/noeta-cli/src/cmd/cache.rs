//! `noeta cache` — inspect and clean the per-user cache (`~/.cache/noeta/`).
//!
//! The cache root holds three kinds of derived state: **cached compilations** (top-level `*.noeb`
//! blobs — the transparent startup cache), **composed toolchains** (`compose/<key>/` — a full
//! cargo build per native-dependency set), and **fetched package sources** (`pkg/<key>/`). All of it
//! is re-derivable — deleting
//! any of it costs at most a recompile, a recompose, or a refetch — so every verb here is safe by
//! construction. The one invariant is that nothing outside the resolved cache root is ever
//! touched: every path is built by joining the root, and only the three known categories are
//! removed (a `bench-baselines/` or `watch/` sibling is never candidate state).
//!
//! `clean` (without `--all`) targets the big one: compose entries are keyed on (among other
//! things) the running binary's build identity, so every toolchain rebuild strands the previous
//! build's compositions. Each use stamps the entry with the composing binary's identity
//! ([`crate::compose::COMPOSE_IDENTITY_FILE`]); `clean` removes the entries whose stamp is not
//! this binary's and reports the bytes reclaimed.
//!
//! A *current* entry is small — tens of MiB, the composed binary itself. It used to be 1–2 GiB,
//! because each entry kept the cargo target dir it was built in: build scratch worth ~27× the
//! artifact, retained for the entry's whole life and reclaimable only by going stale. `compose`
//! now drops that scratch once it has copied the artifact out (`compose::discard_build_scratch`),
//! so what `ls` reports here is the artifact rather than the scaffolding around it.

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use crate::CacheAction;
use crate::compose::COMPOSE_IDENTITY_FILE;
use crate::output::{human_bytes, plural};

/// `noeta cache [<ls|path|info|clean|clear>]` — inspect or clean the user cache. Without a
/// subcommand, `ls`.
pub(crate) fn cmd_cache(action: Option<&CacheAction>) -> ExitCode {
    let Some(root) = noeta_cache::Cache::locate() else {
        eprintln!("noeta: no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)");
        return ExitCode::from(1);
    };
    match action.unwrap_or(&CacheAction::Ls) {
        CacheAction::Ls => cmd_ls(&root),
        CacheAction::Path => {
            println!("{}", root.display());
            ExitCode::SUCCESS
        }
        CacheAction::Info => cmd_info(&root),
        CacheAction::Clean { all } => cmd_clean(&root, *all),
        CacheAction::Clear => cmd_clear(&root),
    }
}

/// `noeta cache` / `noeta cache ls`: the per-category summary, with each compose entry listed
/// individually (they are the multi-GiB ones — worth seeing one by one).
fn cmd_ls(root: &Path) -> ExitCode {
    println!("{}", root.display());
    if !root.exists() {
        println!("(empty — nothing cached yet)");
        return ExitCode::SUCCESS;
    }
    let summary = match scan(root) {
        Ok(summary) => summary,
        Err(err) => {
            eprintln!("noeta: cannot read cache at {}: {err}", root.display());
            return ExitCode::from(1);
        }
    };
    let compose_total: u64 = summary.compose.iter().map(|e| e.bytes).sum();
    let row = |name: &str, count: usize, bytes: u64, what: &str| {
        println!(
            "  {name:<9} {count:>5} {:<7} {:>10}   {what}",
            format!("entr{}", if count == 1 { "y" } else { "ies" }),
            human_bytes(bytes),
        );
    };
    row(
        "bytecode",
        summary.bytecode.count,
        summary.bytecode.bytes,
        "cached compilations (*.noeb)",
    );
    row(
        "compose",
        summary.compose.len(),
        compose_total,
        "composed toolchains",
    );
    row(
        "pkg",
        summary.pkg.count,
        summary.pkg.bytes,
        "fetched package sources",
    );
    println!(
        "  {:<9} {:>5} {:<7} {:>10}",
        "total",
        "",
        "",
        human_bytes(summary.bytecode.bytes + compose_total + summary.pkg.bytes)
    );
    if !summary.compose.is_empty() {
        println!("\ncompose entries (most recently used first):");
        for entry in &summary.compose {
            println!(
                "  {:<17} {:>10}   last used {}",
                short_key(&entry.key),
                human_bytes(entry.bytes),
                entry
                    .last_used
                    .map_or_else(|| "unknown".to_string(), human_age),
            );
        }
        println!("(`noeta cache clean` removes the entries stale toolchain builds left behind)");
    }
    ExitCode::SUCCESS
}

/// `noeta cache info`: the startup-cache (`*.noeb`) location, entry count, size, and cap.
fn cmd_info(dir: &Path) -> ExitCode {
    let cap = noeta_cache::max_bytes();
    let cap_str = if cap == 0 {
        "unbounded".to_string()
    } else {
        human_bytes(cap)
    };
    if !dir.exists() {
        println!("{}\n0 entries, 0 B on disk (cap {cap_str})", dir.display());
        return ExitCode::SUCCESS;
    }
    match noeta_cache::Cache::open_at(dir.to_path_buf()).and_then(|c| c.stats()) {
        Ok((count, bytes)) => {
            println!("{}", dir.display());
            println!(
                "{count} {}, {} on disk (cap {cap_str})",
                if count == 1 { "entry" } else { "entries" },
                human_bytes(bytes),
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: cannot read cache at {}: {err}", dir.display());
            ExitCode::from(1)
        }
    }
}

/// `noeta cache clear`: remove all cached compilations (the `*.noeb` entries only).
fn cmd_clear(dir: &Path) -> ExitCode {
    if !dir.exists() {
        println!("cache is already empty ({})", dir.display());
        return ExitCode::SUCCESS;
    }
    match noeta_cache::Cache::open_at(dir.to_path_buf()).and_then(|c| c.clear()) {
        Ok(n) => {
            println!(
                "removed {n} cached compilation{} from {}",
                plural(n),
                dir.display()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: cannot clear cache at {}: {err}", dir.display());
            ExitCode::from(1)
        }
    }
}

/// `noeta cache clean [--all]`: drop stale composed toolchains (default), or the whole cache.
fn cmd_clean(root: &Path, all: bool) -> ExitCode {
    if !root.exists() {
        println!("cache is already empty ({})", root.display());
        return ExitCode::SUCCESS;
    }
    if all {
        let report = clean_all(root);
        let line = |name: &str, cat: &Category| {
            println!(
                "{name}: removed {} entr{} ({})",
                cat.count,
                if cat.count == 1 { "y" } else { "ies" },
                human_bytes(cat.bytes)
            );
        };
        line("bytecode", &report.bytecode);
        line("compose", &report.compose);
        line("pkg", &report.pkg);
        println!(
            "reclaimed {} from {}",
            human_bytes(report.bytecode.bytes + report.compose.bytes + report.pkg.bytes),
            root.display()
        );
        return ExitCode::SUCCESS;
    }
    // Which compose entries are current is decided by this binary's own build identity — the same
    // value the compose key folds in, stamped beside each entry on use.
    let Some(identity) = noeta_cache::binary_identity() else {
        eprintln!(
            "noeta: cannot determine this binary's build identity, so stale composed toolchains \
             can't be told from current ones (use `noeta cache clean --all` to wipe everything)"
        );
        return ExitCode::from(1);
    };
    match clean_stale_compose(root, &identity) {
        Ok(report) if report.removed == 0 => {
            println!(
                "nothing stale to clean — {} composed toolchain{} cached, all this binary's",
                report.kept,
                plural(report.kept)
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            println!(
                "removed {} stale composed toolchain{} ({} reclaimed); kept {} current",
                report.removed,
                plural(report.removed),
                human_bytes(report.reclaimed),
                report.kept
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!(
                "noeta: cannot clean compose cache at {}: {err}",
                root.join("compose").display()
            );
            ExitCode::from(1)
        }
    }
}

/// What `ls` reports: the flat `*.noeb` category, each compose entry, and the pkg store.
struct Summary {
    bytecode: Category,
    compose: Vec<ComposeEntry>,
    pkg: Category,
}

/// One category's entry count and total on-disk bytes.
#[derive(Default, Debug, PartialEq, Eq)]
struct Category {
    count: usize,
    bytes: u64,
}

/// One `compose/<key>/` entry: its size, last-used time, and the recorded binary identity that
/// decides staleness (see [`COMPOSE_IDENTITY_FILE`]).
struct ComposeEntry {
    key: String,
    bytes: u64,
    last_used: Option<SystemTime>,
    identity: Option<String>,
}

/// Inventory the cache root. Only the three known categories are read; anything else under the
/// root (bench baselines, watch state, …) is ignored.
fn scan(root: &Path) -> io::Result<Summary> {
    let mut bytecode = Category::default();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "noeb") && entry.file_type()?.is_file() {
            bytecode.count += 1;
            bytecode.bytes += entry.metadata()?.len();
        }
    }
    let mut compose = compose_entries(root)?;
    compose.sort_by_key(|e| std::cmp::Reverse(e.last_used));
    Ok(Summary {
        bytecode,
        compose,
        pkg: dir_category(&root.join("pkg"))?,
    })
}

/// Count + size the immediate children of a category directory (each child is one entry — a
/// package tree). A missing directory is an empty category, not an error.
fn dir_category(dir: &Path) -> io::Result<Category> {
    let mut cat = Category::default();
    if !dir.is_dir() {
        return Ok(cat);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        cat.count += 1;
        cat.bytes += if entry.file_type()?.is_dir() {
            dir_size(&entry.path())
        } else {
            entry.metadata()?.len()
        };
    }
    Ok(cat)
}

/// The `compose/<key>/` entries, unsorted. The identity marker's mtime is the last-used time (it
/// is rewritten on every use); a pre-marker entry falls back to the directory's own mtime.
fn compose_entries(root: &Path) -> io::Result<Vec<ComposeEntry>> {
    let dir = root.join("compose");
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue; // a stray file is not a compose entry — never remove what we don't own
        }
        let path = entry.path();
        let marker = path.join(COMPOSE_IDENTITY_FILE);
        let identity = fs::read_to_string(&marker)
            .ok()
            .map(|s| s.trim().to_string());
        let last_used = fs::metadata(&marker)
            .or_else(|_| fs::metadata(&path))
            .and_then(|m| m.modified())
            .ok();
        out.push(ComposeEntry {
            key: entry.file_name().to_string_lossy().into_owned(),
            bytes: dir_size(&path),
            last_used,
            identity,
        });
    }
    Ok(out)
}

/// What a `clean` run removed and what it deliberately kept.
#[derive(Default, Debug, PartialEq, Eq)]
struct CleanReport {
    removed: usize,
    reclaimed: u64,
    kept: usize,
}

/// Remove every compose entry whose recorded identity is not `current_identity` — the entries
/// stranded by other toolchain builds, which the (identity-keyed) compose cache can never hit
/// again. An entry with no marker was last used by a pre-marker build: equally stale. Removal is
/// best-effort per entry (a busy entry is skipped, not fatal).
fn clean_stale_compose(root: &Path, current_identity: &str) -> io::Result<CleanReport> {
    let mut report = CleanReport::default();
    for entry in compose_entries(root)? {
        if entry.identity.as_deref() == Some(current_identity) {
            report.kept += 1;
            continue;
        }
        if fs::remove_dir_all(root.join("compose").join(&entry.key)).is_ok() {
            report.removed += 1;
            report.reclaimed += entry.bytes;
        }
    }
    Ok(report)
}

/// What `clean --all` removed, per category.
#[derive(Default)]
struct AllReport {
    bytecode: Category,
    compose: Category,
    pkg: Category,
}

/// Wipe all three cache categories: every top-level `*.noeb`, the whole `compose/` store, and the
/// whole `pkg/` store. Nothing else under the root is touched. Best-effort throughout — a file
/// that fails to remove is skipped and simply not counted.
fn clean_all(root: &Path) -> AllReport {
    let mut report = AllReport::default();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "noeb")
                && entry.file_type().is_ok_and(|t| t.is_file())
            {
                let bytes = entry.metadata().map_or(0, |m| m.len());
                if fs::remove_file(&path).is_ok() {
                    report.bytecode.count += 1;
                    report.bytecode.bytes += bytes;
                }
            }
        }
    }
    report.compose = remove_category_dir(&root.join("compose"));
    report.pkg = remove_category_dir(&root.join("pkg"));
    report
}

/// Remove every entry under a category directory (then the — now empty — directory itself; it is
/// recreated on demand). Returns what was removed.
fn remove_category_dir(dir: &Path) -> Category {
    let mut cat = Category::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return cat;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let bytes = if is_dir {
            dir_size(&path)
        } else {
            entry.metadata().map_or(0, |m| m.len())
        };
        let removed = if is_dir {
            fs::remove_dir_all(&path).is_ok()
        } else {
            fs::remove_file(&path).is_ok()
        };
        if removed {
            cat.count += 1;
            cat.bytes += bytes;
        }
    }
    let _ = fs::remove_dir(dir);
    cat
}

/// Best-effort recursive on-disk size. Symlinks are counted as themselves, never followed — the
/// walk (like removal) can't escape the cache root through a link.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_size(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// A compose key abbreviated for the listing (the full 64-hex key is line noise; a prefix is
/// plenty to find the directory).
fn short_key(key: &str) -> String {
    let mut short: String = key.chars().take(16).collect();
    if short.len() < key.len() {
        short.push('…');
    }
    short
}

/// A coarse relative time ("3 days ago") for the compose listing.
fn human_age(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (n, unit) = if secs < 60 {
        return "just now".to_string();
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else {
        (secs / 86_400, "day")
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = noeta_test_temp::unique_path(&format!("cache-cmd-{tag}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Seed one compose entry: a `bin/` payload of `payload` bytes, plus an identity marker
    /// (`None` = a pre-marker entry).
    fn seed_compose(root: &Path, key: &str, identity: Option<&str>, payload: usize) {
        let dir = root.join("compose").join(key);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin").join("noeta-composed"), vec![b'x'; payload]).unwrap();
        if let Some(id) = identity {
            fs::write(dir.join(COMPOSE_IDENTITY_FILE), id).unwrap();
        }
    }

    fn seed_pkg(root: &Path, key: &str, payload: usize) {
        let dir = root.join("pkg").join(key);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("main.noe"), vec![b'p'; payload]).unwrap();
    }

    fn seed_noeb(root: &Path, name: &str, payload: usize) {
        fs::write(root.join(format!("{name}.noeb")), vec![b'b'; payload]).unwrap();
    }

    #[test]
    fn cache_scan_reports_all_three_categories() {
        let root = temp_root("scan");
        seed_noeb(&root, "aaaa", 100);
        seed_noeb(&root, "bbbb", 50);
        seed_compose(&root, "k1", Some("id-1"), 1000);
        seed_pkg(&root, "p1", 30);
        seed_pkg(&root, "p2", 20);
        // Unrelated state must not be counted as any category.
        fs::create_dir_all(root.join("bench-baselines")).unwrap();
        fs::write(root.join("bench-baselines").join("x.json"), b"{}").unwrap();

        let summary = scan(&root).unwrap();
        assert_eq!(
            summary.bytecode,
            Category {
                count: 2,
                bytes: 150
            }
        );
        assert_eq!(summary.compose.len(), 1);
        // The entry's size covers the payload plus the identity marker's own bytes.
        assert_eq!(summary.compose[0].bytes, 1000 + "id-1".len() as u64);
        assert_eq!(summary.compose[0].identity.as_deref(), Some("id-1"));
        assert!(summary.compose[0].last_used.is_some());
        assert_eq!(
            summary.pkg,
            Category {
                count: 2,
                bytes: 50
            }
        );
    }

    #[test]
    fn cache_scan_of_a_bare_root_is_empty() {
        let root = temp_root("bare");
        let summary = scan(&root).unwrap();
        assert_eq!(summary.bytecode, Category::default());
        assert!(summary.compose.is_empty());
        assert_eq!(summary.pkg, Category::default());
    }

    #[test]
    fn cache_clean_removes_stale_compose_entries_and_keeps_current() {
        let root = temp_root("clean");
        seed_compose(&root, "current", Some("this-binary"), 10);
        seed_compose(&root, "stale", Some("older-binary"), 200);
        seed_compose(&root, "premarker", None, 300); // no marker ⇒ a pre-marker build ⇒ stale

        let report = clean_stale_compose(&root, "this-binary").unwrap();
        assert_eq!(report.removed, 2);
        assert_eq!(report.kept, 1);
        assert_eq!(
            report.reclaimed,
            200 + "older-binary".len() as u64 + 300,
            "reclaimed bytes cover exactly the removed entries"
        );
        assert!(root.join("compose").join("current").is_dir());
        assert!(!root.join("compose").join("stale").exists());
        assert!(!root.join("compose").join("premarker").exists());
    }

    #[test]
    fn cache_clean_touches_nothing_outside_the_compose_store() {
        let root = temp_root("clean-scope");
        seed_compose(&root, "stale", Some("older-binary"), 10);
        seed_noeb(&root, "cccc", 40);
        seed_pkg(&root, "p1", 30);
        fs::create_dir_all(root.join("watch")).unwrap();
        fs::write(root.join("watch").join("acme.toml"), b"base = \"x\"").unwrap();

        clean_stale_compose(&root, "this-binary").unwrap();
        assert!(root.join("cccc.noeb").is_file());
        assert!(root.join("pkg").join("p1").is_dir());
        assert!(root.join("watch").join("acme.toml").is_file());
    }

    #[test]
    fn cache_clean_all_wipes_the_three_categories_and_nothing_else() {
        let root = temp_root("clean-all");
        seed_noeb(&root, "dddd", 100);
        seed_compose(&root, "k1", Some("id-1"), 1000);
        seed_compose(&root, "k2", None, 500);
        seed_pkg(&root, "p1", 30);
        fs::create_dir_all(root.join("bench-baselines")).unwrap();
        fs::write(root.join("bench-baselines").join("x.json"), b"{}").unwrap();

        let report = clean_all(&root);
        assert_eq!(
            report.bytecode,
            Category {
                count: 1,
                bytes: 100
            }
        );
        assert_eq!(report.compose.count, 2);
        assert_eq!(report.compose.bytes, 1000 + "id-1".len() as u64 + 500);
        assert_eq!(
            report.pkg,
            Category {
                count: 1,
                bytes: 30
            }
        );
        assert!(!root.join("compose").exists());
        assert!(!root.join("pkg").exists());
        assert!(!root.join("dddd.noeb").exists());
        // Unrelated state survives — `--all` means the three cache categories, not the root.
        assert!(root.join("bench-baselines").join("x.json").is_file());
    }

    #[test]
    fn cache_compose_listing_ignores_stray_files() {
        let root = temp_root("stray");
        fs::create_dir_all(root.join("compose")).unwrap();
        fs::write(root.join("compose").join("README"), b"not an entry").unwrap();
        seed_compose(&root, "k1", Some("id"), 10);

        let entries = compose_entries(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "k1");
        // …and clean leaves the stray file alone.
        clean_stale_compose(&root, "other").unwrap();
        assert!(root.join("compose").join("README").is_file());
    }

    #[test]
    fn cache_short_key_abbreviates_only_long_keys() {
        assert_eq!(short_key("abcd"), "abcd");
        assert_eq!(short_key("0123456789abcdef0123"), "0123456789abcdef…");
    }

    #[test]
    fn cache_human_age_is_coarse_and_pluralized() {
        let now = SystemTime::now();
        assert_eq!(human_age(now), "just now");
        let ago = |secs: u64| now - std::time::Duration::from_secs(secs);
        assert_eq!(human_age(ago(90)), "1 minute ago");
        assert_eq!(human_age(ago(7200)), "2 hours ago");
        assert_eq!(human_age(ago(3 * 86_400)), "3 days ago");
    }
}
