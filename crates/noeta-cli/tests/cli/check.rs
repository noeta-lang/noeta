//! `noeta check`: static analysis, no run/build.

use crate::support::*;

// --- `check` (static analysis, no run/build) ---------------------------------------

#[test]
fn check_clean_file_succeeds() {
    let file = temp_program(
        "check_clean",
        "fn add(a: int, b: int): int { return a + b }\necho add(2, 3)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
}

#[test]
fn check_type_error_exits_1() {
    let file = temp_program("check_type_err", "echo 1 + true\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("1 error(s)"));
}

#[test]
fn check_syntax_error_exits_1() {
    let file = temp_program("check_syntax_err", "echo $;\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0001"));
}

#[test]
fn check_directory_is_recursive_and_attributes_errors_to_files() {
    // A clean file at the root and an erroring file in a subdirectory: the recursive walk finds both,
    // the directory check fails, and the error renders against the nested file.
    let dir = temp_dir(
        "check_tree",
        &[
            ("a.noe", "fn ok(): int { return 1 }\n"),
            ("sub/bad.noe", "echo 1 + true\n"),
        ],
    );
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("bad.noe"))
        .stderr(predicate::str::contains("2 files"));
}

#[test]
fn check_shared_erroring_module_is_reported_once() {
    // `m.noe` has one error and is imported by two entries (and is itself an entry in the walk), so it
    // is linked/checked three times — but global dedup means the diagnostic is rendered exactly once.
    let dir = temp_dir(
        "check_shared",
        &[
            (
                "m.noe",
                "namespace App.M;\npub fn boom(): int { return 1 + true }\n",
            ),
            ("main1.noe", "use App.M.{boom}\necho boom()\n"),
            ("main2.noe", "use App.M.{boom}\necho boom()\n"),
        ],
    );
    let out = lang().arg("check").arg(&dir).assert().failure().code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert_eq!(
        stderr.matches("E0007").count(),
        1,
        "the shared module's error is deduplicated to a single rendering:\n{stderr}"
    );
    assert!(stderr.contains("1 error(s)"), "{stderr}");
}

#[test]
fn bare_relative_entry_still_links_siblings() {
    // Regression (multi-file impact arc): an entry given as a bare relative filename
    // (`noeta check main.noe` run FROM the project directory) has parent `""`, and
    // `read_dir("")` errors — the sibling scan silently came up empty and the import failed
    // E0019 while the byte-equivalent `./main.noe` linked fine.
    let dir = temp_dir(
        "bare_relative_siblings",
        &[
            (
                "m.noe",
                "namespace App.M;\npub fn boom(): int { return 7 }\n",
            ),
            ("main.noe", "use App.M.boom;\necho boom()\n"),
        ],
    );
    lang()
        .arg("check")
        .arg("main.noe")
        .current_dir(&dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
}

#[test]
fn a_cross_module_coherence_conflict_renders_both_files() {
    // E0027 across a module boundary must be *locatable*. It used to name only the later impl and
    // say the other was "already implemented above" — pointing the reader at a file that does not
    // contain it. Both sites are labelled now, and the multi-file `ariadne` report renders each
    // against its own source.
    let dir = temp_dir(
        "coherence_two_sites",
        &[
            (
                "types.noe",
                "namespace pkg.types;\npub trait Decoder { fn step(): string }\n\
                 pub class Target { pub fn new(): Target { return Target {} } }\n",
            ),
            (
                "first.noe",
                "namespace pkg.first;\nuse pkg.types.{Decoder, Target};\n\
                 impl Decoder for Target { pub fn step(): string { return \"first\" } }\n",
            ),
            (
                "second.noe",
                "namespace pkg.second;\nuse pkg.types.{Decoder, Target};\n\
                 impl Decoder for Target { pub fn step(): string { return \"second\" } }\n",
            ),
            (
                "main.noe",
                "namespace pkg.main;\nuse pkg.types.{Decoder, Target};\n\
                 fn want(x: dyn Decoder): string { return x.step() }\necho want(Target.new())\n",
            ),
        ],
    );
    let out = lang()
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E0027"), "{stderr}");
    assert!(
        stderr.contains("first.noe") && stderr.contains("second.noe"),
        "both competing implementations are located:\n{stderr}"
    );
    assert!(
        stderr.contains("first implemented here"),
        "the earlier impl carries a secondary label:\n{stderr}"
    );
    assert!(
        !stderr.contains("above"),
        "the positional wording is gone — the other site is in another file:\n{stderr}"
    );
}

#[test]
fn check_empty_directory_exits_2() {
    let dir = temp_dir("check_empty", &[]);
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no `.noe` files"));
}

#[test]
fn check_json_emits_a_machine_readable_report_on_stdout() {
    let file = temp_program("check_json_err", "echo 1 + true\n");
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        // The report goes to stdout; stderr carries no human diagnostics in JSON mode.
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["files_checked"], 1);
    assert_eq!(report["errors"], 1);
    assert_eq!(report["warnings"], 0);
    let diags = report["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "E0007");
    assert_eq!(diags[0]["severity"], "error");
    assert_eq!(diags[0]["line"], 1);
    assert!(diags[0]["file"].as_str().unwrap().ends_with("main.noe"));
}

#[test]
fn check_json_clean_is_an_empty_diagnostics_array() {
    let file = temp_program(
        "check_json_ok",
        "fn id(n: int): int { return n }\necho id(1)\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["errors"], 0);
    assert!(report["diagnostics"].as_array().unwrap().is_empty());
}

// --- `check` covers dev-tier blocks (a green check must not precede a red compile) -------------

/// The trap this closes, verbatim: a `@test` body that does not compile used to check clean, because
/// the baseline build strips every tier block before the checker sees it. `noeta check` now checks
/// each file once as it ships *and* once per code tier its own blocks name.
#[test]
fn check_reports_a_type_error_inside_a_test_block() {
    let file = temp_program(
        "check_tier_test_err",
        "fn ok_fn(): Result<void, string> {\n    return Ok()\n}\n\necho \"hi\"\n\n@test {\n    fn broken(): void {\n        assert(ok_fn() is Ok)\n    }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        // `Ok` is a `Result`'s value, not a type — E0013, exactly what `noeta test` reports.
        .stderr(predicate::str::contains("E0013"))
        .stderr(predicate::str::contains("(tiers: test)"));
}

/// A `@debug` block in statement position is code too, and has no dedicated command at all — the
/// only report its body would otherwise get is somebody running with `--target development`.
#[test]
fn check_reports_a_type_error_inside_a_debug_block() {
    let file = temp_program(
        "check_tier_debug_err",
        "fn f(x: int): void {\n    @debug { echo x + true }\n}\nf(1)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("(tiers: debug)"));
}

/// A clean file's summary names what was looked inside, so silence stops being ambiguous — and a
/// file with no tier block says nothing extra.
#[test]
fn check_summary_names_the_tiers_it_covered() {
    let with_tier = temp_program(
        "check_tier_summary",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn adds(): void { assert(add(1, 2) == 3) }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&with_tier)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "checked 1 file (tiers: test): 0 error(s), 0 warning(s)",
        ));

    let without = temp_program("check_tier_summary_none", "echo 1\n");
    lang()
        .arg("check")
        .arg(&without)
        .assert()
        .success()
        .stderr(predicate::str::contains("checked 1 file: 0 error(s)"));
}

/// One tier per pass, never all at once. No build compiles `@test` and `@bench` together, so two
/// same-named helpers in two different tiers are not a collision and must not be reported as one.
#[test]
fn check_never_conflates_two_tiers_into_one_program() {
    let file = temp_program(
        "check_tier_no_conflate",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn helper(): int { return 1 }\n    fn adds(): void { assert(add(helper(), 2) == 3) }\n}\n\n@bench {\n    fn helper(): int { return 2 }\n    fn adding(): void { echo add(helper(), 2) }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("(tiers: bench, test)"))
        .stderr(predicate::str::contains("0 error(s)"));
}

/// The JSON report carries the same list, so CI and the editor see what the terminal does.
#[test]
fn check_json_reports_the_tiers_it_covered() {
    let file = temp_program(
        "check_tier_json",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn adds(): void { assert(add(1, 2) == 3) }\n}\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["tiers_checked"], serde_json::json!(["test"]));
}

/// An error *outside* every tier block is reported by every shape — the shipping one and each tier's
/// — and must still print exactly once.
#[test]
fn check_does_not_duplicate_a_diagnostic_across_shapes() {
    let file = temp_program(
        "check_tier_dedup",
        "echo 1 + true\n\n@test {\n    fn t(): void { assert(true) }\n}\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .failure()
        .code(1);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["errors"], 1);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
}

// --- diagnostic *wording* (the corpus header pins a code and a span, never a message) -------------

/// A `destruct` block on a `struct` must state the **rule** — that destructors are class-only — and
/// not merely that `destruct` is a reserved word.
///
/// The reader's mistake is not a syntax error and not a typo: they wrote a member the grammar knows,
/// spelled correctly, on the wrong kind of declaration. "`destruct` cannot be used as a name … rename
/// it to `destruct_`" answered a question nobody asked. The conformance case beside this one pins
/// `E0079` at the block; only a text assertion can pin what the code is *for*.
#[test]
fn a_destruct_block_on_a_struct_names_the_class_only_rule() {
    let file = temp_program(
        "check_struct_destruct",
        "struct Point {\n    x: int\n    destruct { echo \"gone\" }\n}\necho Point { x: 1 }.x\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0079"))
        .stderr(predicate::str::contains(
            "a `destruct` block is class-only, and `Point` is a struct",
        ))
        .stderr(predicate::str::contains("reserved").not());
}

/// `dyn` on a built-in trait with no trait object is refused **at the annotation**, naming the trait.
#[test]
fn dyn_on_a_trait_without_an_object_names_the_trait() {
    let file = temp_program(
        "check_dyn_marker_trait",
        "fn f(x: dyn Clone): int { return 1 }\necho 0\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0014"))
        .stderr(predicate::str::contains(
            "`Clone` has no trait object, so `dyn Clone` names no type",
        ));
}

/// **A derived trait's method resolves at `check` time**, so the editor never underlines a call that
/// runs. `traits_of` reports `Display` for this type and `x is dyn Display` is `true`; a checker that
/// refused `x.to_string()` would be contradicting both.
///
/// The absent string is the assertion that matters: "has no method" is what the reader used to be
/// shown for a member their own `@derive` put there.
#[test]
fn a_derived_builtin_trait_method_is_not_reported_missing() {
    let file = temp_program(
        "check_derived_builtin_method",
        "@derive(Display, Equatable, Comparable)\nstruct Tag {\n    n: int\n}\n\
         t = Tag { n: 1 }\necho t.to_string()\necho t.eq(t)\necho t.compare(Tag { n: 2 })\n\
         fn render(x: dyn Display): string { return x.to_string() }\necho render(t)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("has no method").not());
}

/// **`compare` on a type that does not implement `Comparable` is refused where it is written**, and
/// the message says which requirement went unmet.
///
/// `compare` is the one method every ordered built-in answers, so a receiver-agnostic reading of it
/// typed the call as an `Ordering` on *any* receiver — which told the checker the member existed and
/// left the program to abort at run time (`E0005`) on a type with no ordering, while the sibling
/// door `t.eq(u)` was refused statically. The absent `E0005` is what pins the fix: the same mistake
/// must not be reachable through two codes at two different times.
///
/// The wording is the second half, and it is why this is a CLI test rather than a corpus case (the
/// `// expect:` grammar pins a code and a position, not a text). "Has no method" is the wrong story
/// for `compare`: the method is `Comparable`'s and the reader's mistake is not a typo but a missing
/// ordering, which is also exactly what `a < b` would tell them — so the diagnostic names the
/// requirement and the operator, and must NOT read as a spelling mistake.
#[test]
fn compare_on_an_unordered_user_type_is_refused_statically() {
    let file = temp_program(
        "check_compare_unordered",
        "struct Plain {\n    n: int\n}\necho Plain { n: 1 }.compare(Plain { n: 2 })\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(
            "`compare()` needs an ordered receiver, but `Plain` has no ordering",
        ))
        .stderr(predicate::str::contains("@derive(Comparable)"))
        .stderr(predicate::str::contains("type `Plain` has no method `compare`").not())
        .stderr(predicate::str::contains("E0005").not());
}

/// **An unordered NATIVE type is refused at `.compare()` too**, from its own ABI declaration.
///
/// The sibling above is a type the program declares, whose member set the checker closes. A native
/// type's member set stays open — a registry miss is not proof of absence — so it takes the trait
/// declaration instead, which is the same authority `a < b` reads and is authoritative for exactly
/// this question. `Duration` is the case: it renders, and deliberately declares no ordering,
/// because a calendar span has none without a reference date.
///
/// Asserted here rather than in the corpus for the same reason as the sibling — and with the
/// **absent** string carrying the point: a `Duration` must not be reported as lacking a member,
/// because the member set is not what decides it.
#[test]
fn compare_on_an_unordered_native_type_is_refused_statically() {
    let file = temp_program(
        "check_compare_unordered_native",
        "use std.{datetime}\nt = datetime.from_unix_ms(0)\n\
         a = t.diff(t.add(datetime.hours(1)))\n\
         echo a.compare(t.diff(t.add(datetime.hours(2))))\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(
            "`compare()` needs an ordered receiver, but `Duration` has no ordering",
        ))
        .stderr(predicate::str::contains("has no method").not())
        .stderr(predicate::str::contains("E0005").not());
}

/// **A union refused for having no ordering BETWEEN its members says so**, and does not offer the
/// repairs that fit a single type.
///
/// `int | string` is the shape that reads as a contradiction if the wording is generic: both members
/// order perfectly well, so "`int | string` has no ordering" invites the reader to argue. What is
/// missing is an ordering *across* members — only numbers compare across types — and the repair is
/// to narrow, not to declare anything.
///
/// The **absent** strings carry the point, and they are why this is a CLI test rather than a corpus
/// case (the `// expect:` grammar pins a code and a position, never a text). `@derive(Comparable)`
/// and `impl Comparable` are the two repairs the general diagnostic names, and neither can be
/// applied to a union — offering them sends the reader to write something the language will not
/// accept.
#[test]
fn a_union_whose_members_do_not_pair_says_ordering_is_undefined_across_them() {
    let file = temp_program(
        "check_compare_union_unpaired",
        "fn f(x: int | string): Ordering { return x.compare(1) }\necho \"ok\"\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(
            "ordering is not defined for the union `int | string`",
        ))
        .stderr(predicate::str::contains(
            "only numbers compare across types",
        ))
        .stderr(predicate::str::contains("@derive(Comparable)").not())
        .stderr(predicate::str::contains("impl Comparable").not())
        .stderr(predicate::str::contains("has no method").not())
        .stderr(predicate::str::contains("E0005").not());
}

/// **A union refused for holding an unordered member names that member.**
///
/// The sibling above fails the pairing rule; this one fails the other half, and the reader's repair
/// is different — the union is not the problem, `List<int>` is. So the member is named, and the
/// "no ordering between them" story that fits the sibling must NOT appear here, because it would
/// point at the wrong thing entirely.
#[test]
fn a_union_holding_an_unordered_member_names_the_member() {
    let file = temp_program(
        "check_compare_union_unordered_member",
        "fn f(x: int | List<int>): Ordering { return x.compare(1) }\necho \"ok\"\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(
            "ordering is not defined for the union `int | List<int>`",
        ))
        .stderr(predicate::str::contains(
            "`List<int>` has no ordering, so the union has none either",
        ))
        .stderr(predicate::str::contains("only numbers compare across types").not())
        .stderr(predicate::str::contains("has no method").not())
        .stderr(predicate::str::contains("E0005").not());
}

/// **`number` orders, at every door.**
///
/// `number` is a union — `int | float | f32 | … | u64`, written short — so the union rule is what
/// decides whether a `number` can be compared, sorted, reduced or passed to a `Comparable` bound.
/// All five doors are here in one program because they are reached by different machinery and a
/// repair to one does not imply the others.
///
/// The absent `E0025` is the bound door specifically: a union that does not satisfy `Comparable`
/// fails there with a different code from the other four, so a check for `E0007` alone would pass
/// while `biggest(a, b)` stayed refused.
#[test]
fn number_is_an_ordered_type_at_every_door() {
    let file = temp_program(
        "check_number_orders",
        "fn biggest<T: Comparable>(x: T, y: T): T { return if x < y then y else x }\n\
         fn f(x: number, y: number): bool { return x < y }\n\
         fn g(x: number, y: number): Ordering { return x.compare(y) }\n\
         fn h(xs: List<number>): List<number> { return xs.sorted() }\n\
         fn i(xs: List<number>): ?number { return xs.min() }\n\
         fn j(xs: List<number>): ?number { return xs.max() }\n\
         fn k(x: number, y: number): number { return biggest(x, y) }\n\
         echo \"ok\"\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("E0007").not())
        .stderr(predicate::str::contains("E0025").not())
        .stderr(predicate::str::contains("does not satisfy the bound").not())
        .stderr(predicate::str::contains("needs an ordered element type").not());
}

/// **A refused generic widening names the occurrence that forced it.**
///
/// `Slot<Dog>` and `Slot<dyn Speak>` look related, and they are: the only thing standing between
/// them is where `Slot` puts its type parameter. A bare "expected … found …" therefore reads as
/// "trait objects do not work in a generic", which is the wrong lesson — the *same* widening is
/// accepted for a declaration that only ever reads the parameter out.
///
/// Asserted here rather than in the corpus because the `// expect:` grammar pins codes and spans,
/// never diagnostic text. The **absent** string carries as much as the present one: the message
/// must not be the bare mismatch alone.
#[test]
fn a_refused_trait_object_widening_names_the_occurrence() {
    let file = temp_program(
        "check_variance_cause",
        "trait Speak { fn speak(): string }\n\
         struct Dog { impl Speak { pub fn speak(): string { return \"woof\" } } }\n\
         class Slot<T> { pub mut v: T }\n\
         fn widen(s: Slot<Dog>): string {\n\
         wide: Slot<dyn Speak> = s\n\
         return wide.v.speak()\n\
         }\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(
            "`Slot` stores it in the `mut` field `v`",
        ))
        .stderr(predicate::str::contains(
            "cannot be read as a `Slot<dyn Speak>`",
        ))
        .stderr(predicate::str::contains(
            "a literal checked against `Slot<dyn Speak>` instantiates its type argument at \
             `dyn Speak` directly",
        ));
}

/// The **covariant** twin of the case above: the same widening, on a declaration that only reads
/// the parameter out, is accepted — and says nothing at all.
///
/// It is the control that makes the refusal above mean something. Without it, a rule that refused
/// *every* generic widening would pass the sibling test with an equally precise message.
#[test]
fn a_read_only_generic_widens_to_a_trait_object_silently() {
    let file = temp_program(
        "check_variance_ok",
        "trait Speak { fn speak(): string }\n\
         struct Dog { impl Speak { pub fn speak(): string { return \"woof\" } } }\n\
         class Reader<T> { pub v: T }\n\
         r: Reader<Dog> = Reader { v: Dog {} }\n\
         wide: Reader<dyn Speak> = r\n\
         echo wide.v.speak()\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"))
        .stderr(predicate::str::contains("cannot be read as").not());
}

/// E0009's wording, which the conformance header cannot pin: it grammars only the code and span.
///
/// `check` decides an incomplete literal wherever the literal's type is known, so the diagnostic
/// arrives without the program running — the field values here would abort long before a
/// construction-time check saw them, and no `boom` reaches stdout.
#[test]
fn check_reports_a_literal_that_leaves_a_field_unset() {
    let file = temp_program(
        "check_missing_field",
        "struct Point { x: int  y: int }\n\
         fn unused(): Point { return Point { x: 1 } }\n\
         echo \"boom\"\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0009"))
        .stderr(predicate::str::contains(
            "missing field(s) `y` in `Point` literal — every field must be set",
        ))
        .stderr(predicate::str::contains(
            "give every field a value, spread one from another instance with `...base`, or declare \
             a default on the field (`name: T = …`)",
        ))
        .stderr(predicate::str::contains("boom").not());
}

/// The construction-time half of the same rule, in the same words. A `...base` spread of a `dyn`
/// value fills whichever slots the value turns out to have, so the literal is decided when it is
/// built; a reader who meets E0009 there and then meets it from `check` must not have to work out
/// whether they are the same rule.
#[test]
fn a_deferred_literal_reports_the_same_missing_field_at_construction() {
    let file = temp_program(
        "run_missing_field_dyn",
        "struct Point { x: int  y: int }\n\
         struct Flat { x: int }\n\
         fn opaque(): dyn { return Flat { x: 1 } }\n\
         echo \"alive\"\n\
         echo Point { ...opaque() }.y\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("alive"))
        .stderr(predicate::str::contains(
            "missing field(s) `y` in `Point` literal — every field must be set",
        ));
}
