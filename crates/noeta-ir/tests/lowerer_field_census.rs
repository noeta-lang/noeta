//! **The lowerer field census**: every field of `Lowerer` is classified here by *what kind of state
//! it is*, and the classification is checked against the tree.
//!
//! ## The bug class
//!
//! `Lowerer` is driven two ways. The file pipeline hands it a **whole program**. A hot-swap install
//! and a REPL entry hand it a **fragment** of one. Anything the lowering derives by reading
//! `program.stmts` is therefore complete in the first case and empty-or-partial in the second — and
//! the fragment caller has no way to know which of the lowerer's fields are in that category, because
//! nothing on the struct says.
//!
//! Three shipped bugs came from exactly that, one per table:
//!
//! 1. an `@html { … }` inside a swapped body lowered to a **panic** — the `@tier` declaration that
//!    names its handler is in the imported package, not in the fragment (`expr_tiers`);
//! 2. `x is Uuid` in a swapped body silently answered **`false`** — only a *changed* `use` rides in
//!    a fragment, so the narrowing identity was missing (`type_aliases`, `native_type_imports`);
//! 3. a swapped `async` body that assigned a module global **panicked** — the state-machine desugar
//!    hoisted the global into a fresh cell because it did not know it was a global
//!    (`module_globals`).
//!
//! Each was found, and fixed, separately. The fix that ended the class was structural: those tables
//! moved into ONE struct, [`ProgramFacts`], which a fragment lowering receives as
//! `LowerOptions::ambient` and which `ProgramFacts::under` folds beneath what the code in hand
//! declares for itself.
//!
//! That fix is sound and it is also unenforced. Nothing stops the next patch adding a fifth
//! program-derived table as a *sibling* of `facts` on `Lowerer` — and the reviewer of that patch has
//! no reason to know fragments exist. This file makes that a test failure instead of a fourth bug.
//!
//! ## What the gate does
//!
//! [`TABLE`] classifies **every** field of `Lowerer` as one of:
//!
//! - [`Kind::PerNode`] — reset or scoped per node/frame. It carries no information between one
//!   lowering and the next, so a fragment and a whole program see the same thing by construction.
//!   Its anchor is the site that resets or scopes it; a "per-node" field nothing ever resets is
//!   mis-classified.
//! - [`Kind::Environment`] — supplied by the caller, not derived from the program. Its anchor is the
//!   [`LowerOptions`] field it is threaded from: what makes it safe is that the caller states it,
//!   and a fragment caller states it exactly as a whole-program caller does.
//! - [`Kind::CheckerSites`] — the checker's span-keyed lowering hints. Whole-program by
//!   construction: the checker that produced them saw the whole program, and a fragment install
//!   carries the sites the checker computed for it. That is right about the **bundle** and says
//!   nothing about a new **field** of it, so the bundle has a census of its own —
//!   `noeta_check::SITE_POLICIES`, gated by `noeta-check/tests/site_policies.rs`, which asks each
//!   field the question this class cannot: *whose numbering are its values in?* A fresh check
//!   numbers the type-argument table from zero in its own discovery order, so a field carrying one
//!   of those indices is only meaningful together with the run that produced it and a live session
//!   must renumber it on install.
//! - [`Kind::ProgramDerived`] — read off the program being lowered. **There must be exactly one**,
//!   and it must be the [`ProgramFacts`] bundle. This is the class that has failed three times, and
//!   the singleton rule is the whole point: a second one is a fragment bug waiting to happen. It
//!   belongs *inside* `ProgramFacts`, where `under` folds it and where this file's second half
//!   demands a fragment test for it.
//!
//! Five properties are then checked:
//!
//! - **Completeness** — the field list is parsed out of `lower.rs` and every field must appear in
//!   [`TABLE`] exactly once. Adding a field to `Lowerer` fails this test until it is classified.
//!   (The trick is borrowed from the ABI declared-constraint gate,
//!   `noeta-ext-abi/tests/constraint_fields.rs`, which keeps its own hand-written table honest by
//!   counting fields out of the sources it covers.)
//! - **Liveness** — each anchor must still exist: the file must be present and must contain the
//!   anchor's needle. Deleting a reset site, or renaming the option a field is threaded from, fails
//!   here rather than quietly leaving a mis-classified field behind.
//! - **The singleton** — exactly one `ProgramDerived` row, and it is `facts`.
//! - **The construction** — in `lower_with_sites_opts`, only the `facts:` initializer of the
//!   `Lowerer { … }` literal may mention `program`. This is the half that fires on the *precise*
//!   mistake: a new table computed from the program and dropped in beside `facts` is caught even
//!   before anyone thinks about how to classify it.
//! - **The fragment story** — every field of `ProgramFacts` must be folded by BOTH `under` and
//!   `absorb`, and must name a hot-swap test that watches a fragment actually get it right. A fact
//!   that `under` forgets is silently the pre-fix behavior; a fact with no fragment test is a fix
//!   nothing is watching.
//!
//! ## Why source text, and why also a compile error
//!
//! Rust has no stable way for a test to enumerate a *private* struct's fields, and `Lowerer` is
//! private (correctly — it is an implementation detail of one function). A derive macro that emitted
//! a field list would be a new proc-macro crate and a new build dependency to answer a question one
//! `find` answers; the ABI gate already established that reading the source is the house answer.
//!
//! Reading text has one failure mode: a field written in a shape the scanner does not recognize is
//! invisible to it, and an invisible field passes. So the census has a compile-time half —
//! `lower.rs::every_lowerer_field_is_named_by_the_census` builds a `Lowerer` and takes it apart with
//! no `..` on either side. Whatever shape a new field is written in, it fails to *compile* there.
//! Between the two, a new field cannot reach `main` unclassified: the compiler catches that it
//! exists, and this file catches what it is.
//!
//! [`ProgramFacts`]: noeta_ir::ProgramFacts
//! [`LowerOptions`]: noeta_ir::LowerOptions

use std::path::{Path, PathBuf};

const LOWER: &str = "crates/noeta-ir/src/lower.rs";
const HOTSWAP: &str = "crates/noeta-vm/tests/hotswap.rs";

/// A named site in the tree that must still exist: `needle` is a substring stable across ordinary
/// edits (a signature, a statement, a test name), not a line number.
struct Anchor(&'static str, &'static str);

impl Anchor {
    /// The workspace-relative source file this anchor points into.
    fn file(&self) -> &'static str {
        self.0
    }
    /// A substring that must still be present there.
    fn needle(&self) -> &'static str {
        self.1
    }
}

/// What kind of state a `Lowerer` field is — which is the same question as "what does a **fragment**
/// lowering see in it?", asked in a form the author of a new field has to answer.
enum Kind {
    /// Reset or scoped per node/frame, so it never carries whole-program knowledge. The anchor is
    /// the reset/scope site.
    PerNode(Anchor),
    /// Supplied by the caller. The anchor is the [`noeta_ir::LowerOptions`] field it comes from.
    Environment(Anchor),
    /// The checker's whole-program lowering hints. The anchor is the bundle they arrive in.
    ///
    /// This class is about the bundle as a whole. Per-*field* obligations — above all "is this
    /// value's meaning tied to the check run that produced it, so that a live session has to
    /// renumber it?" — live in `noeta_check::SITE_POLICIES`, which classifies all thirty-five and
    /// is gated by `noeta-check/tests/site_policies.rs`.
    CheckerSites(Anchor),
    /// Read off the program being lowered. Exactly one field may be this, and it must be the
    /// `ProgramFacts` bundle; the anchor is the fold rule that gives a fragment the enclosing
    /// program's copy.
    ProgramDerived(Anchor),
}

struct Row(&'static str, Kind);

impl Row {
    /// The `Lowerer` field.
    fn field(&self) -> &'static str {
        self.0
    }
    fn kind(&self) -> &Kind {
        &self.1
    }
    fn anchor(&self) -> &Anchor {
        match self.kind() {
            Kind::PerNode(a)
            | Kind::Environment(a)
            | Kind::CheckerSites(a)
            | Kind::ProgramDerived(a) => a,
        }
    }
}

use Kind::{CheckerSites, Environment, PerNode, ProgramDerived};

/// The classification. One row per field of `Lowerer`; the completeness check below keeps it
/// exhaustive.
const TABLE: &[Row] = &[
    // --- per-node lowering state ----------------------------------------------------------------
    // The frame's temp counter. `lower_func` saves it, zeroes it for the callee's frame and restores
    // it on the way out, so it describes the innermost activation and nothing wider.
    Row("temps", PerNode(Anchor(LOWER, "let outer = self.temps;"))),
    // How many function frames enclose the code in hand. Incremented on the way into a body and
    // decremented on the way out — a depth, never a program property.
    Row("fn_depth", PerNode(Anchor(LOWER, "self.fn_depth += 1;"))),
    // How many hidden type-argument slots the innermost top-level `fn` carries — the operands a
    // door's `RenderHint::Param` resolves through. Set on the way into a top-level declaration and
    // restored on the way out, retained through nested declarations (which reach the same locals as
    // captures), so it describes the frame in hand rather than the program.
    Row(
        "hidden_slots",
        PerNode(Anchor(LOWER, "self.hidden_slots = outer_hidden;")),
    ),
    // The receiver-read half of that same slot list — how many of the enclosing generic type's own
    // parameters a door in this body reads off `self`'s reflected tag. Set and restored at the same
    // two sites as `hidden_slots`, and retained through nested declarations for the same reason (a
    // closure captures the very `self` its enclosing method reads), so it describes the frame in
    // hand rather than the program.
    Row(
        "self_render_slots",
        PerNode(Anchor(LOWER, "self.self_render_slots = outer_self_render;")),
    ),
    // Armed just before an async/generator desugar lowers its synthesized step closure and `take()`n
    // by the first closure the lowering meets. The `take` is what makes it per-node: a user's own
    // closure always finds `None`.
    Row(
        "synth_step_name",
        PerNode(Anchor(LOWER, "let name = self.synth_step_name.take();")),
    ),
    // The seal that rides along with `synth_step_name`, armed and taken at the same two points.
    Row(
        "synth_step_captures",
        PerNode(Anchor(
            LOWER,
            "let captures = self.synth_step_captures.take();",
        )),
    ),
    // What `Self` denotes. Scoped around a type's own members and restored on the way out, exactly
    // as `temps` is scoped around a frame — a property of where the lowering is, not of the
    // program. A fragment lowering derives it from the declaration it is handed, which is why it
    // does not belong in `ProgramFacts`.
    Row(
        "self_type_name",
        PerNode(Anchor(LOWER, "let saved = self.self_type_name.replace(")),
    ),
    // --- environment / config -------------------------------------------------------------------
    // Whether `isolate f(args)` lowers to the real OS-thread spawn. A property of who is running the
    // lowering (only the CLI's VM path sets it), not of the code being lowered.
    Row(
        "real_isolates",
        Environment(Anchor(LOWER, "pub real_isolates: bool,")),
    ),
    // The extension registry native tiers and native type identities resolve against. Threaded from
    // the caller precisely so an embed session's own assembled set is honored; a fragment install
    // passes the session's registry exactly as a cold compile passes the process-global one.
    Row(
        "registry",
        Environment(Anchor(
            LOWER,
            "pub registry: &'static noeta_ext_abi::registry::Registry,",
        )),
    ),
    // --- checker-supplied sites -----------------------------------------------------------------
    // The checker's span-keyed hints. Whole-program by construction on both paths: the checker that
    // produced them saw a whole program, and a hot-swap install carries the bundle the checker
    // computed for the fragment (`a_mailbox_deposit_installs_the_fragment_with_its_sites`). Empty on
    // the deliberately hint-free REPL/IR-corpus path, where "empty" costs a fusion, never a meaning.
    // Whether an individual field of the bundle is safe to install into a LIVE compiler is a
    // separate question, asked field by field in `noeta_check::SITE_POLICIES`: a fresh check numbers
    // the type-argument table from zero, so an index into it means nothing without its own run and
    // `absorb_type_args` has to rewrite it.
    Row(
        "sites",
        CheckerSites(Anchor(LOWER, "pub struct LoweringSites<'a> {")),
    ),
    // --- program-derived: exactly one, and this is it --------------------------------------------
    // Everything read off `program.stmts`. The anchor is the fold: a fragment's own (partial) facts
    // sit ON TOP of the enclosing program's, which is what the three historical bugs were missing.
    // Any NEW table of this kind belongs inside `ProgramFacts`, not beside it — see FACTS below,
    // which is where a table of this kind acquires its fragment test.
    Row(
        "facts",
        ProgramDerived(Anchor(LOWER, "facts: ambient.under(program, registry),")),
    ),
];

/// The second half: one row per field of `ProgramFacts`, naming the hot-swap test that watches a
/// **fragment** get that fact right. Every one of these tests fails against the pre-fix lowering, so
/// they are the behavioral counterpart to the structural rule above.
struct Fact(&'static str, Anchor);

const FACTS: &[Fact] = &[
    // `x is Uuid` in a swapped body, where the `use std.id.Uuid` is in the unchanged part.
    Fact(
        "type_aliases",
        Anchor(
            HOTSWAP,
            "fn a_swapped_body_narrows_against_an_unchanged_native_import(",
        ),
    ),
    // `type_of` / a reflection turbofish in a swapped body, naming a type the fragment never imports.
    Fact(
        "native_type_imports",
        Anchor(
            HOTSWAP,
            "fn a_swapped_body_reflects_an_unchanged_native_imports_qualified_name(",
        ),
    ),
    // An `@tier`-declared expression tier used in a swapped body — the panic that started this.
    Fact(
        "expr_tiers",
        Anchor(
            HOTSWAP,
            "fn a_swapped_body_keeps_an_expression_tier_from_the_program_that_declared_it(",
        ),
    ),
    // A swapped `async` body assigning a module global, which must stay a global store.
    Fact(
        "module_globals",
        Anchor(
            HOTSWAP,
            "fn a_swapped_async_body_still_sees_the_module_globals(",
        ),
    ),
    // A swapped body self-updating a value whose class — declared in the unchanged part — carries
    // its own `destruct`, which makes the update ineligible for in-place reuse. The one fact here
    // that a *pass over the lowered IR* reads rather than the lowerer, and the one whose absence
    // was not a panic or a wrong answer but a destructor that silently stopped running.
    Fact(
        "own_destructors",
        Anchor(
            HOTSWAP,
            "fn a_swapped_self_update_still_destroys_the_value_it_displaces(",
        ),
    ),
    // A swapped type's `From` conversions, each of which must keep the source-named method-table
    // key (`from<Cents>`) its call sites already resolved to. Unlike the four above, this table is
    // co-located with the declarations it names — a conversion's block travels with its target — so
    // the fold is the identity rather than a rescue. It is a fact all the same: it is read off
    // `program.stmts`, which is what puts it here rather than beside `facts` on the `Lowerer`.
    Fact(
        "from_conversion_keys",
        Anchor(
            HOTSWAP,
            "fn a_swapped_type_keeps_each_conversion_under_its_own_source(",
        ),
    ),
];

fn workspace_root() -> PathBuf {
    // crates/noeta-ir → crates → workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is reachable from CARGO_MANIFEST_DIR");
    assert!(
        root.join("Cargo.toml").is_file() && root.join("crates").is_dir(),
        "expected a workspace root at {}; this gate reads the tree's sources",
        root.display()
    );
    root
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("anchor file {} is unreadable: {e}", path.display()))
}

/// The field names of `struct <head>` in `src`, in declaration order. Unlike the ABI gate's
/// equivalent this accepts private fields — `Lowerer` has no public ones — so it keys off "an
/// identifier followed by a colon at the top level of the body" and skips doc comments, attributes
/// and blank lines.
fn fields(src: &str, head: &str) -> Vec<String> {
    let start = src
        .find(head)
        .unwrap_or_else(|| panic!("`{head}` not found — did the type move or get renamed?"))
        + head.len();
    let body = &src[start..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("no closing brace for `{head}`"));
    body[..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with('#'))
        .map(|l| l.strip_prefix("pub ").unwrap_or(l))
        .filter_map(|l| l.split_once(':'))
        .map(|(name, _)| name.trim().to_string())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .collect()
}

/// The `{ … }`-delimited body that follows `sig` in `src`, brace-matched — so a check on "what this
/// function does" cannot silently read the next function's text.
fn body_after(src: &str, sig: &str) -> String {
    let at = src
        .find(sig)
        .unwrap_or_else(|| panic!("`{sig}` not found — did it move or get renamed?"));
    let rest = &src[at..];
    let open = rest.find('{').expect("a body follows the signature");
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[open..=open + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after `{sig}`");
}

/// Adding a field to `Lowerer` must not be possible without saying what kind of state it is. This is
/// the half that fires at the commit which introduces one, rather than whenever someone next reads
/// the file and wonders.
#[test]
fn every_lowerer_field_is_classified() {
    let declared = fields(&read(LOWER), "struct Lowerer<'a> {");
    assert!(
        !declared.is_empty(),
        "the scanner read zero fields off `struct Lowerer<'a>` — it has gone stale against the \
         source, and a stale scanner passes everything. Fix the scanner, not the table."
    );

    let classified: Vec<String> = TABLE.iter().map(|r| r.field().to_string()).collect();

    let missing: Vec<_> = declared
        .iter()
        .filter(|d| !classified.contains(d))
        .collect();
    assert!(
        missing.is_empty(),
        "these `Lowerer` fields are not classified in TABLE: {missing:?}\n\
         Say what kind of state the field is — the question is really \"what does a FRAGMENT \
         lowering (a hot swap, a REPL entry) see in it?\":\n\
           - PerNode        — reset or scoped per node/frame. Name the reset site.\n\
           - Environment    — supplied by the caller. Name the `LowerOptions` field.\n\
           - CheckerSites   — the checker's whole-program hints. Name the bundle.\n\
           - ProgramDerived — read off the program. There is already exactly one (`facts`), and\n\
             that is the rule, not an accident: a second one is empty for every fragment, which is\n\
             how `@html` panicked, `x is Uuid` answered false, and a swapped `async` body blew up.\n\
             Put the table inside `ProgramFacts` instead, fold it in `under` and `absorb`, and give\n\
             it a hot-swap test in FACTS."
    );

    let stale: Vec<_> = classified
        .iter()
        .filter(|c| !declared.contains(c))
        .collect();
    assert!(
        stale.is_empty(),
        "TABLE classifies `Lowerer` fields that no longer exist (renamed or removed?): {stale:?}"
    );

    let mut seen = classified.clone();
    seen.sort();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "TABLE classifies a field twice");
}

/// Every classification must still be true of the tree. A "per-node" field whose reset site was
/// deleted is no longer per-node; an "environment" field whose option was renamed is no longer
/// threaded from the caller.
#[test]
fn every_classified_field_has_a_live_anchor() {
    for row in TABLE {
        let anchor = row.anchor();
        let src = read(anchor.file());
        assert!(
            src.contains(anchor.needle()),
            "`Lowerer::{}`: its anchor {:?} is gone from {} — re-point it after confirming the \
             classification still holds, or re-classify the field",
            row.field(),
            anchor.needle(),
            anchor.file()
        );
    }
}

/// **The singleton.** Exactly one field of `Lowerer` may be program-derived, and it is the
/// `ProgramFacts` bundle.
///
/// This is the rule the whole file exists for. A second program-derived field would be complete for
/// a whole-program lowering and empty for a fragment — and nothing about writing it would look
/// wrong, which is precisely why it happened three times before the bundle existed.
#[test]
fn exactly_one_lowerer_field_is_program_derived() {
    let derived: Vec<&str> = TABLE
        .iter()
        .filter(|r| matches!(r.kind(), Kind::ProgramDerived(_)))
        .map(|r| r.field())
        .collect();
    assert_eq!(
        derived,
        vec!["facts"],
        "exactly ONE `Lowerer` field may be program-derived, and it must be the `ProgramFacts` \
         bundle `facts`; found {derived:?}.\n\
         A second table read off `program.stmts` is empty for every FRAGMENT lowering — a hot-swap \
         install, a REPL entry — and there is no signal at the call site that it should not be. \
         That is how a swapped `@html` lowered to a panic, `x is Uuid` silently answered false, and \
         a swapped `async` body touching a module global panicked.\n\
         Move the table into `ProgramFacts`: fold it in `under` (does the fragment's own entry \
         shadow the enclosing program's, or do they union?) and in `absorb`, and add its hot-swap \
         test to FACTS."
    );

    // …and it really is the bundle, not merely named after it.
    let src = read(LOWER);
    assert!(
        body_after(&src, "struct Lowerer<'a> {").contains("facts: ProgramFacts,"),
        "`Lowerer::facts` is no longer a `ProgramFacts` — the singleton rule is about the bundle, \
         not the name"
    );
}

/// **The construction.** In `lower_with_sites_opts`, only `facts:` may mention `program`.
///
/// The singleton above catches a new program-derived field once someone classifies it. This catches
/// it one step earlier and without relying on anyone being honest: reading the program is a visible
/// act at the construction site, and exactly one initializer is allowed to perform it.
#[test]
fn only_the_program_facts_field_reads_the_program() {
    let src = read(LOWER);
    let opts = body_after(&src, "pub fn lower_with_sites_opts(");
    let literal = body_after(&opts, "let mut lowerer = Lowerer {");

    let offenders: Vec<&str> = literal
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with('{'))
        .filter(|l| l.contains("program"))
        .filter(|l| !l.starts_with("facts:"))
        .collect();
    assert!(
        offenders.is_empty(),
        "these `Lowerer` initializers read the program directly: {offenders:?}\n\
         Only `facts:` may — everything derived from `program.stmts` is empty for a FRAGMENT \
         lowering unless it goes through `ProgramFacts::under`, which folds the enclosing program's \
         copy underneath the fragment's own. Add the table to `ProgramFacts` instead."
    );
    assert!(
        literal.contains("facts: ambient.under(program, registry)"),
        "`facts` is no longer built by folding the caller's ambient facts under the program's own \
         — a fragment lowering has just lost everything its enclosing program knows"
    );
}

/// Every fact must be **folded** — by `under` (a fragment lowering) and by `absorb` (a session
/// accumulating entries). A table added to `ProgramFacts` but forgotten in either is back to the
/// pre-fix behavior for exactly the callers the bundle exists to serve, and nothing else would
/// notice: it is complete for a whole program, which is what the corpus runs.
#[test]
fn every_program_fact_is_folded_for_a_fragment() {
    let src = read(LOWER);
    let declared = fields(&src, "pub struct ProgramFacts {");
    assert!(
        !declared.is_empty(),
        "the scanner read zero fields off `ProgramFacts` — it has gone stale against the source"
    );

    let of = body_after(&src, "pub fn of(");
    let under = body_after(&src, "pub fn under(");
    let absorb = body_after(&src, "pub fn absorb(");
    for field in &declared {
        assert!(
            of.contains(field.as_str()),
            "`ProgramFacts::of` does not build `{field}` — the fact is declared and never read off \
             the program"
        );
        assert!(
            under.contains(field.as_str()),
            "`ProgramFacts::under` does not fold `{field}` — a FRAGMENT lowering (hot swap, REPL \
             entry) sees only what the fragment itself declares for it, which for `{field}` is \
             empty or partial. Decide the merge rule (does the fragment's entry shadow the \
             enclosing program's, or do they union?) and state it there."
        );
        assert!(
            absorb.contains(field.as_str()),
            "`ProgramFacts::absorb` does not fold `{field}` — a session that adds an entry loses \
             the fact for every later entry"
        );
    }
}

/// Every fact must have a **fragment test**: a hot-swap case that watches a swapped body get it
/// right. The structural rules above say where a fact must live; this says the fix is actually
/// observed, on the one path where getting it wrong is invisible to the conformance corpus (which
/// only ever lowers whole programs).
#[test]
fn every_program_fact_has_a_hot_swap_exerciser() {
    let declared = fields(&read(LOWER), "pub struct ProgramFacts {");
    let covered: Vec<&str> = FACTS.iter().map(|f| f.0).collect();

    let missing: Vec<_> = declared
        .iter()
        .filter(|d| !covered.contains(&d.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these `ProgramFacts` have no hot-swap exerciser in FACTS: {missing:?}\n\
         Add a case to {HOTSWAP} in which a swapped body depends on the fact and the fact lives in \
         the UNCHANGED part of the program — that is the shape all three historical bugs had, and \
         the shape the conformance corpus structurally cannot reach."
    );

    let stale: Vec<_> = covered
        .iter()
        .filter(|c| !declared.iter().any(|d| d == *c))
        .collect();
    assert!(
        stale.is_empty(),
        "FACTS names `ProgramFacts` fields that no longer exist: {stale:?}"
    );

    for Fact(field, anchor) in FACTS {
        let src = read(anchor.file());
        assert!(
            src.contains(anchor.needle()),
            "`ProgramFacts::{field}`: its hot-swap exerciser {:?} is gone from {} — the fragment \
             fix is in place and nothing watches it hold",
            anchor.needle(),
            anchor.file()
        );
    }
}

/// **The entry point.** Every construction of `LowerOptions` in the workspace must state `ambient`.
///
/// An empty `ambient` is not a neutral choice: it means "the code I am lowering IS the whole
/// program". That is right for a file compile and silently wrong for a fragment, so a *default* is a
/// hole a caller falls into rather than a decision it makes.
///
/// The `Default` impl this gate was written to compensate for **is gone** — retired at integration,
/// once the compiler refactor that shared the file had landed, in favour of the named
/// `LowerOptions::whole_program()`. So the type level now forces the answer, and what remains here
/// guards the ways it could quietly come back: a re-added `Default` impl, a literal completed with
/// `..Default::default()`, or one that simply never names the field.
#[test]
fn every_lower_options_construction_states_its_enclosing_program() {
    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();

    for path in rust_sources(&root.join("crates")) {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        // This gate's own prose names both spellings.
        if rel.ends_with("lowerer_field_census.rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        if !src.contains("LowerOptions") {
            continue;
        }

        // Struct literals: `LowerOptions { … }` must name `ambient`, and must not fill the rest in
        // from `Default` (which is exactly the empty-ambient hole, spelled shorter).
        for (at, _) in src.match_indices("LowerOptions {") {
            let preceding = src[..at].trim_end();
            // The declaration and its `Default` impl are not constructions.
            if preceding.ends_with("struct") || preceding.ends_with("for") {
                continue;
            }
            let literal = body_after(&src[at..], "LowerOptions {");
            if !literal.contains("ambient") {
                offenders.push(format!(
                    "{rel}: a `LowerOptions` literal that never names `ambient`"
                ));
            }
            if literal.contains("..Default::default()") {
                offenders.push(format!(
                    "{rel}: a `LowerOptions` literal completed with `..Default::default()`"
                ));
            }
        }

        // The hole, re-opened: a `Default` impl brings back exactly the empty-ambient default the
        // named `whole_program()` preset replaced, and every `..Default::default()` that follows it.
        if src.contains("impl Default for LowerOptions") {
            offenders.push(format!(
                "{rel}: a `Default` impl for `LowerOptions` — an empty `ambient` is a decision, not \
                 a default; use the named `whole_program()` preset"
            ));
        }
        if src.contains("LowerOptions::default()") {
            offenders.push(format!(
                "{rel}: `LowerOptions::default()` — the `Default` impl was retired; say \
                 `whole_program()` or state `ambient` explicitly"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "these sites configure a lowering without saying whether it is a whole program or a \
         fragment of one:\n  {}\n\
         `ambient` is what a FRAGMENT lowering (a hot swap, a REPL entry) sees of the program that \
         encloses it. Empty means \"this IS the whole program\" — true for every file-pipeline \
         compile and false, silently, for a fragment: an `@html` in a swapped body panics, `x is \
         Uuid` answers false, a swapped `async` body touching a global panics. Write \
         `ambient: ProgramFacts::default()` for a whole program (and say so), or pass the enclosing \
         program's facts.",
        offenders.join("\n  ")
    );
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Never a source of truth, and potentially enormous.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
