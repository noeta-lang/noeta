//! The crate-root unit-test module, moved verbatim (dedented one level) out of
//! `lib.rs`. Kept as ONE file rather than split by subject because the tests
//! share a web of local helper fns (`run`, `run_traced`, `fragment`,
//! `debug_session_vm`, `compile_module`, `peak_residency`, ...) — a by-subject
//! split would duplicate or re-plumb them, which is churn beyond the
//! verbatim-move pattern.

use super::*;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

/// P-AOT L3.2b drift guard: the `#[export_name]` literals on the JIT helpers (which an AOT
/// program object links against) must equal the `noeta_jit_abi::*_HELPER` name constants the JIT
/// declares its imports under. The literals are hardcoded (an attribute needs a string literal,
/// not a const), so this asserts they still agree — a changed constant fails here, flagging the
/// export attributes to update in lockstep.
#[cfg(feature = "jit")]
#[test]
fn aot_helper_export_names_match_the_jit_constants() {
    assert_eq!(noeta_jit_abi::OBSERVE_HELPER, "noeta_jit_observe");
    assert_eq!(
        noeta_jit_abi::NOTE_GLOBAL_BOUND_HELPER,
        "noeta_jit_note_global_bound"
    );
    assert_eq!(noeta_jit_abi::RETAIN_HELPER, "noeta_jit_retain");
    assert_eq!(noeta_jit_abi::RELEASE_HELPER, "noeta_jit_release");
    assert_eq!(
        noeta_jit_abi::RELEASE_VALUE_HELPER,
        "noeta_jit_release_value"
    );
    assert_eq!(noeta_jit_abi::CALL_HELPER, "noeta_jit_call");
    assert_eq!(noeta_jit_abi::RETURN_HELPER, "noeta_jit_return");
    assert_eq!(noeta_jit_abi::PREPARE_CALL_HELPER, "noeta_jit_prepare_call");
    assert_eq!(noeta_jit_abi::AFTER_CALL_HELPER, "noeta_jit_after_call");
    assert_eq!(noeta_jit_abi::LEAF_OP_HELPER, "noeta_jit_run_leaf_op");
}

fn run(src: &str) -> RunResult {
    // Seed the process-default registry explicitly — this suite is an assembling binary (the
    // registry doc's own instruction), and relying on a sibling test to have seeded first is a
    // scheduling race. Runs ON the caller's thread: several tests measure `noeta_value`'s
    // thread-local residency counters around this call, so the run must share their thread (a
    // test whose program out-recurses the 2 MiB debug test stack wraps ITSELF in
    // `on_deep_stack` instead — see `record_reassign_reuse_paths`).
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    VmBackend::new()
        .try_run(&parsed.program)
        .expect("program should be in the M1.0 subset")
}

/// Run `f` on a deep worker stack — for a test whose front-end recursion (checker
/// `check`/`synth`, the reuse/drops passes) out-recurses libtest's ~2 MiB debug test thread;
/// the conformance corpus's `on_deep_stack` precedent. Thread-local counters measured inside
/// `f` stay consistent because the whole test body moves to the worker.
fn on_deep_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, f)
            .expect("spawn deep-stack test worker")
            .join()
        {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// P-AOT L3.2b: prove the dispatch-table binding + native dispatch **in-process**, isolating the
/// linker as the only remaining unknown for a real AOT binary. Force-JIT a hot call-free loop,
/// harvest its finalized entry pointer into an [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`]-shaped table,
/// then run a *fresh* VM bound to that table with the compiler unarmed (`vm.aot = true`, `jit`
/// stays `None`). The native entry must actually run — not interpret — and match the tier-0
/// output. A call-free body keeps the harvested entry self-contained (no per-site inline caches
/// to share across VMs); the call path is covered corpus-wide by the `NOETA_JIT_AOT` oracle.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
#[test]
fn aot_bound_dispatch_runs_native_in_process() {
    let src = "mut t = 0\nfor i in 0..2000 { t = t + i * i }\necho t\n";
    let source = Source::new(SourceId::FIRST, "aot.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("compiles");
    let expected = VmBackend::new().run_module(&module).stdout;

    // Harvest a dispatch table from a force-JIT VM. That VM owns the finalized code pages, so it
    // is kept alive (`keep`) across the AOT run below.
    let mut keep = Vm::load(
        &module,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
    );
    keep.tier1.force_jit = true;
    keep.init_jit();
    let n = keep.tier1.jit_entries.len();
    assert!(
        keep.tier1.jit_entries.iter().any(Option::is_some),
        "at least one prototype went native"
    );
    let mut table = vec![0usize; 1 + 2 * n];
    table[0] = n;
    for p in 0..n {
        if let Some(f) = keep.tier1.jit_entries[p] {
            table[1 + 2 * p] = f as usize;
        }
        if let Some(ff) = keep.tier1.jit_fast[p] {
            table[1 + 2 * p + 1] = ff;
        }
    }

    // Fresh VM, compiler unarmed, bound to the harvested AOT table.
    let mut vm = Vm::load(
        &module,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
    );
    vm.tier1.aot = true;
    assert!(vm.tier1.jit.is_none(), "the AOT VM arms no compiler");
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    unsafe { vm.bind_aot_dispatch(table.as_ptr()) };
    let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
    assert_eq!(
        result.stdout, expected,
        "AOT-bound native run matches tier-0"
    );
    drop(keep); // hold the code pages live until the AOT run has finished
}

/// P-AOT L3.2b(3): [`compile_module_aot`] wires the object backend end-to-end — it emits a
/// relocatable object carrying the [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`] table. Byte-identity of the
/// native codegen itself is proven corpus-wide by the `NOETA_JIT_AOT` oracle; this asserts the
/// object is produced, is non-trivial, and defines the dispatch symbol (its name lands in the
/// object's string table as raw ASCII — a dependency-free way to see the table was emitted).
#[cfg(feature = "jit")]
#[test]
fn compile_module_aot_emits_a_linkable_object_with_the_dispatch_table() {
    let src = "mut t = 0\nfor i in 0..2000 { t = t + i * i }\necho t\n";
    let source = Source::new(SourceId::FIRST, "aot.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("compiles");

    let obj = compile_module_aot(&module).expect("emits an object");
    assert!(obj.len() > 64, "object carries real content");
    let needle = noeta_jit_abi::AOT_DISPATCH_SYMBOL.as_bytes();
    assert!(
        obj.windows(needle.len()).any(|w| w == needle),
        "the dispatch symbol name appears in the object"
    );
}

/// Run a source program through the sandboxed traced entry, returning the result + traceback.
fn run_traced(src: &str) -> (RunResult, Vec<TraceFrame>) {
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("program should compile");
    VmBackend::new().run_module_traced(&module)
}

/// Parse a fragment the way a debug console would (statements allowed; no checker).
fn fragment(src: &str) -> Program {
    let source = Source::new(SourceId(1), "<console>", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "fragment should parse cleanly: {src:?}"
    );
    parsed.program
}

/// Build a **session-adopted debug Vm**: the checked program compiled with the compiler kept
/// alive (T3), the module arena'd, and the [`DebugSession`] installed — the debug console's
/// launch shape. Returns the Vm ready to `run_top` entry 0.
fn debug_session_vm<'a>(arena: &'a typed_arena::Arena<Module>, src: &str) -> Vm<'a> {
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "program should parse cleanly"
    );
    let checked = noeta_check::check_all(&parsed.program);
    assert!(
        checked.diagnostics.is_empty(),
        "program should check cleanly: {:?}",
        checked.diagnostics
    );
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&parsed.program, checked.sites, false, true)
            .expect("a checked program compiles");
    let module: &Module = arena.alloc(module);
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    let mut vm = Vm::load(
        module,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
    );
    vm.debug_session = Some(DebugSession {
        compiler: Box::new(compiler),
        arena,
        memo: HashMap::new(),
        result_memo: HashMap::new(),
        stop_generation: 0,
    });
    vm
}

/// T4 (tooling-unification): a fragment installed into a *running* Vm executes against the
/// swapped extended module — calling the program's functions and reading its globals by their
/// original ids — and code the fragment defines (a closure bound into a global) stays callable
/// by LATER code, including the program's own functions, after further installs.
#[test]
fn installed_fragments_extend_a_running_debug_vm() {
    let before = noeta_value::live_count();
    let arena = typed_arena::Arena::new();
    let mut vm = debug_session_vm(
        &arena,
        "struct P { x: int }\n\
         fn twice(n: int): int { return n * 2 }\n\
         fn callcb(n: int) use (cb): int { return cb(n) }\n\
         mut cb = fn(n: int) => n\n\
         mut base = 10\n\
         mut p0 = P { x: 3 }\n\
         echo twice(base)\n",
    );
    vm.run_top();
    assert_eq!(vm.out.stdout, "20\n");

    // Fragment 1: calls the program's fn + global by their original ids.
    let entry = vm
        .install_fragment(&fragment("echo twice(base + 1);"))
        .expect("fragment compiles");
    let Ok(v) = vm.run_thunk(entry, &[]) else {
        panic!("fragment runs: {:?}", vm.out.diagnostics);
    };
    release(v);
    assert_eq!(vm.out.stdout, "20\n22\n");

    // Fragment 2: constructs the program's struct; interned-shape identity makes it equal to
    // the value entry 0 built.
    let entry = vm
        .install_fragment(&fragment("echo p0 == P { x: 3 };"))
        .expect("fragment compiles");
    let Ok(v) = vm.run_thunk(entry, &[]) else {
        panic!("fragment runs: {:?}", vm.out.diagnostics);
    };
    release(v);
    assert_eq!(vm.out.stdout, "20\n22\ntrue\n");

    // Fragment 3: ESCAPE — rebind the program's callback global to a fragment-defined closure
    // (a proto index that only exists in the extended module).
    let entry = vm
        .install_fragment(&fragment("cb = fn(n: int) => twice(n) + base;"))
        .expect("fragment compiles");
    let Ok(v) = vm.run_thunk(entry, &[]) else {
        panic!("fragment runs: {:?}", vm.out.diagnostics);
    };
    release(v);

    // Fragment 4: the PROGRAM's own function (old-module code) calls the escaped closure — the
    // dispatch resolves its fragment proto through the newest module at the frame transfer.
    let entry = vm
        .install_fragment(&fragment("echo callcb(4);"))
        .expect("fragment compiles");
    let Ok(v) = vm.run_thunk(entry, &[]) else {
        panic!("fragment runs: {:?}", vm.out.diagnostics);
    };
    release(v);
    assert_eq!(vm.out.stdout, "20\n22\ntrue\n18\n");

    // A fragment that ABORTS unwinds cleanly through the swapped module (the release loops
    // resolve every frame's proto against the newest snapshot) and pollutes nothing.
    let entry = vm
        .install_fragment(&fragment("echo [1][5];"))
        .expect("fragment compiles");
    assert!(vm.run_thunk(entry, &[]).is_err(), "out of bounds aborts");
    vm.out.diagnostics.clear();
    vm.out.abort_trace.clear();

    // Teardown drains everything; residency returns to the baseline (no leaked fragment values).
    let result = vm.teardown(noeta_value::CollectorMode::Trace);
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        noeta_value::live_count(),
        before,
        "teardown after fragment installs returns residency to baseline"
    );
}

/// M6b (MCP arc): a runaway console fragment (`while true { … }`) is STOPPED by the
/// evaluation budget — the nested run executes with the session debugger held out of `self`,
/// so without the budget nothing could interrupt it and the paused session hung forever. The
/// trip is an ordinary nested abort: the paused program is untouched and the very next
/// fragment evaluates normally.
#[test]
fn console_fragment_evaluation_is_bounded() {
    let arena = typed_arena::Arena::new();
    let mut vm = debug_session_vm(&arena, "mut base = 10\necho base\n");
    vm.run_top();
    let frames = vec![Frame {
        proto: 0,
        base: 0,
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    }];
    let regs = vec![Value::unit(); vm.module.protos[0].num_registers as usize];

    let text = "mut i = 0\nwhile true { i = i + 1 }";
    let program = fragment(text);
    let DebugEvalOutcome::Error(message) =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("a runaway fragment must be stopped, not hang");
    };
    assert!(message.contains("budget"), "got: {message}");

    // The session survives the trip: a follow-up fragment evaluates against intact state.
    let text = "base + 1";
    let program = fragment(text);
    let DebugEvalOutcome::Value { text: v, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("the session must survive a budget trip");
    };
    assert_eq!(v, "11");
}

/// Watch-memoization: an *observational* watch (`tick()`) has its rendered result memoized within
/// a stop — a repeated render returns the cached value WITHOUT re-running the fragment (visible
/// because `tick()` mutates a global on each real run) — and bumping the stop generation
/// invalidates the memo so the next render re-evaluates.
#[test]
fn observational_watch_result_is_memoized_until_the_generation_bumps() {
    let arena = typed_arena::Arena::new();
    let mut vm = debug_session_vm(
        &arena,
        "mut counter = 0\nfn tick() use (counter): int { counter = counter + 1\nreturn counter }\necho counter\n",
    );
    vm.run_top();
    let frames = vec![Frame {
        proto: 0,
        base: 0,
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    }];
    let regs = vec![Value::unit(); vm.module.protos[0].num_registers as usize];

    let text = "tick()";
    let program = fragment(text);
    // First watch render runs `tick()` (counter 0 → 1).
    let DebugEvalOutcome::Value { text: v1, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Watch, text, &frames, &regs)
    else {
        panic!("first watch eval should succeed");
    };
    assert_eq!(v1, "1");
    // A repeated render at the same generation is a memo HIT — `tick()` does not run, so the value
    // stays 1 rather than advancing to 2.
    let DebugEvalOutcome::Value { text: v2, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Watch, text, &frames, &regs)
    else {
        panic!("second watch eval should succeed");
    };
    assert_eq!(v2, "1", "a memoized watch must not re-run its fragment");

    // Bumping the generation (what a resume/step or a console mutation does) invalidates the memo:
    // the next render re-runs `tick()` (counter 1 → 2).
    vm.bump_stop_generation();
    let DebugEvalOutcome::Value { text: v3, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Watch, text, &frames, &regs)
    else {
        panic!("third watch eval should succeed");
    };
    assert_eq!(v3, "2", "a generation bump invalidates the watch memo");

    // A CONSOLE entry is never memoized and always re-runs — each call advances the counter.
    let DebugEvalOutcome::Value { text: c1, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("console eval should succeed");
    };
    assert_eq!(c1, "3");
    let DebugEvalOutcome::Value { text: c2, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("console eval should succeed");
    };
    assert_eq!(
        c2, "4",
        "a console entry is not memoized — it re-runs every time"
    );
}

/// U3 (tooling-unification): a re-evaluated fragment — same text, same scope shape — reuses its
/// compiled wrapper instead of appending a fresh proto + slot to the session per step.
#[test]
fn watch_fragments_are_memoized_by_text_and_scope() {
    let arena = typed_arena::Arena::new();
    let mut vm = debug_session_vm(
        &arena,
        "fn twice(n: int): int { return n * 2 }\nmut base = 10\necho twice(base)\n",
    );
    vm.run_top();
    // Fabricate the paused shape the trampoline sees: main's frame at its entry (no in-scope
    // locals yet), over a scratch register window.
    let frames = vec![Frame {
        proto: 0,
        base: 0,
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    }];
    let regs = vec![Value::unit(); vm.module.protos[0].num_registers as usize];

    let text = "twice(base) + 1";
    let program = fragment(text);
    let DebugEvalOutcome::Value { text: v1, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("first eval should succeed");
    };
    assert_eq!(v1, "21");
    let protos = vm.module.protos.len();
    let globals = vm.module.global_names.len();

    // Same text, same scope shape → memo hit: nothing appends, the value is fresh.
    let DebugEvalOutcome::Value { text: v2, .. } =
        vm.debug_eval_fragment(&program, 0, &[], EvalKind::Console, text, &frames, &regs)
    else {
        panic!("second eval should succeed");
    };
    assert_eq!(v2, "21");
    assert_eq!(
        vm.module.protos.len(),
        protos,
        "a repeated watch appends no protos"
    );
    assert_eq!(
        vm.module.global_names.len(),
        globals,
        "a repeated watch appends no global slots"
    );

    // Different text → a fresh compile (the memo is per-expression, not a single slot).
    let other = fragment("twice(base) + 2");
    let DebugEvalOutcome::Value { text: v3, .. } = vm.debug_eval_fragment(
        &other,
        0,
        &[],
        EvalKind::Console,
        "twice(base) + 2",
        &frames,
        &regs,
    ) else {
        panic!("third eval should succeed");
    };
    assert_eq!(v3, "22");
    assert!(vm.module.protos.len() > protos, "new text compiles fresh");
}

/// R0 (REPL-on-VM): [`Vm::run_top`] runs the entry chunk against globals that **persist between
/// calls**, and a single [`Vm::teardown`] afterwards brings heap residency back to zero. This is
/// the mechanism the session rides on — a first entry's global bindings survive into the next, and
/// cleanup is deferred to one final teardown rather than run after every entry.
#[test]
fn run_top_persists_globals_across_entries_then_one_teardown_zeroes_residency() {
    let src = "mut xs = [1, 2, 3];\necho xs.len();\n";
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("compiles");

    let before = noeta_value::live_count();
    let mode = noeta_value::CollectorMode::Trace;
    noeta_value::set_collector_mode(mode);
    let mut vm = Vm::load(
        &module,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
    );

    // Entry 1 binds the global `xs` (a heap list) and leaves it live between entries.
    vm.run_top();
    assert!(
        vm.persist.globals.iter().any(|v| !v.is_unbound()),
        "a global bound by the first entry survives into the next"
    );
    assert!(
        noeta_value::live_count() > before,
        "the bound list is resident between entries (no per-entry teardown ran)"
    );

    // Entry 2 re-runs the entry chunk against the *same* globals (rebinding `xs`, which releases
    // the first list and builds a new one) — no teardown in between.
    vm.run_top();

    // One teardown drains both entries' output and returns residency to the pre-run baseline.
    let result = vm.teardown(mode);
    assert_eq!(result.stdout, "3\n3\n");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        noeta_value::live_count(),
        before,
        "a single teardown after many entries brings residency to zero"
    );
}

#[test]
fn an_abort_captures_a_stack_trace_and_a_clean_run_captures_none() {
    // A panic three calls deep: the trace walks inner ← outer ← main, innermost first, with the
    // failing line on the innermost frame and each caller at its call site.
    let (result, trace) = run_traced(
        "fn inner(): int {\n  panic(\"boom\");\n}\nfn outer(): int {\n  return inner();\n}\nouter();\n",
    );
    assert_eq!(result.exit_code, 1);
    let names: Vec<Option<&str>> = trace.iter().map(|f| f.name.as_deref()).collect();
    assert_eq!(
        names,
        vec![Some("inner"), Some("outer"), Some("main")],
        "trace should be innermost-first: {trace:?}"
    );
    // Every frame resolved a source location (top-level programs have full line tables).
    assert!(
        trace.iter().all(|f| f.span.is_some()),
        "all frames should carry spans: {trace:?}"
    );

    // A clean run leaves no trace behind.
    let (result, trace) = run_traced("fn f(): int {\n  return 1;\n}\necho f();\n");
    assert_eq!(result.exit_code, 0);
    assert!(trace.is_empty(), "clean run must not trace: {trace:?}");
}

/// P-PKEY S0: the compiler bakes key-capability into `Module.shapes` — a `@packed` struct of
/// int/bool fields (or a nested chain of them, forward references included) is `key_capable`;
/// a float-field packed struct and a plain struct are not. Plumbing only — no behavior reads
/// the flag yet.
#[test]
fn shapes_carry_packed_key_capability() {
    let src = "@packed struct Outer { m: Mid }\n@packed struct Mid { c: Cell; n: i64 }\n@packed struct Cell { x: int; y: bool }\n@packed struct Vec2f { x: f32; y: f32 }\nstruct Plain { x: int }\no = Outer { m: Mid { c: Cell { x: 1, y: true }, n: 2i64 } }\nv = Vec2f { x: 1.0, y: 2.0 }\np = Plain { x: 1 }\necho o.m.n\n";
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("compiles");
    let flag = |name: &str| {
        module
            .shapes
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("shape {name} missing"))
            .key_capable
    };
    assert!(flag("Cell"), "all-int/bool packed struct is key-capable");
    assert!(flag("Mid"), "nested capable chain");
    assert!(
        flag("Outer"),
        "forward reference resolves through the fixpoint"
    );
    assert!(!flag("Vec2f"), "float fields disqualify");
    assert!(!flag("Plain"), "a non-packed struct is not key-capable");
}

/// Compile a source program to a [`Module`] (or panic if it's outside the VM subset), for the
/// tests that need to drive `run_module`/`run_module_jit` directly.
#[cfg(feature = "jit")]
fn compile_module(src: &str) -> Module {
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    compile(&parsed.program).expect("program should be in the M1.0 subset")
}

/// P-CALL S1 lock test: every offset [`frame_layout`] reports must locate the real `Frame` field,
/// and the probed `Vec`-header word indices must read back a live `Vec`'s ptr/len/cap. Because the
/// JIT bakes these numbers into native code generated in the same build, a silent `Frame`-layout
/// or `Vec`-header change would corrupt memory under the JIT; this test fails the build first.
#[cfg(feature = "jit")]
#[test]
#[allow(unsafe_code)]
fn frame_layout_locks_the_real_layout() {
    let l = frame_layout();
    assert_eq!(l.frame_size, size_of::<Frame>());
    assert_eq!(l.frame_align, align_of::<Frame>());

    // A sentinel frame: read each scalar field back through its reported offset.
    let f = Frame {
        proto: 0x0BAD_F00D,
        base: 0x1111_2222,
        pc: 0x3333_4444,
        ret_dst: 0x5566,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    };
    let fp = (&f as *const Frame) as usize;
    unsafe {
        assert_eq!(*((fp + l.proto_offset) as *const u32), 0x0BAD_F00D);
        assert_eq!(*((fp + l.base_offset) as *const usize), 0x1111_2222);
        assert_eq!(*((fp + l.pc_offset) as *const usize), 0x3333_4444);
        assert_eq!(*((fp + l.ret_dst_offset) as *const u16), 0x5566);
    }
    // The two empty-initialized fields must sit within the struct.
    assert!(l.ret_transform_offset < l.frame_size);
    assert!(l.upvalues_offset + size_of::<Vec<Value>>() <= l.frame_size);

    // Vec-header words: read a live Vec's ptr/len/cap back through the probed indices.
    let mut v: Vec<Value> = Vec::with_capacity(64);
    v.push(Value::unit());
    v.push(Value::unit());
    let words: [usize; 3] = unsafe { core::mem::transmute_copy(&v) };
    assert_eq!(words[l.vec_ptr_word], v.as_ptr() as usize);
    assert_eq!(words[l.vec_len_word], v.len());
    assert_eq!(words[l.vec_cap_word], v.capacity());
    // The three indices are a permutation of {0, 1, 2}.
    let mut idx = [l.vec_ptr_word, l.vec_len_word, l.vec_cap_word];
    idx.sort_unstable();
    assert_eq!(idx, [0, 1, 2]);
}

/// S1 (Tier-B bitwise native): an xorshift-with-masks loop of `^ & << >>` runs fully native
/// under forced JIT and matches the interpreter bit-for-bit; the two bail contracts hold —
/// a shift amount outside `0..64` aborts identically (native bails before any write, tier 0
/// raises), and a `<<` result past the 48-bit immediate range round-trips through the
/// interpreter's heap boxing with an identical value.
#[cfg(feature = "jit")]
#[test]
fn jit_bitwise_ops_native_with_identical_semantics() {
    // In-range workload: every intermediate fits the immediate range (36-bit state).
    let loop_src = "fn run(n: int): int {\n  mut h = 123456789;\n  mut i = 0;\n  while i < n {\n    h = h ^ (h << 11);\n    h = h & 68719476735;\n    h = h ^ (h >> 7);\n    i = i + 1;\n  }\n  return h;\n}\necho run(200);\n";
    let module = compile_module(loop_src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, bails) = VmBackend::new().run_module_jit_bails(&module);
    assert_eq!(interp, jit, "bitwise loop must be tier-identical");
    assert_eq!(jit.exit_code, 0);
    // The loop body never bails: the only recorded sites (if any) are outside `run`'s loop
    // (main's trailing echo) — no site may carry a per-iteration count.
    assert!(
        bails.iter().all(|b| b.count < 200),
        "the bitwise loop body must not bail per iteration: {bails:?}"
    );

    // Shift out of range: aborts identically (native bails, tier 0 raises E0008-class).
    let oob = compile_module("fn f(n: int): int {\n  return 1 << n;\n}\necho f(64);\n");
    let interp = VmBackend::new().run_module(&oob);
    let jit = VmBackend::new().run_module_jit(&oob);
    assert_eq!(interp, jit, "over-shift abort must be tier-identical");
    assert_ne!(jit.exit_code, 0, "1 << 64 aborts");

    // `<<` overflowing the immediate range: the interpreter heap-boxes; native bails before
    // the write and the values agree end to end.
    let big = compile_module(
        "fn f(n: int): int {\n  big = 1 << n;\n  return (big >> n) + 1;\n}\necho f(55);\n",
    );
    let interp = VmBackend::new().run_module(&big);
    let jit = VmBackend::new().run_module_jit(&big);
    assert_eq!(interp, jit, "boxing `<<` must be tier-identical");
    assert_eq!(jit.stdout, "2\n");
}

/// S1 (Tier W native): the sign-dependent fixed-width ops run native with the interpreter's
/// exact semantics. A u32 div/wrap loop stays native (no per-iteration bails); u64 semantics
/// on negative-erased words (unsigned div/compare/shift of `u64::MAX`, which erases to the
/// immediate `-1`) agree with tier 0 including where the result must heap-box (the fit guard
/// bails, tier 0 boxes); signed i8 `MIN / -1` wraps identically through the width mask.
#[cfg(feature = "jit")]
#[test]
fn jit_wide_int_ops_native_with_identical_semantics() {
    // u32 loop: WideInt Div (unsigned) + Binary Add + MaskWidth wrap, all in-width — native.
    let loop_src = "fn churn(n: int): u32 {\n  mut h: u32 = 123456789u32;\n  mut i = 0;\n  while i < n {\n    h = h / 3u32;\n    h = h + 4000000000u32;\n    i = i + 1;\n  }\n  return h;\n}\necho churn(300);\n";
    let module = compile_module(loop_src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, bails) = VmBackend::new().run_module_jit_bails(&module);
    assert_eq!(interp, jit, "u32 loop must be tier-identical");
    assert_eq!(jit.exit_code, 0);
    assert!(
        bails.iter().all(|b| b.count < 300),
        "the u32 loop body must not bail per iteration: {bails:?}"
    );

    // u64 semantics on a negative-erased word: unsigned div boxes its quotient (bail → tier-0
    // box), unsigned compare and logical shift read the full bit pattern.
    let u64_src =
        "x: u64 = 18446744073709551615u64;\necho x / 3u64;\necho x > 2u64;\necho x >> 1u64;\n";
    let m = compile_module(u64_src);
    let interp = VmBackend::new().run_module(&m);
    let jit = VmBackend::new().run_module_jit(&m);
    assert_eq!(
        interp, jit,
        "u64 negative-erased semantics must be tier-identical"
    );
    assert_eq!(
        jit.stdout,
        "6148914691236517205\ntrue\n9223372036854775807\n"
    );

    // Signed wrap: i8 MIN / -1 wraps through the width mask, no trap.
    let i8_src = "a: i8 = -128i8;\nb: i8 = -1i8;\necho a / b;\n";
    let m = compile_module(i8_src);
    let interp = VmBackend::new().run_module(&m);
    let jit = VmBackend::new().run_module_jit(&m);
    assert_eq!(interp, jit, "i8 MIN / -1 must be tier-identical");
    assert_eq!(jit.stdout, "-128\n");
}

/// S2 (mixed int/float + float `%` native): the canonical float-accumulator loop
/// (`total = total + i` — f64 × int every iteration) stays native with no per-iteration
/// bails and matches tier 0 bit-for-bit; float `%` runs through the fmod helper (including
/// a NaN result canonicalized like every float op); mixed comparisons and equality follow
/// the interpreter's widen-to-f64 semantics.
#[cfg(feature = "jit")]
#[test]
fn jit_mixed_numeric_and_float_rem_native_with_identical_semantics() {
    // The mixed-accumulator loop: previously bailed at `total + i` every iteration.
    let loop_src = "fn accum(n: int): float {\n  mut total = 0.0;\n  mut i = 0;\n  while i < n {\n    total = total + i;\n    total = total % 1000000.0;\n    i = i + 1;\n  }\n  return total;\n}\necho accum(300);\n";
    let module = compile_module(loop_src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, bails) = VmBackend::new().run_module_jit_bails(&module);
    assert_eq!(interp, jit, "mixed loop must be tier-identical");
    assert_eq!(jit.exit_code, 0);
    assert!(
        bails.iter().all(|b| b.count < 300),
        "the mixed loop body must not bail per iteration: {bails:?}"
    );

    // Float `%` semantics, NaN included; mixed compare/equality widen to f64.
    let src = "echo 7.5 % 2.25;\necho 5.5 % 0.0;\necho 1 == 1.0;\necho 2 < 2.5;\necho 3.5 % 2;\n";
    let m = compile_module(src);
    let interp = VmBackend::new().run_module(&m);
    let jit = VmBackend::new().run_module_jit(&m);
    assert_eq!(
        interp, jit,
        "float % / mixed semantics must be tier-identical"
    );
    assert_eq!(jit.stdout.lines().nth(2), Some("true"), "1 == 1.0 widens");
}

/// The `--jit-stats` bail histogram (S0): a function whose body holds a non-native op
/// (`Stringify`, string interpolation) bails there on **every call**, and the seam counts each
/// one against that exact `(proto, pc)`. Under `force_jit` everything compiles up front, so the
/// counts are deterministic: 10 calls → count 10 at `tag`'s `Stringify`, sorted first (the
/// histogram is most-frequent-first). Also locks that the recording seam is `None`-gated — the
/// plain stats entry point records nothing.
#[cfg(feature = "jit")]
#[test]
fn jit_bail_histogram_counts_per_call_bails_at_the_exact_site() {
    let src = "fn tag(n: int): string {\n  return \"v${n}\";\n}\nmut acc = 0;\nmut i = 0;\nwhile i < 10 {\n  s = tag(i);\n  acc = acc + 1;\n  i = i + 1;\n}\necho acc;\n";
    let module = compile_module(src);
    let (result, bails) = VmBackend::new().run_module_jit_bails(&module);
    assert_eq!(result.stdout, "10\n");

    // Locate `tag`'s prototype and its Stringify pc straight from the bytecode.
    let (tag_proto, tag_chunk) = module
        .protos
        .iter()
        .enumerate()
        .find(|(_, c)| c.name.as_deref() == Some("tag"))
        .expect("tag should have a prototype");
    let stringify_pc = tag_chunk
        .code
        .iter()
        .position(|op| matches!(op, Op::Stringify { .. }))
        .expect("tag should contain a Stringify") as u32;

    let site = bails
        .iter()
        .find(|b| b.proto == tag_proto as u32 && b.pc == stringify_pc)
        .expect("the Stringify site should be in the histogram");
    assert_eq!(site.count, 10, "one bail per call, exactly: {bails:?}");
    // Most-frequent-first: no site outranks the per-call one.
    assert_eq!(bails[0].count, site.count, "sorted descending: {bails:?}");

    // The plain stats entry point never records (the seam is `None`-gated).
    let (_, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(stats.native >= 1, "sanity: {stats:?}");
}

/// P-JIT foundation: a prototype with no compilable op runs its bail stub — reaching the
/// `noeta_jit_observe` helper — and control falls cleanly back to tier 0 with a byte-identical
/// result. `echo "hi"` is exactly such a program (its only prototype is `LoadConst`(str) /
/// `Stringify` / `Echo` / `Halt`, none of them fast). Proves the seam (Cranelift build + finalize,
/// tier-0/1 dispatch, the helper ABI, the deopt handoff) end to end.
#[cfg(feature = "jit")]
#[test]
fn jit_foundation_bails_to_identical_result_and_runs_native_stubs() {
    let module = compile_module("echo \"hi\";\n");
    let interp = VmBackend::new().run_module(&module);
    let before = jit_observe_count();
    let jit = VmBackend::new().run_module_jit(&module);
    let entered = jit_observe_count() - before;

    assert_eq!(interp, jit, "tier-1 result must match the interpreter");
    assert_eq!(jit.stdout, "hi\n");
    assert!(entered >= 1, "expected the bail stub to run, got {entered}");
}

/// J1 (integer fast path): a pure-integer `while`-loop function compiles to native code and, run
/// through the forced JIT, produces exactly the interpreter's result. This exercises the whole
/// integer op set — `LoadConst`, `Binary` (`+`/`%`/`<`), `CondBranch`, `Move`, `Drop`, `Jump` —
/// natively, with the `Return` bailing to tier 0.
#[cfg(feature = "jit")]
#[test]
fn jit_integer_while_loop_is_native_and_correct() {
    // sum of (i % 7) for i in 0..n — arithmetic, remainder, comparison, and a back-edge, all in
    // registers (no globals, no calls) → J1-eligible.
    let src = "fn run(n: int): int {\n  mut total = 0;\n  mut i = 0;\n  while i < n {\n    total = total + (i % 7);\n    i = i + 1;\n  }\n  return total;\n}\necho run(1000);\n";
    let module = compile_module(src);

    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);

    // The `run` prototype (and only it) is J1-eligible.
    assert!(
        stats.native >= 1,
        "the while-loop fn must go native, got {stats:?}"
    );
    assert_eq!(interp, jit, "tier-1 result must match the interpreter");
    // Independently confirm the value: sum_{i=0}^{999} (i % 7).
    let expected: i64 = (0..1000).map(|i| i % 7).sum();
    assert_eq!(jit.stdout, format!("{expected}\n"));
}

/// J1 deopt: a would-be big-int result (overflowing the 48-bit immediate range) bails from native
/// code to the interpreter, which heap-boxes it — so the JIT and interpreter still agree.
#[cfg(feature = "jit")]
#[test]
fn jit_integer_overflow_bails_and_matches() {
    // 2^40 * 2^40 = 2^80 wraps in i64 and, at each doubling, eventually exceeds the 48-bit
    // immediate range, forcing the overflow-bail path; the interpreter's wrapping result must match.
    let src = "fn run(n: int): int {\n  mut x = 1;\n  mut i = 0;\n  while i < n {\n    x = x * 3;\n    i = i + 1;\n  }\n  return x;\n}\necho run(60);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, _) = VmBackend::new().run_module_jit_with_stats(&module);
    assert_eq!(
        interp, jit,
        "overflow-bail result must match the interpreter"
    );
}

/// J2 (float fast path): a mixed int/float `while` loop — a float accumulator (`+`) with an
/// integer counter (`<`, `+`) — compiles to native code (each homogeneous `Binary` takes its
/// int or float branch) and matches the interpreter exactly.
#[cfg(feature = "jit")]
#[test]
fn jit_float_while_loop_is_native_and_correct() {
    let src = "fn run(n: int): float {\n  mut x = 0.0;\n  mut i = 0;\n  while i < n {\n    x = x + 1.5;\n    i = i + 1;\n  }\n  return x;\n}\necho run(1000);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the float loop fn must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "tier-1 float result must match the interpreter"
    );
    assert_eq!(jit.stdout, "1500.0\n");
}

/// J2 float division, comparison, and NaN: `6.0 / 4.0` divides natively, `0.0 / 0.0` produces a
/// canonicalized NaN, and an ordered float `<` (false on NaN) drives a `CondBranch` — the paths
/// most likely to diverge from the interpreter.
#[cfg(feature = "jit")]
#[test]
fn jit_float_division_and_nan_match() {
    let src = "fn run(): float {\n  mut a = 6.0 / 4.0;\n  mut z = 0.0;\n  mut q = z / z;\n  if q < a { return 0.0; }\n  return a;\n}\necho run();\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the float fn must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "NaN/division float result must match the interpreter"
    );
    assert_eq!(jit.stdout, "1.5\n");
}

/// J4 (heap/collections): a `for i in 0..n` loop — the idiomatic range loop, whose `MakeRange` /
/// `IterSnapshot` / `ListLen` / `ListGet` internals now run natively (through the leaf-op helper),
/// so the whole loop body is native. Refcount-exact (the snapshot list is a heap value) and
/// byte-identical to the interpreter.
#[cfg(feature = "jit")]
#[test]
fn jit_for_range_loop_is_native_and_correct() {
    let src = "fn run(n: int): int {\n  mut acc = 0;\n  for i in 0..n {\n    acc = acc + i;\n  }\n  return acc;\n}\necho run(1000);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the for-range loop fn must go native, got {stats:?}"
    );
    assert_eq!(interp, jit, "for-range result must match the interpreter");
    let expected: i64 = (0..1000).sum();
    assert_eq!(jit.stdout, format!("{expected}\n"));
}

/// Field access (P-JIT J4 slice 2): a hot loop that reads (`LoadField`) and writes (`SetField`,
/// the struct copy-on-write / reuse path) object fields runs natively through the leaf-op helper
/// and matches the interpreter — the store logic is the shared `set_field_fast`, so refcounts are
/// identical across the tier boundary (the `--jit-differential` leak check gates that).
#[cfg(feature = "jit")]
#[test]
fn jit_field_access_loop_is_native_and_correct() {
    let src = "struct Point {\n  mut x: int\n  mut y: int\n}\nfn run(n: int): int {\n  mut p = Point { x: 0, y: 0 };\n  mut i = 0;\n  while i < n {\n    p.x = p.x + i;\n    p.y = p.y + p.x;\n    i = i + 1;\n  }\n  return p.x + p.y;\n}\necho run(100);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the field-access loop fn must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "field-access result must match the interpreter"
    );
    assert_eq!(jit.stdout, "171600\n");
}

/// Subscript indexing (P-JIT J4 slice 3): a hot loop that indexes a list (`xs[i]`), a map
/// (`m[key]`), and a nested list-of-keys runs natively through the leaf-op helper's `Op::Index`
/// arm (the non-dispatching list/map/string paths; a user `Index` impl and every error case bail)
/// and matches the interpreter, including the borrow/retain of each looked-up element.
#[cfg(feature = "jit")]
#[test]
fn jit_indexing_loop_is_native_and_correct() {
    let src = "fn run(n: int): int {\n  xs = [10, 20, 30, 40, 50];\n  m = { \"a\": 1, \"b\": 2, \"c\": 3 };\n  keys = [\"a\", \"b\", \"c\"];\n  mut total = 0;\n  mut i = 0;\n  while i < n {\n    total = total + xs[i % 5];\n    total = total + m[keys[i % 3]];\n    i = i + 1;\n  }\n  return total;\n}\necho run(30);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the indexing loop fn must go native, got {stats:?}"
    );
    assert_eq!(interp, jit, "indexing result must match the interpreter");
    assert_eq!(jit.stdout, "960\n");
}

/// Tuple construction + projection (P-JIT J4 slice 4): a `for (i, x) in xs.enumerate()` loop —
/// `enumerate` yields `(int, T)` tuples (native `ListGet`) that the destructuring reads with
/// `TupleIndex` — runs natively through the leaf-op helper and matches the interpreter, including
/// the retain of each projected element.
#[cfg(feature = "jit")]
#[test]
fn jit_tuple_enumerate_loop_is_native_and_correct() {
    let src = "fn run(): int {\n  xs = [10, 20, 30, 40];\n  mut total = 0;\n  for (i, x) in xs.enumerate() {\n    total = total + i * x;\n  }\n  return total;\n}\necho run();\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the enumerate loop fn must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "tuple-enumerate result must match the interpreter"
    );
    // 0*10 + 1*20 + 2*30 + 3*40 = 200.
    assert_eq!(jit.stdout, "200\n");
}

/// OSR (P-JIT J5): a **top-level** loop — the whole program is one `while` loop in `main`, which
/// is entered exactly *once*, so entry-count promotion would never make it hot. Under ordinary
/// hot-counter promotion (not `force_jit`), it must still go native by counting the loop's
/// **back-edges** and entering tier 1 mid-frame at the loop header (on-stack replacement). This is
/// the production hole J5 closes.
#[cfg(feature = "jit")]
#[test]
fn jit_osr_top_level_loop_goes_native() {
    // 200 iterations > JIT_HOT_THRESHOLD (50): the back-edge counter promotes `main` (proto 0)
    // and OSRs into its loop, even though `main` is entered only once.
    let src =
        "mut acc = 0\nmut i = 0\nwhile i < 200 {\n  acc = acc + i\n  i = i + 1\n}\necho acc\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_hot_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the top-level loop must go native via OSR under hot-counter promotion, got {stats:?}"
    );
    assert_eq!(interp, jit, "OSR result must match the interpreter");
    let expected: i64 = (0..200).sum();
    assert_eq!(jit.stdout, format!("{expected}\n"));
}

/// OSR refcount-exactness (P-JIT J5): a top-level loop whose body moves **heap** values — a
/// top-level struct `b` read (`LoadField`) and written (`SetField`, the struct copy-on-write path)
/// each iteration, with the global `b` loaded into a register (a heap value) every pass. It
/// promotes and OSRs into native code mid-frame with that heap value live. Forcing `heap_aware`
/// for OSR-capable prototypes keeps the register stores refcount-correct; the result must match
/// the interpreter (the `--jit-differential` leak check gates residency).
#[cfg(feature = "jit")]
#[test]
fn jit_osr_heap_body_matches_interpreter() {
    let src = "struct Box { mut v: int }\nmut b = Box { v: 0 }\nmut i = 0\nwhile i < 100 {\n  b.v = b.v + i\n  i = i + 1\n}\necho b.v\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_hot_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the heap-body top-level loop must go native via OSR, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "OSR heap-body result must match the interpreter"
    );
    let expected: i64 = (0..100).sum();
    assert_eq!(jit.stdout, format!("{expected}\n"));
}

/// Native calls (P-JIT J3): recursive `fib` — the callee closure loaded via a heap-aware
/// `LoadGlobal` (retain), the recursive `Call` handled by the shared setup on the contiguous
/// stack, refcounts exact across the tier-0/tier-1 boundary — produces exactly the interpreter's
/// result. The `fib` prototype (and the top-level) go native.
#[cfg(feature = "jit")]
#[test]
fn jit_recursive_call_is_native_and_correct() {
    let src = "fn fib(n: int): int {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(20);\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    assert!(
        stats.native >= 1,
        "the recursive fn must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "recursive-call result must match the interpreter"
    );
    // fib(20) = 6765.
    assert_eq!(jit.stdout, "6765\n");
}

/// Native globals (P-JIT): a **top-level** loop with global `mut` accumulators — the natural
/// scripting shape — compiles natively (LoadGlobal/StoreGlobal inlined; first-bind via the
/// `note_global_bound` helper; `echo` at the end bails) and matches the interpreter. This exercises
/// per-op bail (the top-level prototype has `Echo`/`Stringify` it can't compile) plus the
/// unbound→bound global transition.
#[cfg(feature = "jit")]
#[test]
fn jit_global_top_level_loop_is_native_and_correct() {
    let src = "mut total = 0;\nmut i = 0;\nwhile i < 1000 {\n  total = total + (i % 7);\n  i = i + 1;\n}\necho total;\n";
    let module = compile_module(src);
    let interp = VmBackend::new().run_module(&module);
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
    // The top-level prototype (proto 0) itself goes native here.
    assert!(
        stats.native >= 1,
        "the top-level global loop must go native, got {stats:?}"
    );
    assert_eq!(
        interp, jit,
        "native-globals result must match the interpreter"
    );
    let expected: i64 = (0..1000).map(|i| i % 7).sum();
    assert_eq!(jit.stdout, format!("{expected}\n"));
}

/// Peak heap residency for one program (architecture §0.3) — `reset_peak` before, `live_peak`
/// after, so the high-water mark is measured in isolation.
fn peak_residency(src: &str) -> usize {
    noeta_value::reset_peak();
    let _ = run(src);
    noeta_value::live_peak()
}

#[test]
fn destructor_runs_on_collected_cycle_capture() {
    // Phase-6 destructor-on-collect: a self-recursive nested `fn` (the closure↔cell cycle) also
    // captures a destructor-bearing `Res`. After the call the whole subgraph — cycle + captured
    // `Res` — is unreachable garbage that only the collector reclaims; reclaiming it must run the
    // captured `Res`'s `destruct` (its last reference died with the cycle). So `drop 7` prints at
    // program-exit collection, after `make()`'s own `7`.
    let r = run(
        "class Res {\n  pub id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\nfn make(): int {\n  r = Res.new(7);\n  fn rec(n: int) use (r): int { if n <= 0 { return r.id; } return rec(n - 1); }\n  return rec(2);\n}\necho make();\n",
    );
    assert_eq!(r.stdout, "7\ndrop 7\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn cycle_is_reclaimed_by_backup_trace() {
    // A self-recursive nested `fn` ties a closure↔cell cycle that outlives the enclosing call;
    // refcounting alone cannot reclaim it (each member is kept alive by the other), so without
    // the Phase-6 backup mark-sweep it would leak. After the run, live residency must return to
    // its pre-run baseline — the collector reaped the cycle. Run under miri to validate the
    // collector + live-object registry (no use-after-free / double-free / leak).
    let before = noeta_value::live_count();
    let r = run(
        "fn compute(): int {\n  fn fact(n: int): int {\n    if n <= 1 { return 1; }\n    return n * fact(n - 1);\n  }\n  return fact(5);\n}\necho compute();\n",
    );
    assert_eq!(r.stdout, "120\n");
    assert_eq!(r.exit_code, 0);
    assert_eq!(
        noeta_value::live_count(),
        before,
        "the closure↔cell cycle must be reclaimed by the backup trace"
    );
}

fn run_with_collector(src: &str, mode: noeta_value::CollectorMode) -> RunResult {
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).expect("program should be in the M1.0 subset");
    VmBackend::new().run_module_with_collector(&module, mode)
}

#[test]
fn trial_deletion_reclaims_cycles_and_acyclic_garbage() {
    // The Phase-6.4 trial-deletion collector, exercised on its release path: a self-recursive
    // nested `fn` (the closure↔cell cycle, buffered as a candidate when the frame unwinds) plus
    // ordinary acyclic, heap-bearing programs (strings, objects, lists — none should be wrongly
    // buffered/freed). Each must finish with residency back at its pre-run baseline. Run under
    // miri to validate the deferred-dealloc release path + candidate buffering (no UAF / double
    // free / leak).
    let cyclic = "fn compute(): int {\n  fn fact(n: int): int {\n    if n <= 1 { return 1; }\n    return n * fact(n - 1);\n  }\n  return fact(5);\n}\necho compute();\n";
    let acyclic = "class P { mut x: int  tag: string\n  fn new(): P { return P { x: 0, tag: \"t\" }; } }\nmut p = P.new();\nfor i in 0..3 { p.x = p.x + i; }\nmut xs = [\"a\", \"b\"];\nxs[0] = \"z\";\necho \"${p.x} ${xs.join(\",\")}\";\n";
    // A reassigned destructor-bearing object exercises the VM's `release_value` last-reference
    // free — the path that must defer a *buffered* object rather than free it shallowly (the bug
    // that segfaulted before `free_shallow` became the universal deferral point).
    let destructed = "class Res { id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { x = self.id + 1; }\n}\nmut r = Res.new(0);\nfor i in 0..3 { r = Res.new(i); }\necho r.id;\n";
    for src in [cyclic, acyclic, destructed] {
        let before = noeta_value::live_count();
        let r = run_with_collector(src, noeta_value::CollectorMode::TrialDeletion);
        assert_eq!(r.exit_code, 0, "program aborted: {:?}", r.diagnostics);
        assert_eq!(
            noeta_value::live_count(),
            before,
            "trial-deletion must reclaim all heap (cycles + acyclic) by clean exit"
        );
    }
    // Reset the thread-local mode so later tests on this thread see the default again.
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
}

#[test]
fn mm_peak_residency_baseline() {
    // The pre-migration peak-residency snapshot for `plans/memory-management/phase-0-benchmarks`.
    // Prints under `--nocapture`; asserts the meter reflects each program's footprint shape.

    // Allocation churn: each short-lived struct dies before the next is built ⇒ a small,
    // n-independent peak (the reclaim-at-last-use shape we already have on a local temp).
    let churn = "class Pair { a: int b: int }\nmut total = 0;\nfor i in 0..4000 { p = Pair { a: i, b: i }; total = total + p.a; }\necho total;\n";
    let churn_peak = peak_residency(churn);

    // A monotonically-growing accumulator of **heap** elements (records — ints would be immediate
    // and never counted). Peak ≈ n live objects at the end: the genuinely-live structure prompt
    // reclamation cannot shrink, but whose transient cost reuse/COW keeps O(n) not O(n²).
    let accumulate = "class Pair { a: int b: int }\nmut acc = [];\nfor i in 0..4000 { acc ~= [Pair { a: i, b: i }]; }\necho acc.len();\n";
    let accumulate_peak = peak_residency(accumulate);

    // (Deep-nested teardown is benched separately on the optimized bench profile — its recursive
    // `free` overflows this 2 MiB debug test thread at shallow depth, the MM limitation recorded
    // in `phase-0-benchmarks.md`; it is not measured here.)

    eprintln!(
        "MM peak residency (objects): alloc_churn(n=4000)={churn_peak}  accumulate_records(n=4000)={accumulate_peak}"
    );

    // Shape assertions (not exact counts — those are the recorded baseline): churn stays small and
    // n-independent; the struct accumulator's peak scales with n.
    assert!(churn_peak < 100, "alloc churn peak should be n-independent");
    assert!(
        accumulate_peak >= 4000,
        "record-accumulator peak should scale with n"
    );
}

/// A function with `n` **single-assignment** intermediate records chained `aᵢ = f(aᵢ₋₁)`, each
/// dead once the next is built. Returns a scalar so nothing heap stays live past the chain.
fn sequential_intermediates_src(n: usize) -> String {
    let mut body = String::from("  a0 = Pair { a: 1, b: 1 };\n");
    for i in 1..n {
        body.push_str(&format!(
            "  a{i} = Pair {{ a: a{prev}.a + 1, b: a{prev}.b }};\n",
            prev = i - 1
        ));
    }
    format!(
        "class Pair {{ a: int b: int }}\nfn chain(): int {{\n{body}  return a{last}.a;\n}}\necho chain();\n",
        last = n - 1
    )
}

#[test]
fn mm_peak_residency_prompt_reclamation_is_n_independent() {
    // The headline Phase-3 metric (memory-management `phase-3-rc-passes` gate): precise last-use
    // drops reclaim a function-local the moment it dies, so a straight-line chain of n transient
    // intermediates holds only ~the current+previous struct live at once — an O(1), n-INDEPENDENT
    // peak. Under the pre-migration reclaim-at-teardown model every aᵢ stayed live until `chain`
    // returned, an O(n) peak. We prove the win by its shape: the peak must not grow with n.
    let small = peak_residency(&sequential_intermediates_src(50));
    let large = peak_residency(&sequential_intermediates_src(400));
    eprintln!("MM peak residency (objects): sequential_intermediates n=50={small}  n=400={large}");
    // n-independence is the proof of prompt reclamation: 8× the chain length leaves the peak flat
    // (a tiny constant — the live window — not 8× larger). A generous bound absorbs allocator slack
    // while still failing hard if drops regressed to teardown reclamation (which would be ≈ n).
    assert!(
        small < 20 && large < 20,
        "prompt last-use reclamation should keep the intermediate-chain peak O(1); got n=50→{small}, n=400→{large}"
    );
}

#[test]
fn invoke_by_name_wraps_ok_and_err() {
    // `invoke` dispatches by runtime name: a hit wraps the return in `Result.Ok` (via the
    // `WrapOk` frame transform); an unknown name / arity mismatch builds `Result.Err`. Exercises
    // the new type-handle value, the `Op::Invoke` dispatch, and the refcount handoff on return.
    let r = run(
        "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}\nhit = match invoke(Box.new(21), \"doubled\", []) { Ok(v) => \"${v}\", Err(e) => \"err ${e}\" };\necho hit;\nmade = match invoke(Box, \"new\", [7]) { Ok(b) => match invoke(b, \"doubled\", []) { Ok(d) => \"${d}\", Err(_) => \"x\" }, Err(_) => \"x\" };\necho made;\nmiss = match invoke(Box.new(1), \"nope\", []) { Ok(_) => \"ok\", Err(_) => \"miss\" };\necho miss;\n",
    );
    assert_eq!(r.stdout, "42\n14\nmiss\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn type_of_distinguishes_nominal_kinds() {
    // `type_of` classifies a value's shape kind into `Type.Enum`/`Type.Struct`/`Type.Class`
    // (not a collapsed `Named`). Exercises `vm_type_repr` + `build_type_value`'s kind arms and
    // their refcount handoff.
    let r = run(
        "enum E { A; }\nstruct R { x: int }\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\nfn k(t: Type): string { return match t { Type.Enum(n, _) => \"e:${n}\", Type.Struct(n, _) => \"r:${n}\", Type.Class(n, _) => \"c:${n}\", _ => \"?\" }; }\necho k(type_of(E.A));\necho k(type_of(R { x: 1 }));\necho k(type_of(C.new()));\n",
    );
    assert_eq!(r.stdout, "e:E\nr:R\nc:C\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn abstract_kind_is_tests() {
    // `is Enum`/`Struct`/`Class` are runtime kind tests over a `dyn` value, keyed on the
    // value's shape kind. Exercises the new `narrow_matches` arms in the VM.
    let r = run(
        "enum E { A; }\nstruct R { x: int }\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\ne: dyn = E.A;\nrec: dyn = R { x: 1 };\nc: dyn = C.new();\necho e is Enum;\necho rec is Struct;\necho c is Class;\necho e is Struct;\n",
    );
    assert_eq!(r.stdout, "true\ntrue\ntrue\nfalse\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn roles_of_materializes_the_index() {
    // `roles_of()` materializes the `(declaration, role)` index into a `List<RoleBinding>`,
    // each carrying a fresh `string` target and the named enum value. Exercises `materialize_roles`
    // and `make_role` plus the refcount handoff of the freshly-built list/struct/enum values.
    let r = run(
        "@attribute(Function)\n@role(Semantic.EntryPoint)\nstruct Route { path: string }\n#[Route(\"/x\")]\nfn handle(): int { return 1; }\nfor b in roles_of() {\n  echo match b.role { Semantic.EntryPoint => \"${b.target}=entry\", _ => \"other\" };\n}\n",
    );
    assert_eq!(r.stdout, "handle=entry\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn arithmetic_and_concat() {
    let r = run("echo 1 + 2 * 3;\necho \"users/\" ~ 42 ~ \"/profile\";\n");
    assert_eq!(r.stdout, "7\nusers/42/profile\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn cow_in_place_append_paths() {
    // VM-side copy-on-write self-append (`~=`). Covers: a GLOBAL accumulator (TakeGlobal +
    // ConcatInPlace) on the unique path (`g ~= ["b"]`) and the aliased path (`h = g; g ~= ["c"]`
    // — the alias must keep `h` at the pre-append value, so COW copies); and a LOCAL accumulator
    // inside a function (the register path, int elements). Heap elements (strings) exercise the
    // element-retain accounting; run under miri to validate refcounts (no UAF / double free).
    let r = run(
        "mut g = [\"a\"];\ng ~= [\"b\"];\nh = g;\ng ~= [\"c\"];\necho g;\necho h;\nfn build(): List<int> {\n    mut acc = [];\n    for i in 0..3 {\n        acc ~= [i];\n    }\n    return acc;\n}\necho build();\n",
    );
    assert_eq!(
        r.stdout,
        "[\"a\", \"b\", \"c\"]\n[\"a\", \"b\"]\n[0, 1, 2]\n"
    );
    assert_eq!(r.exit_code, 0);
}

#[test]
fn record_update_reuse_paths() {
    // VM-side record-update reuse (`acc = T { ...acc, … }`). Covers the RUNTIME-checked
    // `Op::MakeStructInPlace` paths reached via a GLOBAL accumulator (`TakeGlobal` exposes the
    // taken-out value's uniqueness; Phase 5.1b): (1) the in-place hit — a global whose update
    // overwrites a field, with a HEAP field (`tag`) whose reference must transfer untouched across
    // the reuse; (2) the copy fallback — an aliased accumulator (`snap = acc`) must keep `snap` at
    // the pre-update value (the runtime refcount > 1 forces the copy). Heap fields exercise the
    // slot retain/release accounting; run under miri to validate refcounts (no UAF/double free).
    let r = run(
        "class Point {\n  x: int\n  tag: string\n  fn show(): string { return \"${self.x} ${self.tag}\"; }\n}\nmut acc = Point { x: -1, tag: \"k\" };\nfor i in 0..4 {\n  acc = Point { ...acc, x: i };\n}\necho acc.show();\nmut p = Point { x: 1, tag: \"a\" };\nsnap = p;\np = Point { ...p, x: 9 };\necho p.show();\necho snap.show();\n",
    );
    assert_eq!(r.stdout, "3 k\n9 a\n1 a\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn map_update_reuse_paths() {
    // VM-side in-place map update (`m[k] = v` ⟶ `m = m.set(k, v)`; Phase 5.1c). Covers the two
    // runtime paths of a reuse-marked local map self-update: (1) the in-place hit — a uniquely-owned
    // accumulator mutated in place, including overwriting a key (its displaced HEAP value released)
    // and removing one; (2) the copy fallback — an aliased accumulator (`snap = m`) must keep `snap`
    // at the pre-update value. String values exercise the slot retain/release accounting; run under
    // miri to validate refcounts (no UAF / double free).
    let r = run(
        "fn build(): string {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = \"v${i}\"; }\n  m[\"k0\"] = \"x\";\n  m = m.remove(\"k1\");\n  return \"${m.values()} ${m.len()}\";\n}\necho build();\nmut acc = { \"a\": \"1\" };\nsnap = acc;\nacc[\"a\"] = \"9\";\nacc[\"b\"] = \"2\";\necho acc.values();\necho snap.values();\n",
    );
    assert_eq!(r.stdout, "[\"x\", \"v2\"] 2\n[\"9\", \"2\"]\n[\"1\"]\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn list_set_reuse_paths() {
    // VM-side in-place list `set` (`xs[i] = v` ⟶ `xs = xs.set(i, v)`). Covers the in-place hit — a
    // function-local accumulator overwrites each slot in place, its displaced HEAP element released
    // each step — and the copy fallback — an aliased accumulator (`snap = ys`) keeps its value.
    // String elements exercise the slot retain/release accounting; run under miri (no UAF / double
    // free).
    let r = run(
        "fn build(): string {\n  mut xs = [\"a\", \"b\", \"c\"];\n  for i in 0..3 { xs[i] = \"v${i}\"; }\n  return xs.join(\",\");\n}\necho build();\nmut ys = [\"x\", \"y\"];\nsnap = ys;\nys[0] = \"z\";\necho ys.join(\",\");\necho snap.join(\",\");\n",
    );
    assert_eq!(r.stdout, "v0,v1,v2\nz,y\nx,y\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn set_update_reuse_paths() {
    // VM-side in-place set update (`s = s.add(x)` / `s = s.remove(x)`). Covers the in-place hit —
    // a function-local accumulator binary-search-inserts/removes one element in its existing
    // canonical buffer, including a duplicate `add` (a no-op) and a `remove` — and the copy
    // fallback — an aliased accumulator (`snap = t`) keeps its value. String elements exercise the
    // element retain/release accounting; run under miri (no UAF / double free).
    let r = run(
        "fn build(): string {\n  mut s = #{};\n  for i in 0..3 { s = s.add(\"v${i}\"); }\n  s = s.add(\"v0\");\n  s = s.remove(\"v1\");\n  return \"${s.len()}\";\n}\necho build();\nmut t = #{\"a\", \"b\"};\nsnap = t;\nt = t.add(\"c\");\nt = t.remove(\"a\");\necho t;\necho snap;\n",
    );
    assert_eq!(r.stdout, "2\n{\"b\", \"c\"}\n{\"a\", \"b\"}\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn mut_field_set_reuse_paths() {
    // VM-side in-place `mut` field assignment on a value `struct` (`x.f = v`, copy-on-write).
    // Covers the in-place hit — a function-local accumulator overwrites its `mut` fields each
    // iteration (its displaced HEAP field, a string, released each step) — and the copy fallback
    // — an aliased snapshot (`snap = p`) keeps its value because the shared struct is copied
    // before the write. The string field exercises the slot retain/release accounting; run under
    // miri (no UAF / double free).
    let r = run(
        "struct Box {\n  mut tag: string\n  mut n: int\n  fn new(): Box { return Box { tag: \"init\", n: 0 }; }\n}\nfn build(): string {\n  mut b = Box.new();\n  for i in 0..3 { b.n = b.n + i; b.tag = \"t${i}\"; }\n  return \"${b.tag} ${b.n}\";\n}\necho build();\nmut p = Box.new();\nsnap = p;\np.tag = \"changed\";\necho p.tag;\necho snap.tag;\n",
    );
    assert_eq!(r.stdout, "t2 3\nchanged\ninit\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn class_field_set_is_reference_semantic() {
    // VM-side in-place `mut` field assignment on a reference `class` (object-model slice 2b):
    // the instance is mutated in place even when **aliased**, so a snapshot taken beforehand
    // (`snap = p`) observes the change (`snap.tag` → "changed", unlike the struct copy fallback).
    // The displaced HEAP string is released on each overwrite; run under miri (no UAF / double
    // free) to validate the in-place-while-shared retain/release accounting.
    let r = run(
        "class Box {\n  mut tag: string\n  mut n: int\n  fn new(): Box { return Box { tag: \"init\", n: 0 }; }\n}\nfn build(): string {\n  mut b = Box.new();\n  for i in 0..3 { b.n = b.n + i; b.tag = \"t${i}\"; }\n  return \"${b.tag} ${b.n}\";\n}\necho build();\nmut p = Box.new();\nsnap = p;\np.tag = \"changed\";\necho p.tag;\necho snap.tag;\n",
    );
    assert_eq!(r.stdout, "t2 3\nchanged\nchanged\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn reference_cycle_is_collected_at_exit() {
    // VM-side reference-`class` cycle (object-model slice 2c): `a.next = b; b.next = a` ties a
    // cycle precise refcounting cannot reclaim. The exit-time backup `collect_trace(&[])` reclaims
    // both members and runs each `destruct` in reverse-creation order (newest-first). Run under
    // miri to validate the cycle's `gc_free_shallow` reclamation (no UAF / double free) and the
    // leak oracle to confirm residency 0.
    let before = noeta_value::live_count();
    let r = run(
        "class Node {\n  mut next: ?Node\n  id: int\n  fn new(id: int): Node { return Node { next: none, id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\na = Node.new(1);\nb = Node.new(2);\na.next = some(b);\nb.next = some(a);\necho \"linked\";\n",
    );
    assert_eq!(r.stdout, "linked\ndrop 2\ndrop 1\n");
    assert_eq!(r.exit_code, 0);
    assert_eq!(
        noeta_value::live_count(),
        before,
        "cycle must leave no residency"
    );
}

#[test]
fn record_reassign_reuse_paths() {
    // VM-side whole-value struct reassignment reuse (`p = P { … }`, no spread; Phase 5 general
    // reassignment). The reuse pass injects a `...p` spread (a struct literal sets every field, so
    // it is value-identical), so this lowers to `MakeStructInPlace` overwriting *all* slots — the
    // in-place hit reuses `p`'s cell across the loop (its displaced HEAP field `tag` released each
    // step), while an aliased reassignment (`snap = q`) copies to preserve `snap`. Run under miri to
    // validate the all-slot overwrite's retain/release accounting (no UAF / double free).
    // On a deep stack: this case's front-end recursion out-runs the 2 MiB debug test thread.
    let r = on_deep_stack(|| {
        run(
            "class P {\n  n: int\n  tag: string\n  fn show(): string { return \"${self.n} ${self.tag}\"; }\n}\nfn build(): string {\n  mut p = P { n: 0, tag: \"a\" };\n  for i in 0..3 { p = P { n: i, tag: \"t${i}\" }; }\n  return p.show();\n}\necho build();\nmut q = P { n: 1, tag: \"x\" };\nsnap = q;\nq = P { n: 9, tag: \"y\" };\necho q.show();\necho snap.show();\n",
        )
    });
    assert_eq!(r.stdout, "2 t2\n9 y\n1 x\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn record_update_reuse_with_self_read() {
    // Drop insertion (Step B): a self-update that *reads* the accumulator
    // (`acc = Point { ...acc, x: acc.x + 1 }`) reuses in place — the `Drop` after the `acc.x`
    // `LoadField` frees the receiver temporary, restoring unique ownership before the construct.
    // Covers a LOCAL accumulator (Step A: no declaration `Move`) inside a function with a HEAP
    // field carried across each in-place update. Run under miri to validate the `Drop` does not
    // double-free the receiver and the carried heap field's refcount stays balanced.
    let r = run(
        "class Point {\n  x: int\n  label: string\n  fn show(): string { return \"${self.x} ${self.label}\"; }\n}\nfn run(n: int): string {\n  mut acc = Point { x: 0, label: \"p\" };\n  for i in 0..n {\n    acc = Point { ...acc, x: acc.x + 2 };\n  }\n  return acc.show();\n}\necho run(5);\n",
    );
    assert_eq!(r.stdout, "10 p\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn in_place_reuse_fires_replaced_field_destructor() {
    // Phase 5.1a: a function-local self-update of a destructor-free `Box` reuses in place, but the
    // *replaced* field `r` (a destructor-bearing `Res`) must run its `destruct` at the update via
    // the in-place path's `replace_slot` + `release_value`. Run under miri to validate the
    // displaced field is released exactly once (no UAF / double-free) and the carried field `n`
    // stays balanced.
    let r = run(
        "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\nclass Box {\n  r: Res\n  n: int\n}\nfn run(): void {\n  mut acc = Box { r: Res.new(0), n: 7 };\n  acc = Box { ...acc, r: Res.new(1) };\n  echo \"n=${acc.n}\";\n}\nrun();\n",
    );
    assert_eq!(r.stdout, "drop 0\nn=7\ndrop 1\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn heap_element_list_concat_refcounts() {
    // Probe: concatenating lists of HEAP elements (strings) must keep element refcounts
    // balanced (no UAF / double free at teardown). Run under miri to validate.
    let r = run(
        "mut acc = [\"a\", \"b\"];\nacc = acc ~ [\"c\"];\nacc ~= [\"d\"];\nb = acc;\nacc ~= [\"e\"];\necho acc;\necho b;\n",
    );
    assert_eq!(
        r.stdout,
        "[\"a\", \"b\", \"c\", \"d\", \"e\"]\n[\"a\", \"b\", \"c\", \"d\"]\n"
    );
    assert_eq!(r.exit_code, 0);
}

#[test]
fn integer_wrapping_matches_i64() {
    let r = run("echo 9223372036854775807 + 1;\necho 9223372036854775807 * 2;\n");
    assert_eq!(r.stdout, "-9223372036854775808\n-2\n");
}

#[test]
fn mutable_reassignment() {
    let r = run("mut total = 0;\ntotal = total + 5;\necho total;\n");
    assert_eq!(r.stdout, "5\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn immutable_reassignment_is_e0006() {
    let r = run("name = \"a\";\nname = \"b\";\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(r.diagnostics.len(), 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::ImmutableAssignment
    );
}

#[test]
fn functions_calls_and_nested_calls() {
    let r = run(
        "fn add(a, b) { return a + b; }\nfn dbl(n) { return n * 2; }\nfn quad(n) { return dbl(dbl(n)); }\necho add(2, 3);\necho quad(3);\n",
    );
    assert_eq!(r.stdout, "5\n12\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn recursion_through_globals() {
    let r = run(
        "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(10);\n",
    );
    assert_eq!(r.stdout, "55\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn closure_captures_global() {
    let r = run("base = 100;\nadd_base = fn(x) => x + base;\necho add_base(5);\n");
    assert_eq!(r.stdout, "105\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn pipeline_threads_first_argument() {
    let r = run(
        "fn inc(n) { return n + 1; }\nfn add(a, b) { return a + b; }\necho 5 |> inc |> inc;\necho 5 |> add(10);\n",
    );
    assert_eq!(r.stdout, "7\n15\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn parameter_shadows_global() {
    let r = run("base = 100;\nfn f(base) { return base; }\necho f(5);\necho base;\n");
    assert_eq!(r.stdout, "5\n100\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn arity_mismatch_is_type_error() {
    let r = run("fn add(a, b) { return a + b; }\necho add(1);\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn implicit_unit_return_displays_empty() {
    // A function with no `return` yields unit, which echoes as an empty line (M0 parity).
    let r = run("fn noop(x) { x + 1; }\necho noop(5);\n");
    assert_eq!(r.stdout, "\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn short_circuit_logic() {
    // `false && <error>` short-circuits to false without evaluating the right side.
    assert_eq!(run("echo false && 1 < 2;\n").stdout, "false\n");
    assert_eq!(run("echo true || 1 < 2;\n").stdout, "true\n");
    assert_eq!(run("echo 1 < 2 && 3 >= 3;\n").stdout, "true\n");
}

#[test]
fn division_by_zero_is_e0008() {
    let r = run("echo 1 / 0;\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::DivisionByZero
    );
}

#[test]
fn unknown_name_is_e0005() {
    let r = run("echo missing;\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::UnknownName
    );
}

#[test]
fn destructors_run_at_program_end_in_reverse_declaration_order() {
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\na = R.new(\"a\");\nb = R.new(\"b\");\necho \"body\";\n",
    );
    // Globals destroyed in reverse declaration order: b before a.
    assert_eq!(r.stdout, "body\nclose b\nclose a\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn destructor_fires_at_a_locals_last_use_not_at_program_end() {
    // Phase 4: a destructor-bearing function **local** runs its `destruct` at its last use —
    // here the `r.announce()` call — before the function returns, not deferred to program end.
    // The bare `compile` path marks every drop conservatively relevant, so the local's
    // `Op::Drop` routes through `release_value` and fires the destructor.
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn announce(): void { echo \"here ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn scope(): void {\n  r = R.new(\"x\");\n  r.announce();\n  echo \"after\";\n}\necho \"start\";\nscope();\necho \"end\";\n",
    );
    // `r`'s last use is `r.announce()`; the destructor fires right after it returns, before
    // "after" — and definitely before program end ("end").
    assert_eq!(r.stdout, "start\nhere x\nclose x\nafter\nend\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn reassigning_a_binding_destroys_the_displaced_value() {
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nmut x = R.new(\"first\");\nx = R.new(\"second\");\necho \"mid\";\n",
    );
    // "first" is destroyed at the reassignment; "second" at program end.
    assert_eq!(r.stdout, "close first\nmid\nclose second\n");
}

#[test]
fn reassigning_a_local_destroys_displaced_then_survivor_at_scope_exit() {
    // Phase 4.2a: a reassigned **local** (not a global) destroys its displaced value at the
    // assignment via the `Op::Drop` the compiler emits before the overwriting `Op::Move`
    // (`set_reg`'s plain release would not fire the destructor), and its surviving value via the
    // function-body scope-exit drop. "first" closes between the two reads; "second" before return.
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn go(): void {\n  mut r = R.new(\"first\");\n  r.use_it();\n  r = R.new(\"second\");\n  r.use_it();\n}\necho \"start\";\ngo();\necho \"end\";\n",
    );
    assert_eq!(
        r.stdout,
        "start\nuse first\nclose first\nuse second\nclose second\nend\n"
    );
    assert_eq!(r.exit_code, 0);
}

#[test]
fn question_mark_propagation_destroys_abandoned_locals() {
    // Phase 4.2c: a `?` that early-returns an `Err` destroys the frame locals it abandons before
    // unwinding (the `on_error` drops the compiler attaches to `Op::TryUnwrap`). `r` is live past
    // the `?`, so `close r` fires on the error path, before the caller prints the propagated Err.
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn check(c: bool): Result<int, string> {\n  if c { return Ok(1); }\n  return Err(\"bad\");\n}\nfn go(c: bool): Result<int, string> {\n  r = R.new(\"r\");\n  x = check(c)?;\n  return Ok(x);\n}\necho \"start\";\necho go(false);\necho \"end\";\n",
    );
    assert_eq!(r.stdout, "start\nclose r\nErr(bad)\nend\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn panic_destroys_live_frame_locals_in_reverse_construction_order() {
    // Phase 4.2c-ii: as a panic aborts, the VM's per-frame teardown fires the `destruct` of each
    // live destructor-bearing frame local (the `frame_locals` list reversed), so `a` and `b` are
    // destroyed — `b` before `a` — before the program exits 1. They are never read, so they live
    // undropped to the panic; the panic-aware `coalesce` pinning keeps them in distinct registers.
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn go(): void {\n  a = R.new(\"a\");\n  b = R.new(\"b\");\n  echo \"made\";\n  panic(\"boom\");\n}\necho \"start\";\ngo();\n",
    );
    assert_eq!(r.stdout, "start\nmade\nclose b\nclose a\n");
    assert_eq!(r.exit_code, 1);
}

#[test]
fn destroying_a_container_runs_its_destructor_then_its_fields_in_declared_order() {
    // Phase 4.3 (spec §4): destroying an object runs the container's own `destruct` first (its
    // fields still live), then releases its fields depth-first in declared order, each firing its
    // own `destruct`. `Outer`'s two destructor-bearing `Leaf` fields are built inline (so the
    // struct holds the sole reference — the construction-temp release makes refcount 1 here), and
    // `o` is a dead-store dropped at scope exit: `outer`, then `a`, then `b` (declared order).
    let r = run(
        "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${self.tag}\"; }\n}\nclass Outer {\n  label: string\n  a: Leaf\n  b: Leaf\n  fn new(): Outer { return Outer { label: \"o\", a: Leaf.new(\"a\"), b: Leaf.new(\"b\") }; }\n  destruct { echo \"drop outer ${self.label}\"; }\n}\nfn go(): void {\n  o = Outer.new();\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
    );
    assert_eq!(
        r.stdout,
        "start\nbuilt\ndrop outer o\ndrop a\ndrop b\nend\n"
    );
    assert_eq!(r.exit_code, 0);
}

#[test]
fn destroying_a_list_runs_its_elements_destructors_in_order() {
    // Phase 4.3 (spec §4): a collection releases its elements in iteration order. The list has no
    // `destruct`; its contained `Leaf`s do, and fire a, b, c (index order) when the list dies. The
    // construction-temp releases make the list the sole owner, so each element is at refcount 1.
    let r = run(
        "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${self.tag}\"; }\n}\nfn go(): void {\n  items = [Leaf.new(\"a\"), Leaf.new(\"b\"), Leaf.new(\"c\")];\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
    );
    assert_eq!(r.stdout, "start\nbuilt\ndrop a\ndrop b\ndrop c\nend\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn a_temp_used_only_as_a_receiver_fires_its_destructor() {
    // Phase 4.4 (spec §2): a destructor-bearing value used only as a method receiver, or
    // discarded as a bare statement, still fires at last use — a temp is an owner. `R.new("a")`
    // is consumed by `.use_it()` (fires after the call); `R.new("b");` is discarded (fires at the
    // statement). The compiler emits a destructor-aware `Op::Drop` of the receiver / discarded
    // register where there was none before.
    let r = run(
        "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\necho \"start\";\nR.new(\"a\").use_it();\nR.new(\"b\");\necho \"end\";\n",
    );
    assert_eq!(r.stdout, "start\nuse a\nclose a\nclose b\nend\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn a_class_without_a_destructor_runs_nothing() {
    let r = run(
        "class R {\n  v: int\n  fn new(v: int): R { return R { v: v }; }\n}\nx = R.new(1);\necho \"done\";\n",
    );
    assert_eq!(r.stdout, "done\n");
}

#[test]
fn record_literal_field_access_and_structural_equality() {
    let r = run(
        "struct Item { price: float qty: int }\na = Item { price: 2.5, qty: 4 };\necho a.price;\necho a.price * a.qty;\nb = Item { price: 2.5, qty: 4 };\necho a == b;\n",
    );
    assert_eq!(r.stdout, "2.5\n10.0\ntrue\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn object_displays_as_a_literal() {
    let r = run("struct Pt { x: int y: int }\necho Pt { x: 1, y: 2 };\n");
    assert_eq!(r.stdout, "Pt {x: 1, y: 2}\n");
}

#[test]
fn missing_field_is_e0009() {
    let r = run("struct P { x: int y: int }\np = P { x: 1 };\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::MissingField
    );
}

#[test]
fn class_constructor_method_and_field_access() {
    let r = run(
        "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}\nb = Box.new(21);\necho b.doubled();\necho b.v;\n",
    );
    assert_eq!(r.stdout, "42\n21\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn method_takes_arguments_alongside_fields() {
    let r = run(
        "class Counter {\n  base: int\n  fn new(base: int): Counter { return Counter { base: base }; }\n  fn plus(n: int): int { return self.base + n; }\n}\nc = Counter.new(10);\necho c.plus(5);\n",
    );
    assert_eq!(r.stdout, "15\n");
}

#[test]
fn structural_update_overrides_one_field() {
    let r = run(
        "class M {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): M { return M { amount: a, currency: c }; }\n}\na = M.new(500, \"USD\");\nb = M { amount: 300, ...a };\necho b.amount;\necho b.currency;\necho a.amount;\n",
    );
    assert_eq!(r.stdout, "300\nUSD\n500\n");
}

#[test]
fn operator_trait_overloads_plus() {
    // `a + b` on a class implementing `Add` dispatches to its `add` method (M1.8).
    let r = run(
        "class Money {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): Money { return Money { amount: a, currency: c }; }\n  impl Add {\n    fn add(other: Money): Money { return Money { amount: self.amount + other.amount, currency: self.currency }; }\n  }\n}\na = Money.new(5, \"USD\");\nb = Money.new(3, \"USD\");\nt = a + b;\necho t.amount;\necho t.currency;\n",
    );
    assert_eq!(r.stdout, "8\nUSD\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn operators_on_builtins_are_unaffected_by_overloads() {
    // A class without the relevant trait method leaves built-in `+` semantics untouched.
    let r = run("echo 2 + 3;\necho \"a\" ~ \"b\";\n");
    assert_eq!(r.stdout, "5\nab\n");
}

#[test]
fn equatable_overrides_equality_and_negates_for_ne() {
    // `impl Equatable` routes `==`/`!=` to `eq`; `eq` here ignores `tag`, and `!=` negates the
    // returned bool through the frame's return transform.
    let r = run(
        "class M {\n  amount: int\n  tag: int\n  fn new(a: int, t: int): M { return M { amount: a, tag: t }; }\n  impl Equatable {\n    fn eq(other: M): bool { return self.amount == other.amount; }\n  }\n}\na = M.new(5, 1);\nb = M.new(5, 2);\necho a == b;\necho a != b;\necho a == M.new(9, 1);\n",
    );
    assert_eq!(r.stdout, "true\nfalse\nfalse\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn comparable_overloads_ordering_operators() {
    // `impl Comparable` routes `< <= > >=` to `compare`; the returned `Ordering` is mapped to
    // each operator's bool via the frame's return transform.
    let r = run(
        "class M {\n  amount: int\n  fn new(a: int): M { return M { amount: a }; }\n  impl Comparable {\n    fn compare(other: M): Ordering { return self.amount.compare(other.amount); }\n  }\n}\na = M.new(5);\nb = M.new(8);\necho a < b;\necho a > b;\necho a <= b;\necho a >= b;\n",
    );
    assert_eq!(r.stdout, "true\nfalse\ntrue\nfalse\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn primitive_compare_yields_ordering() {
    let r = run("echo 1.compare(2);\necho 5.compare(5);\necho 9.compare(2);\n");
    assert_eq!(
        r.stdout,
        "Ordering.Less\nOrdering.Equal\nOrdering.Greater\n"
    );
}

#[test]
fn derive_comparable_orders_fields_lexicographically() {
    // `@derive(Comparable)` gives structural ordering via the Module's comparable set + the
    // VM's `structural_compare`; no method is called.
    let r = run(
        "@derive(Comparable)\nclass P {\n  x: int\n  y: int\n  fn new(x: int, y: int): P { return P { x: x, y: y }; }\n}\na = P.new(1, 2);\nb = P.new(1, 5);\nc = P.new(1, 2);\necho a < b;\necho a > b;\necho a <= c;\necho a >= c;\n",
    );
    assert_eq!(r.stdout, "true\nfalse\ntrue\ntrue\n");
}

#[test]
fn comparison_on_non_comparable_object_errors() {
    let r = run(
        "class P {\n  x: int\n  fn new(x: int): P { return P { x: x }; }\n}\necho P.new(1) < P.new(2);\n",
    );
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn index_list_by_position() {
    // List element access retains the element (refcount discipline checked under miri).
    let r = run("xs = [\"a\", \"b\", \"c\"];\necho xs[1];\necho [10, 20][0];\n");
    assert_eq!(r.stdout, "b\n10\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn index_out_of_bounds_is_e0016() {
    let r = run("xs = [1, 2];\necho xs[5];\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::IndexOutOfBounds
    );
}

#[test]
fn index_dispatches_to_index_trait() {
    // `inv[i]` routes to the class's `Index::get`, pushing a call frame `[recv, index]`.
    let r = run(
        "class Inv {\n  items: list\n  fn new(items: list): Inv { return Inv { items: items }; }\n  impl Index {\n    fn get(i: int): int { return self.items[i]; }\n  }\n}\necho Inv.new([7, 8, 9])[2];\n",
    );
    assert_eq!(r.stdout, "9\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn indexing_a_non_indexable_is_type_error() {
    let r = run("echo 42[0];\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn index_map_by_key() {
    // Map element access by string key retains the value (refcount discipline under miri).
    let r = run("m = {\"a\": \"x\", \"b\": \"y\"};\necho m[\"b\"];\n");
    assert_eq!(r.stdout, "y\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn index_map_missing_key_is_e0018() {
    let r = run("m = {\"a\": 1};\necho m[\"z\"];\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::KeyNotFound
    );
}

#[test]
fn index_string_by_position() {
    let r = run("s = \"hello\";\necho s[0];\necho s[4];\n");
    assert_eq!(r.stdout, "h\no\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn index_string_out_of_bounds_is_e0016() {
    let r = run("s = \"hi\";\necho s[5];\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::IndexOutOfBounds
    );
}

#[test]
fn len_dispatches_to_length_trait() {
    // `len(o)` routes to the class's `Length::len`, pushing a receiver-only call frame.
    let r = run(
        "class Stack {\n  items: list\n  fn new(items: list): Stack { return Stack { items: items }; }\n  impl Length {\n    fn len(): int { return self.items.len(); }\n  }\n}\necho Stack.new([1, 2, 3]).len();\n",
    );
    assert_eq!(r.stdout, "3\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn echo_dispatches_to_display_trait() {
    // `echo o` and `"{o}"` route to the class's `Display::to_string` (the `Stringify` op).
    let r = run(
        "class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n  impl Display {\n    fn to_string(): string { return \"P#${self.n}\"; }\n  }\n}\np = P.new(7);\necho p;\necho \"it is ${p}\";\n",
    );
    assert_eq!(r.stdout, "P#7\nit is P#7\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn tuple_construct_project_and_equality() {
    // Object-model slice 4: build a tuple (`MakeTuple`), project positions (`TupleIndex`,
    // including a nested `.0.1`), and compare structurally. Mirrors the tree-walker (the
    // differential oracle guards the agreement).
    let r = run(
        "p = (1, \"two\", 3.0);\necho p;\necho p.1;\nn = ((1, 2), (3, 4));\necho n.1.0;\necho p == (1, \"two\", 3.0);\necho p == (1, \"two\", 4.0);\n",
    );
    assert_eq!(r.stdout, "(1, \"two\", 3.0)\ntwo\n3\ntrue\nfalse\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn match_tuple_patterns() {
    // Object-model slice 4b.2: refutable tuple patterns in `match` — literal, binding, wildcard,
    // and nested tuple positions all compose (the `MatchTuple` test + `TupleIndex` extraction).
    let r = run(
        "fn f(p: (int, int)): string { return match p { (0, 0) => \"o\", (0, y) => \"y${y}\", (x, _) => \"x${x}\" }; }\necho f((0, 0));\necho f((0, 7));\necho f((3, 9));\necho match (1, (\"a\", true)) { (n, (s, b)) => \"${n}/${s}/${b}\" };\n",
    );
    assert_eq!(r.stdout, "o\ny7\nx3\n1/a/true\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn enum_method_and_impl_dispatch() {
    // Object-model slice 3: an enum's unified body. An instance method (`label`) takes the whole
    // value as `self`; `echo`/`${}` route to an `impl Display { to_string }`; and `==` routes to
    // an `impl Equatable { eq }` — all through the same `(type, method)` table an object uses.
    let r = run(
        "enum Color {\n  Red;\n  Green;\n  fn label(): string { return match self { Color.Red => \"r\", Color.Green => \"g\" }; }\n  impl Display { fn to_string(): string { return \"<${self.label()}>\"; } }\n  impl Equatable { fn eq(other: Color): bool { return true; } }\n}\necho Color.Red.label();\necho Color.Red;\necho Color.Red == Color.Green;\n",
    );
    assert_eq!(r.stdout, "r\n<r>\ntrue\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn derived_to_json_serializes_structurally() {
    // `@derive(Serialize<Json>)` synthesizes `to_json`: fields in declared order, strings
    // escaped, nested objects recursed — computed inline (no call frame).
    let r = run(
        "@derive(Serialize<Json>)\nclass U {\n  name: string\n  id: int\n  fn new(name: string, id: int): U { return U { name: name, id: id }; }\n}\necho U.new(\"Ada\", 7).to_json();\n",
    );
    assert_eq!(r.stdout, "{\"name\":\"Ada\",\"id\":7}\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn for_dispatches_to_iterable_trait() {
    // `for x in o` routes to the class's `Iterable::iter`, iterating its returned list.
    let r = run(
        "class Bag {\n  items: list\n  fn new(items: list): Bag { return Bag { items: items }; }\n  impl Iterable {\n    fn iter(): list { return self.items; }\n  }\n}\nmut total = 0;\nfor x in Bag.new([1, 2, 3]) { total = total + x; }\necho total;\n",
    );
    assert_eq!(r.stdout, "6\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn iterable_returning_non_list_is_e0007() {
    let r = run(
        "class B {\n  x: int\n  fn new(): B { return B { x: 1 }; }\n  impl Iterable { fn iter(): int { return 5; } }\n}\nfor v in B.new() { echo v; }\n",
    );
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn object_without_display_uses_structural_render() {
    // No `Display` impl ⇒ the `Stringify` op is identity and the structural form prints.
    let r =
        run("class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n}\necho P.new(7);\n");
    assert_eq!(r.stdout, "P {n: 7}\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn plain_enum_construction_and_equality() {
    let r = run("enum S { A; B; }\necho S.A == S.A;\necho S.A == S.B;\n");
    assert_eq!(r.stdout, "true\nfalse\n");
}

#[test]
fn opaque_use_stub_constructs_and_reads_fields() {
    let r = run(
        "use App.Models.User;\nu = User { name: \"Ada\", id: 7 };\necho u.name;\necho u.id;\necho u;\n",
    );
    // Opaque objects display their fields in sorted-key order (M0 `BTreeMap` parity).
    assert_eq!(r.stdout, "Ada\n7\nUser {id: 7, name: \"Ada\"}\n");
}

#[test]
fn match_over_enums_binds_variant_data() {
    let r = run(
        "enum E { Empty; Code(n: int); }\nx = E.Code(42);\necho match x { E.Empty => \"empty\", E.Code(n) => \"code ${n}\" };\n",
    );
    assert_eq!(r.stdout, "code 42\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn match_literals_and_wildcard() {
    let r = run(
        "fn name(n) { return match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }; }\necho name(0);\necho name(5);\n",
    );
    assert_eq!(r.stdout, "zero\nmany\n");
}

#[test]
fn unmatched_value_is_a_runtime_error() {
    let r = run("enum E { A; B; C; }\necho match E.C { E.A => 1, E.B => 2 };\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn result_constructors_display_bare() {
    let r = run("echo Ok(5);\necho Err(\"boom\");\necho some(3);\necho none;\necho Ok();\n");
    assert_eq!(r.stdout, "Ok(5)\nErr(boom)\nsome(3)\nnone\nOk\n");
}

#[test]
fn question_propagates_err_and_unwraps_ok() {
    assert_eq!(
        run("fn validate(): int { return Err(\"empty\"); }\nfn run_it(): int { validate()?; return Ok(\"done\"); }\necho run_it();\n").stdout,
        "Err(empty)\n"
    );
    assert_eq!(
        run("fn ok_val(): int { return Ok(41); }\nfn use_it(): int { return Ok(ok_val()? + 1); }\necho use_it();\n").stdout,
        "Ok(42)\n"
    );
}

#[test]
fn coalesce_supplies_a_default() {
    let r = run("echo none ?? 99;\necho some(7) ?? 99;\necho Err(\"x\") ?? 0;\necho Ok(5) ?? 0;\n");
    assert_eq!(r.stdout, "99\n7\n0\n5\n");
}

#[test]
fn panic_aborts_with_e0010_keeping_prior_output() {
    let r = run("echo \"before\";\npanic(\"boom\");\necho \"after\";\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(r.stdout, "before\n");
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::Panic
    );
}

#[test]
fn next_id_is_a_deterministic_counter() {
    let r = run("use std.id.{next_id}\necho next_id();\necho next_id();\necho next_id();\n");
    assert_eq!(r.stdout, "1\n2\n3\n");
}

#[test]
fn capture_free_closure_inside_a_method_is_supported() {
    // The `fn(it) => it.price * it.qty` closure captures nothing enclosing, so it compiles
    // even though it is defined inside a method (true upvalue capture stays unsupported).
    let r = run(
        "struct Item { price: float qty: int }\nclass Cart {\n  items: List<Item>\n  fn new(items: List<Item>): Cart { return Cart { items: items }; }\n  fn total(): float { return self.items.map(fn(it) => it.price * it.qty).sum(); }\n}\nc = Cart.new([Item { price: 2.5, qty: 4 }, Item { price: 1.0, qty: 3 }]);\necho c.total();\n",
    );
    assert_eq!(r.stdout, "13.0\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn string_interpolation_concatenates_display_forms() {
    let r = run("name = \"Niro\";\necho \"Hello ${name}\";\necho \"sum is ${1 + 2 * 3}\";\n");
    assert_eq!(r.stdout, "Hello Niro\nsum is 7\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn list_literals_display_with_repr() {
    let r = run("echo [1, 2, 3];\necho [\"a\", \"b\"];\necho [];\n");
    assert_eq!(r.stdout, "[1, 2, 3]\n[\"a\", \"b\"]\n[]\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn maps_display_in_sorted_key_order() {
    let r = run("echo {\"b\": 2, \"a\": 1};\necho {\"a\": 1, \"b\": 2}.len();\n");
    assert_eq!(r.stdout, "{\"a\": 1, \"b\": 2}\n2\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn len_over_list_map_and_string() {
    let r = run(
        "echo [1, 2, 3].len();\necho {\"a\": 1}.len();\necho \"héllo\".len();\necho [].len();\n",
    );
    assert_eq!(r.stdout, "3\n1\n5\n0\n");
}

#[test]
fn filter_map_sum_pipeline() {
    let r = run("echo [1, 2, 3, 4].filter(fn(n) => n % 2 == 0).map(fn(n) => n * 10).sum();\n");
    assert_eq!(r.stdout, "60\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn sum_promotes_to_float_when_any_element_is_float() {
    assert_eq!(run("echo [1, 2, 3].sum();\n").stdout, "6\n");
    assert_eq!(run("echo [1, 2.5, 3].sum();\n").stdout, "6.5\n");
    assert_eq!(run("echo [].sum();\n").stdout, "0\n");
}

#[test]
fn for_over_list_accumulates_into_a_global() {
    let r = run("mut total = 0;\nfor n in [1, 2, 3, 4] {\n  total = total + n;\n}\necho total;\n");
    assert_eq!(r.stdout, "10\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn for_over_empty_list_runs_no_iterations() {
    let r = run("for x in [] { echo \"never\"; }\necho \"done\";\n");
    assert_eq!(r.stdout, "done\n");
}

#[test]
fn for_pair_destructures_enumerate() {
    let r = run("for (i, x) in [\"a\", \"b\"].enumerate() {\n  echo i ~ \":\" ~ x;\n}\n");
    assert_eq!(r.stdout, "0:a\n1:b\n");
    assert_eq!(r.exit_code, 0);
}

#[test]
fn for_over_map_iterates_values_in_key_order() {
    let r = run(
        "mut total = 0;\nfor v in {\"b\": 20, \"a\": 1} {\n  total = total + v;\n}\necho total;\n",
    );
    assert_eq!(r.stdout, "21\n");
}

#[test]
fn iterating_a_non_collection_is_a_type_error() {
    let r = run("for x in 42 { echo x; }\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::TypeMismatch
    );
}

#[test]
fn len_of_an_int_is_an_unknown_method() {
    // `len` is a collection method (P1.2), so on an int it is an unknown method (E0005) — the
    // same error every other unknown method raises (the old free `len(42)` was a TypeMismatch).
    let r = run("echo (42).len();\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::UnknownName
    );
}

#[test]
fn map_closure_error_propagates_and_frees() {
    // The closure divides by zero on the second element: the error must surface and the
    // partially-built result list must be freed (miri verifies no leak).
    let r = run("echo [1, 0, 2].map(fn(n) => 10 / n);\n");
    assert_eq!(r.exit_code, 1);
    assert_eq!(
        r.diagnostics[0].code,
        noeta_diagnostics::DiagnosticCode::DivisionByZero
    );
}

#[test]
fn nested_list_of_lists_round_trips() {
    // Exercises recursive collection freeing through the register/global machinery.
    let r = run("xs = [[1, 2], [3, 4]];\necho xs;\necho xs.len();\n");
    assert_eq!(r.stdout, "[[1, 2], [3, 4]]\n2\n");
}

#[test]
fn disassembly_is_stable() {
    let source = Source::new(SourceId::FIRST, "t.noe", "mut x = 1;\necho x + 2;\n");
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn attribute_manifest_records_decorations() {
    // `#[...]` data attributes (with literal args) are collected into the queryable
    // build manifest, in source order, keyed by the decorated type.
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "#[Entity]\n#[Route(login, post)]\nclass Account {\n  id: int\n  fn new(id: int): Account { return Account { id: id }; }\n}\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    let attrs: Vec<_> = module.attributes_for("Account").collect();
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[0].name, "Entity");
    assert!(attrs[0].args.is_empty());
    assert_eq!(attrs[1].name, "Route");
    let arg_values: Vec<_> = attrs[1].args.iter().map(|a| a.value.clone()).collect();
    assert_eq!(
        arg_values,
        vec![
            noeta_ast::AttrValue::TypeRef {
                name: noeta_ast::Name::written("login"),
                args: Vec::new()
            },
            noeta_ast::AttrValue::TypeRef {
                name: noeta_ast::Name::written("post"),
                args: Vec::new()
            },
        ]
    );
    // A type with no attributes has no manifest entries.
    assert_eq!(module.attributes_for("Missing").count(), 0);
}

#[test]
fn disassembly_of_a_recursive_function_is_stable() {
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(6);\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn disassembly_of_a_for_loop_is_stable() {
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "mut total = 0;\nfor n in [1, 2, 3] {\n  total = total + n;\n}\necho total;\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn disassembly_of_the_object_model_is_stable() {
    // A struct literal, a class with a constructor + an instance method (showing the
    // shape and method tables, field loads, and enum construction).
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "enum Status { Pending; Paid; }\nclass Order {\n  id: int\n  mut status: Status\n  fn new(id: int): Order { return Order { id: id, status: Status.Pending }; }\n  fn tag(): int { return self.id; }\n}\no = Order.new(7);\necho o.tag();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn local_self_update_lowers_to_in_place_record_reuse() {
    // Phase 5.1a: a self-update of a destructor-free type whose accumulator is a directly-held
    // **function-local** must lower to the in-place `MakeStructInPlace` (the reuse pass marks it,
    // the compiler emits it) rather than a copying `MakeStruct` — the proof the reuse token reaches
    // the VM. (A top-level global accumulator is the `TakeGlobal` case — see
    // `global_self_update_lowers_to_take_global_plus_in_place_reuse`.)
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "class P { x: int }\nfn run(): int {\n  mut acc = P { x: 0 };\n  acc = P { ...acc, x: acc.x + 1 };\n  return acc.x;\n}\necho run();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    let disasm = module.disassemble();
    assert!(
        disasm.contains("MakeRecIP"),
        "expected an in-place record-reuse op, got:\n{disasm}"
    );
}

#[test]
fn global_self_update_lowers_to_take_global_plus_in_place_reuse() {
    // Phase 5.1b: a top-level (global) struct accumulator's self-update must move the global out
    // with `TakeGlobal` and reuse it in place with `MakeStructInPlace` — not the copying
    // `MakeStruct` the local-only 5.1a path fell back to for a global. Both ops together are the
    // proof the global path is wired.
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "class P { x: int }\nmut acc = P { x: 0 };\nacc = P { ...acc, x: 5 };\necho acc.x;\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    let disasm = module.disassemble();
    assert!(
        disasm.contains("TakeGlobal") && disasm.contains("MakeRecIP"),
        "expected TakeGlobal + in-place record reuse for a global accumulator, got:\n{disasm}"
    );
}

#[test]
fn local_map_self_update_lowers_to_reuse_method_call() {
    // Phase 5.1c: a function-local map accumulator updated with `m[k] = v` (desugaring to
    // `m = m.set(k, v)`) must carry the in-place-reuse token to the VM — `CallMethod ... [reuse]` —
    // so the dispatch mutates the uniquely-owned backing map in place rather than copying it. A
    // top-level (global) map accumulator is the `TakeGlobal` case (a later slice; the IR
    // interpreter already reuses it, and reuse is invisible, so the backends still agree).
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "fn build(): Map<string, int> {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = i; }\n  return m;\n}\necho build().len();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    let disasm = module.disassemble();
    assert!(
        disasm.contains("[reuse"),
        "expected a reuse-marked method call for a local map self-update, got:\n{disasm}"
    );
}

#[test]
fn self_append_lowers_to_in_place_concat() {
    // Phase 5.1b: a list self-append `acc ~= rhs` must lower to `ConcatInPlace` — for a global
    // accumulator preceded by `TakeGlobal` (to expose unique ownership), and for a function-local
    // accumulator directly on its register. The proof the concat reuse token reaches the VM rather
    // than the copying `Op::Binary` (`~`).
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "mut g = [\"a\"];\ng ~= [\"b\"];\nfn build(): List<int> {\n  mut acc = [];\n  for i in 0..3 { acc ~= [i]; }\n  return acc;\n}\necho g;\necho build();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    let disasm = module.disassemble();
    assert_eq!(
        disasm.matches("ConcatIP").count(),
        2,
        "expected two in-place concats (global + local), got:\n{disasm}"
    );
    assert!(
        disasm.contains("TakeGlobal"),
        "expected the global self-append to be preceded by TakeGlobal, got:\n{disasm}"
    );
}

#[test]
fn disassembly_of_a_match_decision_tree_is_stable() {
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "enum E { Empty; Code(n: int); }\nfn describe(e): string {\n  return match e {\n    E.Empty => \"empty\",\n    E.Code(n) => \"code ${n}\",\n  };\n}\necho describe(E.Code(7));\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn disassembly_of_a_question_propagating_function_is_stable() {
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "fn validate(): int { return Err(\"bad\"); }\nfn place(): int { validate()?; return Ok(\"ok\"); }\necho place();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn disassembly_of_a_map_filter_chain_is_stable() {
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "echo [1, 2, 3, 4].filter(fn(n) => n % 2 == 0).map(fn(n) => n * 10).sum();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn disassembly_of_local_bindings_consumes_temporaries() {
    // Each local declaration's value is a single-use temporary, so the local *adopts* the
    // temporary's register (a consuming move, Phase 3.3b) instead of a retaining `Op::Move` into
    // a fresh slot: the body holds no `Move` between the producing `Add` and the binding, and
    // `registers` stays small. A borrowed source (`y = x`, an aliased live local) still copies.
    let source = Source::new(
        SourceId::FIRST,
        "t.noe",
        "fn build(): int {\n  a = 1 + 2;\n  b = a + 3;\n  return b;\n}\necho build();\n",
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = compile(&parsed.program).unwrap();
    insta::assert_snapshot!(module.disassemble());
}

#[test]
fn closure_default_reads_a_captured_cell() {
    // A closure default that references a captured variable the body never otherwise names: the
    // default thunk shares the closure's upvalue layout and reads the captured cell. Exercises
    // the run_thunk upvalue-retain path (miri verifies no leak / double-free).
    let r = run(
        "fn make(tag: string): dyn {\n  return fn(s: string, label: string = tag) => label ~ \":\" ~ s;\n}\nt = make(\"X\");\necho t(\"a\");\necho t(\"a\", \"Y\");\n",
    );
    assert_eq!(r.stdout, "X:a\nY:a\n");
    assert_eq!(r.exit_code, 0);
}

/// The **line-count ratchet** on `lib.rs` (audit-1 finding 1). The 2025 split
/// (`plans/code-quality/split-vm-lib.md`) took lib.rs 7,733 -> 5,729 lines, but nothing held the
/// line: five later arcs (tier-1 glue, JIT engine mgmt, hot-swap, isolate workers, the
/// `run_module_*` family) each defaulted their code into lib.rs and it regrew to 10,685. The
/// 2026 re-split moved those into `hooks`/`backend`/`tier1`/`lifecycle`/`dispatch`/`hotswap`/
/// `calls`/`tests`, leaving lib.rs at ~580 lines (crate docs, module decls + re-exports, the
/// `Vm` struct + its grouped sub-structs, `Frame`/`RetTransform`/`Abort`, constants). The budget
/// is that figure plus ~10% headroom for doc growth: a NEW SUBSYSTEM BELONGS IN ITS OWN MODULE,
/// not here — if this fires, move the addition out rather than raising the budget.
///
/// The sub-structs have since been drifting out to the modules that own them, which is the same
/// rule applied one level down and is what keeps the headroom from being spent on them: `SchedState`
/// lives in `scheduler`, and `IsolateState` in `lifecycle` beside `IsolateSlot`/`IsolateOutcome`/
/// `run_isolate_worker` (isolate-cancel — two more worker fields were what tipped the ratchet, and
/// moving the struct was the fix the message asks for).
#[test]
fn lib_rs_stays_decomposed() {
    const BUDGET: usize = 640;
    let lines = include_str!("lib.rs").lines().count();
    assert!(
        lines <= BUDGET,
        "src/lib.rs is {lines} lines (budget {BUDGET}). The god-file is regrowing — land new \
         subsystems in their own module (see the module map at the top of lib.rs and \
         plans/audit/audit-1-vm-runtime.md finding 1) instead of raising the budget."
    );
}

/// Isolates I.4b worker-teardown gap: a worker isolate that strands reference cycles
/// (`a.next = b; b.next = a` on a reference `class`) must reap them at its **own** teardown —
/// refcounting alone never reclaims a cycle, and before the fix the worker's teardown ran no cycle
/// pass, so the cycle (and its `__destruct`) leaked until the thread died. The value heap is
/// thread-local, so `live_count()` measured on the worker's own thread *is* the worker heap's
/// residency; it must return to the pre-run baseline (delta 0), exactly as the main heap's
/// [`Vm::teardown`] guarantees. Drives `run_isolate_worker` directly (real isolates are CLI/
/// out-of-oracle, so the differential leak oracle never samples this path).
#[test]
fn worker_teardown_reaps_stranded_reference_cycles() {
    use std::sync::Arc;
    let _ = noeta_stdlib::registry::default_seeded();
    let src = "class Node { pub mut next: ?Node\n\
         fn new(): Node { return Node { next: none } } }\n\
         fn spin(count: int): int {\n\
         mut i = 0\n\
         while i < count {\n\
         a = Node.new()\n\
         b = Node.new()\n\
         a.next = some(b)\n\
         b.next = some(a)\n\
         i = i + 1\n\
         }\n\
         return i\n\
         }\n";
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = Arc::new(compile(&parsed.program).expect("in subset"));
    let proto = module
        .protos
        .iter()
        .position(|p| p.name.as_deref() == Some("spin"))
        .expect("spin proto") as u32;
    let factory: crate::IsolateFactory = Arc::new(|| {
        (
            Box::new(noeta_stdlib::SandboxHost::new()) as Box<dyn noeta_stdlib::Host>,
            Box::new(noeta_stdlib::SandboxExecutor::new()) as Box<dyn noeta_stdlib::Executor>,
        )
    });
    let residual = std::thread::spawn(move || {
        let before = noeta_value::live_count();
        let result = crate::lifecycle::run_isolate_worker(
            &module,
            &factory,
            None,
            proto,
            vec![crate::isolate::IsoArg::Copied(crate::isolate::Wire::Int(5))],
            Vec::new(),
            Vec::new(),
            None,
            None,
            false,
            // Never cancelled here: this test is about the worker's teardown, and a cancelled
            // worker's teardown is exercised end to end by the CLI isolate tests instead.
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            noeta_span::Span::new(0, 0),
        );
        assert!(
            matches!(result, crate::lifecycle::IsolateOutcome::Done(_)),
            "spin returns an int"
        );
        noeta_value::live_count() as i64 - before as i64
    })
    .join()
    .unwrap();
    assert_eq!(
        residual, 0,
        "the worker's own teardown must reap its stranded cycles (residency 0)"
    );
}

/// isolate-cancel: a worker that observes its cancellation flag reports
/// [`IsolateOutcome::Cancelled`] — not a failure, and not a value — and tears its own heap down to
/// the same zero residency a completed worker leaves. Drives `run_isolate_worker` directly, the
/// only in-crate way to reach the real worker path (real isolates are CLI / out-of-oracle, so the
/// differential leak oracle never samples it).
///
/// The flag is set **before** the worker starts, so the stop is deterministic: the body's very first
/// safepoint — the dispatch loop's entry frame transfer — sees it, with no timing to race. What that
/// leaves untested here is *where* a mid-run cancel lands, which the CLI tests measure end to end
/// (a 40M-iteration loop cancelled 200 ms in, stopping in milliseconds rather than seconds).
#[test]
fn a_cancelled_worker_reports_cancelled_and_frees_its_heap() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let _ = noeta_stdlib::registry::default_seeded();
    let src = "class Node { pub mut next: ?Node\n\
         fn new(): Node { return Node { next: none } } }\n\
         fn spin(count: int): int {\n\
         mut i = 0\n\
         while i < count {\n\
         a = Node.new()\n\
         b = Node.new()\n\
         a.next = some(b)\n\
         b.next = some(a)\n\
         i = i + 1\n\
         }\n\
         return i\n\
         }\n";
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let module = Arc::new(compile(&parsed.program).expect("in subset"));
    let proto = module
        .protos
        .iter()
        .position(|p| p.name.as_deref() == Some("spin"))
        .expect("spin proto") as u32;
    let factory: crate::IsolateFactory = Arc::new(|| {
        (
            Box::new(noeta_stdlib::SandboxHost::new()) as Box<dyn noeta_stdlib::Host>,
            Box::new(noeta_stdlib::SandboxExecutor::new()) as Box<dyn noeta_stdlib::Executor>,
        )
    });
    let cancel = Arc::new(AtomicBool::new(true));
    let (outcome, residual) = std::thread::spawn(move || {
        let before = noeta_value::live_count();
        let outcome = crate::lifecycle::run_isolate_worker(
            &module,
            &factory,
            None,
            proto,
            vec![crate::isolate::IsoArg::Copied(crate::isolate::Wire::Int(
                1_000_000,
            ))],
            Vec::new(),
            Vec::new(),
            None,
            None,
            false,
            cancel,
            noeta_span::Span::new(0, 0),
        );
        let residual = noeta_value::live_count() as i64 - before as i64;
        (outcome, residual)
    })
    .join()
    .unwrap();
    assert!(
        matches!(outcome, crate::lifecycle::IsolateOutcome::Cancelled),
        "an honored cancellation is its own outcome, not a value and not a failure"
    );
    assert_eq!(
        residual, 0,
        "a cancelled worker tears its heap down exactly like a completed one (residency 0)"
    );
}
