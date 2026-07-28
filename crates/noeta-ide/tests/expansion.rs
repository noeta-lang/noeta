//! Compile-time directive expansion **through the editor's link** — `noeta_db::linked_from`, and the
//! [`DocumentStore`] diagnostics view over it.
//!
//! Its own test binary, like the loader's `tests/expansion.rs` and `tests/dir_expansion.rs`, because
//! the extension registry installs **once per process**: two test binaries can each seed their own
//! fixture, two tests in one binary cannot.
//!
//! What it is here to prove is that the editor and the compiler agree about what a decorated type's
//! members are. `linked_from` did not expand, so a generated method resolved under `noeta run` and
//! `noeta check` and showed as an unknown name in the editor — the exact divergence the one shared
//! `noeta_loader::run_expansion` exists to prevent. And that a fault *inside* generated code is
//! still reported: the per-document diagnostics view filters to spans its own document owns, so
//! without re-attribution the user would see nothing at all for a real error.

use std::sync::atomic::{AtomicUsize, Ordering};

use noeta_ext_abi::registry::{
    DirectiveCtx, Expansion, ExpansionError, ExtDirective, ExtModule, Extension, TierSite,
};
use noeta_ide::DocumentStore;
use noeta_span::{Source, SourceId};

/// The spec path a reads-reporting fixture claims. Built from the directive's own file directory
/// (`ctx.source_dir`), exactly as `@openapi` resolves `"petstore.json"` — so the same fixture works
/// both for the in-memory `link` (rooted at `/proj`) and for the on-disk `ImpactSession` test.
fn spec_path(ctx: &DirectiveCtx) -> String {
    format!("{}/petstore.json", ctx.source_dir)
}

/// A directive whose expansion checks: one method returning a constant.
fn expand_ok(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Ok(Expansion {
        source: format!(
            "fn from_{}(): string {{ return self.base; }}\n",
            ctx.args[0]
        ),
        reads: Vec::new(),
    })
}

/// A directive whose expansion **parses but does not check**: the generated body calls a function
/// that does not exist. The fault is real, is the extension author's, and lands on a span the user's
/// document does not own.
fn expand_unchecked(_: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Ok(Expansion {
        source: "fn broken(): int { return no_such_function(); }\n".to_string(),
        reads: Vec::new(),
    })
}

/// Succeeds **and reports a read** — a spec-driven generator's shape. Its read must reach
/// [`noeta_db::LinkedProgram::reads`], the editor's rebuild trigger.
fn expand_reads(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Ok(Expansion {
        source: format!(
            "fn from_{}(): string {{ return self.base; }}\n",
            ctx.args[0]
        ),
        reads: vec![spec_path(ctx)],
    })
}

/// **Fails, but reports the file it read** — the reads-on-error contract. A missing spec is the
/// archetype: the read must survive the failure so that *creating* the spec re-runs the expansion.
fn expand_fails_with_read(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Err(ExpansionError {
        message: "the spec does not exist yet".to_string(),
        reads: vec![spec_path(ctx)],
    })
}

/// How many times the **shape** hook has run. Only [`editing_a_field_re_runs_the_expansion`] uses
/// `@ix_shape`, so the counter is not racy against the other tests in this binary.
static SHAPE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Generates one accessor per field of the decorated declaration, from `DirectiveCtx::fields` alone
/// — no arguments, no reads. Its output is therefore a pure function of the declaration's shape,
/// which is what makes a stale expansion detectable: if the memo did not participate in the field
/// edit, the generated members would still describe the *old* struct.
fn expand_shape(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    SHAPE_CALLS.fetch_add(1, Ordering::SeqCst);
    let mut source = String::new();
    for (name, spelling) in &ctx.fields {
        source.push_str(&format!(
            "fn {name}_type(): string {{ return \"{spelling}\"; }}\n"
        ));
    }
    Ok(Expansion {
        source,
        reads: Vec::new(),
    })
}

struct Fixture;

impl Extension for Fixture {
    fn name(&self) -> &'static str {
        "idefixture"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        const BASE: ExtDirective = ExtDirective {
            name: "",
            sites: &[TierSite::Type],
            max_args: Some(1),
            named_keys: &[],
            detail: "test fixture",
            doc: "test fixture",
            params: &["spec"],
            expand: None,
        };
        &[
            ExtDirective {
                name: "ix_expand",
                expand: Some(expand_ok),
                ..BASE
            },
            ExtDirective {
                name: "ix_unchecked",
                expand: Some(expand_unchecked),
                ..BASE
            },
            ExtDirective {
                name: "ix_reads",
                expand: Some(expand_reads),
                ..BASE
            },
            ExtDirective {
                name: "ix_fails",
                expand: Some(expand_fails_with_read),
                ..BASE
            },
            // Argument-free, so nothing but the decorated declaration's own shape can change what
            // it emits.
            ExtDirective {
                name: "ix_shape",
                max_args: Some(0),
                params: &[],
                expand: Some(expand_shape),
                ..BASE
            },
        ]
    }
}

static FIXTURE: Fixture = Fixture;

/// The registry installs once per process; `cargo test` runs these in threads within one process, so
/// every test funnels through this (idempotent-by-first-caller) install rather than doing its own.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));
}

/// A one-member salsa workspace over `text`, linked from it as the entry — the editor's own query,
/// reached directly so the test can look at what the link produced.
fn link(text: &str) -> noeta_db::LinkedProgram {
    install();
    let db = noeta_db::LangDatabase::default();
    let entry = Source::new(SourceId(0), "/proj/main.noe", text);
    let ws = noeta_db::workspace(&db, &entry, &[], noeta_lexer::Edition::DEFAULT);
    // Cloned out of the memo so the db can be dropped with it: the assertions are about the value,
    // and a test has no incrementality to preserve.
    noeta_db::linked_from(&db, ws, noeta_db::workspace_entry(&db, ws)).clone()
}

/// The method names of struct `name` in a linked program.
fn methods_of(program: &noeta_ast::Program, name: &str) -> Vec<String> {
    program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == name => {
                Some(d.methods.iter().map(|m| m.name.to_string()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// A store with one open document. The directory does not exist on disk, so the scan yields nothing
/// and the open buffer is the workspace's only member — the single-file editing case.
fn store_with(uri: &str, text: &str) -> DocumentStore {
    install();
    let mut store = DocumentStore::default();
    store.open(uri, text.to_string());
    store
}

const EXPANDING: &str = r#"
@ix_expand("petstore")
struct Api {
    base: string
    fn ping(): int { return 0; }
}
echo 1;
"#;

const PLAIN: &str = "struct Plain { x: int }\necho 1;\n";

/// The editor's link splices the generated members in, hand-written ones first — the same program
/// the compiler builds, because it is the same one expansion.
#[test]
fn the_editor_s_link_carries_the_generated_members() {
    let linked = link(EXPANDING);
    let program = linked.program.as_ref().expect("the entry links");
    assert_eq!(methods_of(program, "Api"), vec!["ping", "from_petstore"]);
}

/// The generated source comes back with the link, id'd past every member (and dependency module), so
/// a span inside it is addressable rather than dangling — and the generated member's own span really
/// does carry that id.
#[test]
fn the_generated_source_is_numbered_past_the_workspace_s_own() {
    let linked = link(EXPANDING);
    let program = linked.program.as_ref().expect("the entry links");

    assert_eq!(linked.expansions.len(), 1);
    let generated = &linked.expansions[0].source;
    // One member, no dependency modules → the first unused id is 1.
    assert_eq!(generated.id(), SourceId(1));
    assert_eq!(generated.name(), r#"Api ⟨@ix_expand "petstore"⟩"#);

    let span = program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == "Api" => {
                d.methods.iter().find(|m| m.name == "from_petstore")
            }
            _ => None,
        })
        .expect("the generated method is present")
        .name_span;
    assert_eq!(span.source, generated.id());
    assert_eq!(generated.slice(span), "from_petstore");

    // The origin travels with it: the `@ix_expand` token in the user's own file, which is the only
    // span in a file the author can act on.
    let origin = linked.expansions[0].origin;
    assert_eq!(origin.source, SourceId(0));
    assert_eq!(&EXPANDING[origin.range()], "ix_expand");
}

/// The user-visible half: calling a generated method is **clean** in the editor. Before expansion ran
/// here it was an unknown-member error on a program `noeta run` and `noeta check` both accept.
#[test]
fn calling_a_generated_method_is_clean_in_the_editor() {
    let uri = "file:///noeta-ide-expansion-call/main.noe";
    let store = store_with(
        uri,
        "@ix_expand(\"petstore\")\nstruct Api { base: string }\nfn go(a: Api): string { return a.from_petstore(); }\necho 1;\n",
    );
    let (diags, _) = store.diagnostics(uri).expect("the document is known");
    assert!(
        diags.is_empty(),
        "the generated method must resolve in the editor, got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// A checker fault **inside** generated code is reported at the directive that produced it. Without
/// this the per-document filter drops it and the editor shows a clean file the compiler rejects —
/// silence being strictly worse than a misplaced error.
#[test]
fn a_fault_in_generated_code_is_reported_at_the_directive() {
    let uri = "file:///noeta-ide-expansion-fault/main.noe";
    let text = "@ix_unchecked(\"x\")\nstruct Api { base: string }\necho 1;\n";
    let store = store_with(uri, text);
    let (diags, _) = store.diagnostics(uri).expect("the document is known");

    let reblamed: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("generated code that does not check"))
        .collect();
    assert_eq!(
        reblamed.len(),
        1,
        "exactly one re-attributed diagnostic, got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let d = reblamed[0];
    assert!(
        d.message.contains("no_such_function"),
        "the original fault must survive the move: {}",
        d.message
    );
    // It lands on the directive the author wrote, in this file.
    assert_eq!(&text[d.span.range()], "ix_unchecked");
    assert!(d.help.is_some(), "and says whose fault it is");

    // Every diagnostic the view publishes belongs to this document (the sole member, id 0): a
    // foreign span would render at a nonsense offset in the wrong file.
    assert!(
        diags.iter().all(|d| d.span.source == SourceId(0)),
        "a published diagnostic must be addressed to the open document"
    );
}

/// A successful expansion's reads reach the editor's link — the rebuild trigger the watcher folds
/// into its watch set so a spec edit re-runs the generation.
#[test]
fn a_successful_expansion_reports_its_reads_to_the_link() {
    let linked = link("@ix_reads(\"petstore\")\nstruct Api { base: string }\necho 1;\n");
    assert!(linked.program.is_ok(), "the entry links");
    assert_eq!(linked.reads, vec!["/proj/petstore.json".to_string()]);
}

/// **The reads-on-error crux.** A hook that fails because its spec is missing still reports the
/// path, and that report survives all the way to `LinkedProgram.reads` even though the program did
/// not link. Without this, creating the spec would leave the client stale — the watcher was never
/// told to watch it. A `Result<Expansion, String>` could not carry it; the reads lived only in the
/// `Ok`.
#[test]
fn a_failed_expansion_still_reports_its_reads_to_the_link() {
    let linked = link("@ix_fails(\"petstore\")\nstruct Api { base: string }\necho 1;\n");
    assert!(
        linked.program.is_err(),
        "a failed expansion fails the link (the members it declared are absent)"
    );
    assert_eq!(
        linked.reads,
        vec!["/proj/petstore.json".to_string()],
        "the read must survive the failure — creating the spec has to re-trigger the expansion"
    );
}

/// A workspace with no expanding directive is untouched: no expansion sources, no id past the
/// members, and nothing to report. The cheap guard inside `run_expansion` means such a program never
/// even materializes its sources.
#[test]
fn a_workspace_with_no_expanding_directive_is_unchanged() {
    let linked = link(PLAIN);
    assert!(linked.program.is_ok(), "the entry links");
    assert!(linked.expansions.is_empty(), "nothing was generated");
    assert!(linked.reads.is_empty(), "and nothing was read");

    let uri = "file:///noeta-ide-expansion-plain/main.noe";
    let store = store_with(uri, PLAIN);
    assert!(
        store.diagnostics(uri).expect("known").0.is_empty(),
        "and a clean program stays clean"
    );
}

/// **The incrementality crux for `DirectiveCtx::fields`.** Editing the decorated struct's *fields*
/// must re-run its `expand` hook.
///
/// Expansion is memoized: it runs inside the `linked_from` salsa query, whose inputs are the
/// workspace's `SourceProgram` texts. The declaration's fields are read out of the parsed entry, so
/// they are part of that memo's input by construction — but "by construction" is exactly the kind of
/// claim that quietly stops being true. A stale expansion here would be silent: the program would
/// keep compiling, against members describing a struct that no longer exists.
///
/// Both halves are asserted, because either alone can pass while the feature is broken: the hook
/// really *ran again* (the counter), and what it produced really *describes the new shape*.
#[test]
fn editing_a_field_re_runs_the_expansion() {
    use salsa::Setter as _;

    install();
    let mut db = noeta_db::LangDatabase::default();
    let before = "@ix_shape\nstruct Api { base: string }\necho 1;\n";
    let entry = Source::new(SourceId(0), "/proj/main.noe", before);
    let ws = noeta_db::workspace(&db, &entry, &[], noeta_lexer::Edition::DEFAULT);
    let src = noeta_db::workspace_entry(&db, ws);

    let linked = noeta_db::linked_from(&db, ws, src);
    assert_eq!(
        methods_of(linked.program.as_ref().expect("the entry links"), "Api"),
        vec!["base_type"]
    );
    assert_eq!(
        linked.expansions[0].source.text(),
        "struct Api {\nfn base_type(): string { return \"string\"; }\n}\n"
    );
    let runs_before = SHAPE_CALLS.load(Ordering::SeqCst);

    // The edit: rename the field and give it a generic type. Nothing else about the directive
    // changes — same name, same (absent) arguments, same file, no reads.
    src.set_text(&mut db)
        .to("@ix_shape\nstruct Api { tags: List<int> }\necho 1;\n".to_string());

    let linked = noeta_db::linked_from(&db, ws, src);
    assert_eq!(
        methods_of(
            linked.program.as_ref().expect("the entry still links"),
            "Api"
        ),
        vec!["tags_type"],
        "the expansion is stale: it still describes the struct's old fields"
    );
    assert_eq!(
        linked.expansions[0].source.text(),
        // And at full fidelity — a `List<int>` field must not reach the hook as `List`.
        "struct Api {\nfn tags_type(): string { return \"List<int>\"; }\n}\n"
    );
    assert!(
        SHAPE_CALLS.load(Ordering::SeqCst) > runs_before,
        "the hook must actually re-run — a memo that served the old answer would be the bug"
    );
}

/// The **watcher consumer**, end to end: an `ImpactSession` over an on-disk project whose entry
/// carries a reads-reporting directive captures the read, and reports a change to that file as
/// `Impact::All` — so `noeta test --watch` re-runs when the spec changes. This is the whole point of
/// carrying `reads` to `LinkedProgram`: the impact diff speaks the `.noe` vocabulary and cannot see a
/// `.json` edit unless it is taught to.
#[test]
fn an_impact_session_watches_the_files_an_expansion_read() {
    install();
    // A unique on-disk project — `ImpactSession` scans a real directory.
    let dir = std::env::temp_dir().join(format!(
        "noeta-reads-session-{}",
        std::process::id() as u64 * 31 + 7
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the project dir");
    let entry = dir.join("main.noe");
    std::fs::write(
        &entry,
        "@ix_reads(\"petstore\")\nstruct Api { base: string }\n@test fn t(): void { assert(true); }\n",
    )
    .expect("write the entry");
    // The spec the directive reports reading — present, so the project links clean.
    let spec = dir.join("petstore.json");
    std::fs::write(&spec, "{}").expect("write the spec");

    let mut session =
        noeta_ide::impact::ImpactSession::new(&entry).expect("the entry anchors a project");

    // The session captured the spec as a watched read (canonicalized).
    let canon_spec = spec.canonicalize().expect("the spec exists");
    assert!(
        session.reads().contains(&canon_spec),
        "the session must watch the spec the directive read; watched: {:?}",
        session.reads()
    );

    // A change to the spec is All-impact: a `.noe` diff cannot narrow it, and it invalidates the
    // generated members, so the whole suite must rerun.
    match session.impact_of_changes(std::slice::from_ref(&spec)) {
        noeta_ide::impact::Impact::All { reason } => {
            assert!(
                reason.contains("spec read by an expanding directive"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("a spec change must rerun everything, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
