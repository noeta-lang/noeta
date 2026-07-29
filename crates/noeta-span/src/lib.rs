//! Source spans and source-file bookkeeping — the shared vocabulary every other
//! crate uses to point at a place in the source. Deliberately tiny and dependency-light.

use serde::{Deserialize, Serialize};

/// Identifies a single source file within a [`SourceMap`].
///
/// M0 only ever has one source loaded at a time, but the id exists from the start
/// so multi-file support (M1 modules) does not require touching every span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceId(pub u32);

impl SourceId {
    /// The id assigned to the first (and, in M0, only) loaded source.
    pub const FIRST: SourceId = SourceId(0);
}

/// A half-open byte range `[start, end)` into a source file, tagged with the [`SourceId`]
/// of the file those offsets index.
///
/// Byte offsets (not char offsets) so they map directly onto `&str` slicing and onto the
/// offsets the lexer reports. The offsets stay **local** to their source (each file is parsed
/// 0-based); `source` is what disambiguates them once declarations from several modules are
/// merged into one program — a diagnostic renders against `source`, not the entry. Spans built
/// without a known source default to [`SourceId::FIRST`] (the entry, and the only source in a
/// single-file program), so single-source and synthetic call sites need no source threaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub source: SourceId,
}

impl Span {
    /// A span in the entry source ([`SourceId::FIRST`]). The ergonomic constructor for the
    /// single-source and synthetic cases; the parser uses [`Span::new_in`] to stamp the real id.
    pub fn new(start: u32, end: u32) -> Span {
        Span::new_in(SourceId::FIRST, start, end)
    }

    /// A span in `source`. Used by the parser, which knows which file it is parsing.
    pub fn new_in(source: SourceId, start: u32, end: u32) -> Span {
        debug_assert!(start <= end, "span start must not exceed end");
        Span { start, end, source }
    }

    /// An empty span at `offset` in the entry source, useful for "unexpected end of input".
    pub fn empty_at(offset: u32) -> Span {
        Span::empty_at_in(SourceId::FIRST, offset)
    }

    /// An empty span at `offset` in `source`.
    pub fn empty_at_in(source: SourceId, offset: u32) -> Span {
        Span {
            start: offset,
            end: offset,
            source,
        }
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The smallest span covering both `self` and `other`, keeping `self`'s source (the two are
    /// expected to come from the same file; merging across sources is not meaningful).
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            source: self.source,
        }
    }

    /// This span moved by `by` bytes, keeping its source. Used when a span is computed against a
    /// substring (e.g. a string-interpolation hole) and rebased to its absolute source position.
    pub fn shifted(self, by: u32) -> Span {
        Span {
            start: self.start + by,
            end: self.end + by,
            source: self.source,
        }
    }

    /// This span re-tagged to `source`, keeping its offsets. Used when offsets were computed
    /// against a throwaway source (e.g. an interpolation hole lexed on its own) and belong to the
    /// real enclosing source.
    pub fn with_source(self, source: SourceId) -> Span {
        Span { source, ..self }
    }

    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

impl From<std::ops::Range<usize>> for Span {
    fn from(range: std::ops::Range<usize>) -> Span {
        Span {
            start: range.start as u32,
            end: range.end as u32,
            source: SourceId::FIRST,
        }
    }
}

/// A 1-based line and column position, for human-facing diagnostics and the
/// conformance corpus's `error CODE at L:C` expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// One loaded source file: its name, its text, and a precomputed index of line
/// starts for O(log n) offset → line:col lookup.
#[derive(Debug, Clone)]
pub struct Source {
    id: SourceId,
    name: String,
    text: String,
    /// Byte offset of the start of each line. `line_starts[0]` is always 0.
    line_starts: Vec<u32>,
}

impl Source {
    pub fn new(id: SourceId, name: impl Into<String>, text: impl Into<String>) -> Source {
        let text = text.into();
        let mut line_starts = vec![0u32];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset as u32 + 1);
            }
        }
        Source {
            id,
            name: name.into(),
            text,
            line_starts,
        }
    }

    pub fn id(&self) -> SourceId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The source text covered by `span`.
    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.range()]
    }

    /// Convert a byte offset into a 1-based line and column.
    pub fn line_col(&self, offset: u32) -> LineCol {
        // Find the last line start that is <= offset.
        let line_idx = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };
        let line_start = self.line_starts[line_idx];
        // Column counts characters, not bytes, so multi-byte UTF-8 reads naturally.
        let col = self.text[line_start as usize..offset as usize]
            .chars()
            .count() as u32;
        LineCol {
            line: line_idx as u32 + 1,
            col: col + 1,
        }
    }
}

/// The set of [`Source`]s that make up one loaded program — the entry plus any sibling modules
/// merged into it — indexed by [`SourceId`]. A [`Span`] carries the id of the source it indexes,
/// so a diagnostic produced anywhere in the merged program resolves back to the right file and
/// its local line/column through this map, even when the offending declaration was pulled in from
/// a sibling module.
#[derive(Debug, Clone)]
pub struct SourceMap {
    /// Sources keyed by `SourceId(index)`. The entry is always index 0 ([`SourceId::FIRST`]).
    sources: Vec<Source>,
}

impl SourceMap {
    /// Build a map from sources. The first source is the entry (`SourceId::FIRST`); the rest
    /// follow in id order. Ids are expected to match their position, as the loader assigns them.
    pub fn new(sources: Vec<Source>) -> SourceMap {
        SourceMap { sources }
    }

    /// The source a span belongs to. Falls back to the entry for an out-of-range id, so a stray
    /// synthetic span can never panic the renderer (it renders against the entry, as it did
    /// before source attribution existed).
    pub fn source(&self, id: SourceId) -> &Source {
        self.sources.get(id.0 as usize).unwrap_or(&self.sources[0])
    }

    /// The entry source (`SourceId::FIRST`).
    pub fn entry(&self) -> &Source {
        &self.sources[0]
    }

    /// The 1-based line/column of `span`'s start, resolved against the source it belongs to.
    pub fn line_col(&self, span: Span) -> LineCol {
        self.source(span.source).line_col(span.start)
    }

    /// Consume the map into its sources, in id order — for a consumer that continues assigning
    /// `SourceId`s where the loader stopped (the REPL's `--load` bootstrap: entry ids follow the
    /// file's, so a stack trace into a bootstrap function renders against its real text).
    pub fn into_sources(self) -> Vec<Source> {
        self.sources
    }
}

/// Which **package** a source came from.
///
/// The linker merges every package's modules into one program, so a package boundary survives
/// linking only as a side-table (see [`PackageMap`]). This is the identity that table records.
///
/// Deliberately *not* a namespace: a `namespace` is declared per file and has no required
/// relationship to the package that shipped it, so two packages may declare namespaces under one
/// root and one package may spread across several. Only the loader knows which package it read a
/// file from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageOrigin {
    /// The **root** package — the program being compiled: its entry file and the sibling modules
    /// beside it. Unnamed because the loader is handed an entry path, not a manifest; "the root
    /// package" is how a diagnostic refers to it.
    Root,
    /// A **dependency** package, named by the globally-unique link segment the resolver assigned it
    /// (the consumer's dependency key for a direct dependency, a synthesized unique segment for a
    /// transitive-only one) — the same segment its declarations are addressed under after
    /// re-rooting, so the name in a diagnostic is the name in the source.
    Dependency(String),
}

impl std::fmt::Display for PackageOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageOrigin::Root => f.write_str("the root package"),
            PackageOrigin::Dependency(key) => write!(f, "package `{key}`"),
        }
    }
}

/// Which package each source of a merged program came from, keyed by [`SourceId`] — the package
/// analogue of `noeta_edition::EditionMap`, and the carrier the checker's package orphan rule reads.
///
/// The loader merges every package's modules into **one** program, so a package boundary cannot be a
/// property of the program: the merged statement list mixes declarations from many packages, and by
/// the time the checker runs, nothing in the AST says where a declaration came from. Each
/// declaration's span already carries the [`SourceId`] of the file it was parsed from, so this
/// side-table recovers "which package declared this?" from any span.
///
/// **An unrecorded source has *unknown* provenance, and every provenance-dependent rule stands
/// down.** A single-file check, a REPL fragment, a synthetic program, and compile-time generated
/// code are all unrecorded, so [`Self::at`] answers `None` and the orphan rule cannot fire on a
/// guess. An empty map means "provenance is unknown everywhere", which is exactly what a plain
/// `check_all` wants — never "everything is one package".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageMap {
    by_source: std::collections::HashMap<SourceId, PackageOrigin>,
}

impl PackageMap {
    /// An empty map — every source's package is unknown.
    pub fn new() -> PackageMap {
        PackageMap::default()
    }

    /// Record that the source `id` was read from `origin`.
    pub fn set(&mut self, id: SourceId, origin: PackageOrigin) {
        self.by_source.insert(id, origin);
    }

    /// The package the source `id` came from, or `None` when it was never recorded.
    pub fn source_package(&self, id: SourceId) -> Option<&PackageOrigin> {
        self.by_source.get(&id)
    }

    /// The package that declared whatever `span` points at — [`Self::source_package`] of the span's
    /// owning source. The per-span lookup a whole-program rule uses to compare two declarations'
    /// packages.
    pub fn at(&self, span: Span) -> Option<&PackageOrigin> {
        self.source_package(span.source)
    }

    /// Whether nothing has been recorded — i.e. provenance is unknown for every source.
    pub fn is_empty(&self) -> bool {
        self.by_source.is_empty()
    }

    /// How many sources have a recorded package.
    pub fn len(&self) -> usize {
        self.by_source.len()
    }
}

/// One resolved per-package `@`-name binding (`[directives]` / `[tiers]`): the provider namespace
/// **root** segment(s) the local `@name` resolves to — a scope dependency key covers several member
/// packages, hence a list — and the name the provider exported. Matched against an extension unit by
/// `unit.root() ∈ provider_roots && declared-name == exported`, the same root-namespace identity the
/// module system uses. Built per-package in the loader from a package's manifest table, in that
/// package's own dependency context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUse {
    /// The provider package(s)' namespace root segment(s) this local name resolves to.
    pub provider_roots: Vec<String>,
    /// The tier/directive name the provider declared.
    pub exported: String,
}

/// Per-package `@`-name resolution tables, keyed by the **using** package and then the local `@name`
/// it writes in source. The checker recovers a `@name`'s package from its span's [`SourceId`]
/// (via [`PackageMap`]) and looks the local name up here. An absent entry means the package binds no
/// such name — resolution reports it unmapped, rather than reaching into a global namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageUses {
    by_package:
        std::collections::HashMap<PackageOrigin, std::collections::HashMap<String, PackageUse>>,
}

impl PackageUses {
    /// An empty set of tables.
    pub fn new() -> PackageUses {
        PackageUses::default()
    }

    /// Record that `origin`'s source binds local `@name` to `use_`.
    pub fn set(&mut self, origin: PackageOrigin, local: String, use_: PackageUse) {
        self.by_package
            .entry(origin)
            .or_default()
            .insert(local, use_);
    }

    /// Resolve local `@name` for a given using package, or `None` when that package binds no such name.
    pub fn get(&self, origin: &PackageOrigin, local: &str) -> Option<&PackageUse> {
        self.by_package.get(origin)?.get(local)
    }

    /// Whether nothing has been recorded (no package binds any `@`-name — the single-file default).
    pub fn is_empty(&self) -> bool {
        self.by_package.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_map_defaults_an_unrecorded_source_to_unknown() {
        // Unknown, never "the root package": a rule that reads this must stand down rather than
        // judge a single-file or synthetic program as though it were a resolved dependency graph.
        let map = PackageMap::new();
        assert!(map.is_empty());
        assert_eq!(map.source_package(SourceId(0)), None);
        assert_eq!(map.at(Span::new_in(SourceId(3), 0, 1)), None);
    }

    #[test]
    fn package_map_recovers_a_recorded_source_and_span() {
        let mut map = PackageMap::new();
        map.set(SourceId(0), PackageOrigin::Root);
        map.set(SourceId(2), PackageOrigin::Dependency("glue".to_string()));
        assert_eq!(map.len(), 2);
        assert_eq!(map.source_package(SourceId(0)), Some(&PackageOrigin::Root));
        assert_eq!(
            map.at(Span::new_in(SourceId(2), 4, 8)),
            Some(&PackageOrigin::Dependency("glue".to_string()))
        );
        assert_eq!(map.source_package(SourceId(9)), None);
    }

    #[test]
    fn a_package_origin_names_itself_for_a_diagnostic() {
        assert_eq!(PackageOrigin::Root.to_string(), "the root package");
        assert_eq!(
            PackageOrigin::Dependency("vendor_a".to_string()).to_string(),
            "package `vendor_a`"
        );
    }

    #[test]
    fn span_merge_and_len() {
        let a = Span::new(2, 5);
        let b = Span::new(7, 9);
        assert_eq!(a.len(), 3);
        assert_eq!(a.merge(b), Span::new(2, 9));
        assert!(Span::empty_at(4).is_empty());
    }

    #[test]
    fn line_col_lookup() {
        let src = Source::new(SourceId::FIRST, "test.noe", "ab\ncde\nf");
        assert_eq!(src.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(src.line_col(1), LineCol { line: 1, col: 2 });
        assert_eq!(src.line_col(3), LineCol { line: 2, col: 1 });
        assert_eq!(src.line_col(7), LineCol { line: 3, col: 1 });
    }

    #[test]
    fn line_col_handles_multibyte() {
        // "é" is two bytes (offsets 0..2); the column at the following "!" (offset 2)
        // should be 2, because columns count characters, not bytes.
        let src = Source::new(SourceId::FIRST, "t.noe", "é!");
        assert_eq!(src.line_col(2), LineCol { line: 1, col: 2 });
    }

    #[test]
    fn source_map_resolves_a_span_to_its_own_source() {
        // Two sources with deliberately different shapes: the same local offset means a different
        // line in each, so resolving through the map (not the entry) is what gets it right.
        let entry = Source::new(SourceId(0), "main.noe", "echo 1;\n");
        let sibling = Source::new(SourceId(1), "models.noe", "a\nb\nc / 0;\n");
        let map = SourceMap::new(vec![entry, sibling]);

        // A span tagged for the sibling at offset 6 ("/" on line 3) resolves against the sibling.
        let in_sibling = Span::new_in(SourceId(1), 6, 7);
        assert_eq!(map.line_col(in_sibling), LineCol { line: 3, col: 3 });
        assert_eq!(map.source(SourceId(1)).name(), "models.noe");

        // An entry span resolves against the entry.
        let in_entry = Span::new_in(SourceId(0), 5, 6);
        assert_eq!(map.line_col(in_entry), LineCol { line: 1, col: 6 });

        // An out-of-range id falls back to the entry rather than panicking.
        assert_eq!(map.source(SourceId(9)).name(), "main.noe");
    }
}
