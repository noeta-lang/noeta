//! Bakes the example corpus (`tests/conformance/**/*.noe`) into **one blob plus an integer
//! index**, instead of one `&'static str` per file.
//!
//! WHY, precisely. The corpus was embedded with `include_dir!`, which generates a `Dir`/`File`
//! tree: every file contributes a `&'static str` path, a `&'static [u8]` body and a `DirEntry`
//! wrapper, and every directory contributes a slice of entries. In a position-independent
//! executable each of those pointers is an `R_X86_64_RELATIVE` relocation that `ld.so` rewrites
//! at **every process start**, before `main` runs. Measured on the 0.4.0 tree by mapping the
//! binary's relocations back to their defining object files, `noeta_mcp` owned **4,709 of 52,869**
//! relative relocations (8.9%) — the largest in-tree owner, and second overall only to
//! `regex_syntax`'s Unicode tables. 1,233 `.noe` files at ~3-4 pointers each is exactly that
//! number.
//!
//! A flat blob has no pointers in it. The index is `[u32; 4]` rows — path offset/len, source
//! offset/len — which is plain integer data in `.rodata`, so the whole corpus costs **two**
//! relocations (the blob slice and the index slice) regardless of how many files it holds. The
//! cost stops scaling with the corpus, which is the point: the corpus grows with every conformance
//! case, so `include_dir!` made every new test case a little more startup work for every `noeta`
//! invocation, including `noeta run` on a one-line program that never touches MCP.
//!
//! It also embeds *less*: `include_dir!` took the whole tree, including the 45 `.toml` fixtures
//! `collect_examples` has always filtered back out.
//!
//! The parallel `include_dir!` in `noeta-ide` (`docs/*.md`, the language guide) is deliberately
//! left alone — 48 files is ~145 relocations, and a second copy of this machinery costs more in
//! clarity than it buys back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../../tests/conformance");
    println!("cargo::rerun-if-changed={}", root.display());
    println!("cargo::rerun-if-changed=build.rs");

    // BTreeMap: the emitted blob and index are byte-identical across builds on any filesystem,
    // whatever order `read_dir` happens to hand back.
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    collect(&root, &root, &mut files);

    let mut blob: Vec<u8> = Vec::new();
    let mut rows: Vec<[u32; 4]> = Vec::with_capacity(files.len());
    for (rel, body) in &files {
        let p_off = blob.len() as u32;
        blob.extend_from_slice(rel.as_bytes());
        let s_off = blob.len() as u32;
        blob.extend_from_slice(body);
        rows.push([p_off, rel.len() as u32, s_off, body.len() as u32]);
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("examples.blob"), &blob).expect("write examples.blob");

    let mut index = String::with_capacity(rows.len() * 40);
    index.push_str("pub(crate) static EXAMPLE_INDEX: &[[u32; 4]] = &[\n");
    for [a, b, c, d] in rows {
        index.push_str(&format!("[{a},{b},{c},{d}],"));
    }
    index.push_str("\n];\n");
    std::fs::write(out.join("examples_index.rs"), index).expect("write examples_index.rs");
}

/// Every `*.noe` under `dir`, keyed by its path relative to `root` with `/` separators (the same
/// key `include_dir`'s `File::path` produced, so the feature/name derivation is unchanged).
fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "noe")
            && let Ok(rel) = path.strip_prefix(root)
            && let Some(rel) = rel.to_str()
            && let Ok(body) = std::fs::read(&path)
        {
            out.insert(rel.replace('\\', "/"), body);
        }
    }
}
