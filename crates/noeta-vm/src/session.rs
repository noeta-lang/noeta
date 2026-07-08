//! The REPL session on the VM (REPL-on-VM R1).
//!
//! A [`VmSession`] runs many REPL entries against **one persistent runtime** so bindings, `fn` /
//! `type` / `enum` / `class` declarations, channels, the reactive graph, and the host's id / entropy /
//! clock state survive between entries — the same continuity the tree-walker `noeta_eval::Session`
//! gave the REPL, now on the production VM so the oracle backend can be cut from the shipped binary.
//! (The `next_id()` counter now lives on the `Host`, which the session persists, so it carries across
//! entries for free.)
//!
//! The design keeps the hot path untouched (see `plans/repl-on-vm`): the `Vm` struct and its dispatch
//! loop are unchanged. Each entry builds an **ephemeral** [`Vm`] over the session's persistent
//! [`SessionState`] via [`Vm::load_seeded`], runs the entry chunk with [`Vm::run_top`] (no teardown),
//! then moves the state back out with [`Vm::into_state`]. One [`Vm::teardown`] runs at session end,
//! bringing heap residency to zero.

use std::rc::Rc;

use noeta_ast::{Program, Stmt};
use noeta_backend::TraceFrame;
use noeta_bytecode::{Module, PackedFieldDef};
use noeta_compiler::SessionCompiler;
use noeta_diagnostics::Diagnostic;
use noeta_object::{PackedKind, PackedSchema, Shape};
use noeta_span::{SourceId, Span};
use noeta_stdlib::{Executor, Host};
use noeta_value::Value;

use crate::{Channel, Vm, release};

/// A factory for a fresh host + executor pair — the session builds one at construction and again on
/// `:reset`, so a reset REPL starts against the same *kind* of environment (a real host, or the
/// deterministic sandbox) without the session having to know which. Mirrors the isolate factory.
pub type HostFactory = Box<dyn Fn() -> (Box<dyn Host>, Box<dyn Executor>)>;

/// The persistent runtime state carried between REPL entries: everything the ephemeral per-entry
/// [`Vm`] inherits so a first entry's effects survive into the next. Moved into the `Vm` at the start
/// of an entry ([`Vm::load_seeded`]) and back out at the end ([`Vm::into_state`]); [`Vm::teardown`]
/// consumes it at session end.
pub(crate) struct SessionState {
    globals: Vec<Value>,
    global_order: Vec<u32>,
    channels: Vec<Channel>,
    channel_progress: u64,
    /// The extensions' persistent runtime (higher-order-abi H4/H5): the retained-value arena
    /// (signals' contents, cells) plus per-extension Rust state (the reactive graph) and gates —
    /// what the pre-H5 `Rc<ReactiveGraph>` field carried, generalized.
    ext_arena: Vec<Option<Value>>,
    ext_arena_free: Vec<u32>,
    ext_state: Vec<(&'static str, noeta_stdlib::ExtState)>,
    ext_closed_gates: Vec<&'static str>,
    /// The `Rc`-wrapped derived tables grow by **append** (never rebuild), so an entry-1 aggregate and
    /// an entry-2 aggregate of the same type share `&'static Shape` identity — the invariant the reuse
    /// gate, packed-value ops, and inline caches assume within a single run. [`SessionState::sync_to`]
    /// extends them to a grown module; the existing prefix keeps its identity.
    shapes: Vec<&'static Shape>,
    packed_schemas: Vec<&'static PackedSchema>,
    type_reprs: Vec<Rc<noeta_ast::reflect::TypeRepr>>,
    host: Box<dyn Host>,
    executor: Box<dyn Executor>,
}

impl SessionState {
    /// A fresh runtime: empty tables, id counter at 1, and the given host + executor.
    fn fresh(host: Box<dyn Host>, executor: Box<dyn Executor>) -> SessionState {
        SessionState {
            globals: Vec::new(),
            global_order: Vec::new(),
            channels: Vec::new(),
            channel_progress: 0,
            ext_arena: Vec::new(),
            ext_arena_free: Vec::new(),
            ext_state: Vec::new(),
            ext_closed_gates: Vec::new(),
            shapes: Vec::new(),
            packed_schemas: Vec::new(),
            type_reprs: Vec::new(),
            host,
            executor,
        }
    }

    /// Grow the append-only derived tables to match `module` after an entry added new types. New
    /// shapes / packed-schemas / type-reprs are built here (fresh `Rc`s); the existing prefix is
    /// untouched, so old values keep their shape identity. New global slots start unbound.
    fn sync_to(&mut self, module: &Module) {
        if module.global_names.len() > self.globals.len() {
            self.globals
                .resize(module.global_names.len(), Value::unbound());
        }
        for shape in &module.shapes[self.shapes.len()..] {
            self.shapes.push(noeta_object::intern_shape(shape.clone()));
        }
        // Packed schemas are interned inner-before-outer, so a new schema's referenced shape index and
        // nested-schema index are already present in the grown tables above / earlier in this loop.
        for def in &module.packed_schemas[self.packed_schemas.len()..] {
            let fields = def
                .fields
                .iter()
                .map(|f| match f {
                    PackedFieldDef::Int => PackedKind::Int,
                    PackedFieldDef::Float => PackedKind::Float,
                    PackedFieldDef::F32 => PackedKind::F32,
                    PackedFieldDef::Bool => PackedKind::Bool,
                    PackedFieldDef::Struct(idx) => {
                        PackedKind::Struct(self.packed_schemas[*idx as usize])
                    }
                })
                .collect();
            self.packed_schemas
                .push(noeta_object::intern_schema(PackedSchema {
                    shape: self.shapes[def.shape as usize],
                    fields,
                    byte_size: def.byte_size as usize,
                    column: def.column,
                }));
        }
        for repr in &module.type_reprs[self.type_reprs.len()..] {
            self.type_reprs.push(Rc::new(repr.clone()));
        }
    }
}

impl<'m> Vm<'m> {
    /// Build a `Vm` for one REPL entry, seeded with the session's persistent `state` instead of a
    /// fresh heap. The caller has already `sync_to`'d `state`'s derived tables to `module`. Reuses
    /// [`Vm::load`] for the module-derived *name* tables (methods / destructors / …) and all per-entry
    /// scratch, then swaps in the persistent globals, id counter, channels, reactive graph, and the
    /// identity-preserving `Rc` tables (discarding the fresh ones `load` built — cheap at an
    /// interactive prompt, and it keeps `load_seeded` in lockstep with `load`'s field init).
    fn load_seeded(module: &'m Module, state: SessionState) -> Vm<'m> {
        let mut vm = Vm::load(module, state.host, state.executor);
        vm.globals = state.globals;
        vm.global_order = state.global_order;
        vm.channels = state.channels;
        vm.channel_progress = state.channel_progress;
        vm.ext_arena = state.ext_arena;
        vm.ext_arena_free = state.ext_arena_free;
        vm.ext_state = state.ext_state;
        vm.ext_closed_gates = state.ext_closed_gates;
        vm.shapes = state.shapes;
        vm.packed_schemas = state.packed_schemas;
        vm.type_reprs = state.type_reprs;
        // `map_packed` references packed schemas by index; rebuild it against the seeded (persistent)
        // schemas so an old span still resolves to the same shared schema.
        vm.map_packed = module
            .map_packed_sites
            .iter()
            .map(|(span, idx)| (*span, vm.packed_schemas[*idx as usize]))
            .collect();
        vm
    }

    /// Move the persistent runtime state back out of the `Vm` after an entry ran (the ephemeral `Vm`
    /// is then dropped; its per-entry scratch — empty scopes, drained stdout/diagnostics, no isolates
    /// — drops cleanly, `Vm` having no `Drop`). The next entry re-seeds from this.
    fn into_state(self) -> SessionState {
        SessionState {
            globals: self.globals,
            global_order: self.global_order,
            channels: self.channels,
            channel_progress: self.channel_progress,
            ext_arena: self.ext_arena,
            ext_arena_free: self.ext_arena_free,
            ext_state: self.ext_state,
            ext_closed_gates: self.ext_closed_gates,
            shapes: self.shapes,
            packed_schemas: self.packed_schemas,
            type_reprs: self.type_reprs,
            host: self.host,
            executor: self.executor,
        }
    }
}

/// The outcome of one [`VmSession::eval`]: this entry's stdout, diagnostics, the display form of a
/// trailing bare expression (for the REPL to echo), and the abort traceback if it panicked. Mirrors
/// `noeta_eval::SessionOutput` field-for-field so the CLI's REPL rendering is backend-agnostic and the
/// R2 session differential can compare the two directly.
#[derive(Debug, Clone)]
pub struct SessionOutput {
    pub stdout: String,
    pub diagnostics: Vec<Diagnostic>,
    pub value: Option<String>,
    /// The abort traceback if this entry panicked (empty otherwise), innermost frame first. A frame
    /// from a function defined in an *earlier* entry carries a span into that entry's now-gone text;
    /// the CLI's renderer degrades such a frame to name-only.
    pub trace: Vec<TraceFrame>,
}

/// A persistent REPL session on the bytecode VM. Owns the incremental [`SessionCompiler`] and the
/// [`SessionState`]; each [`VmSession::eval`] compiles one entry against the accumulated tables and
/// runs it against the persistent globals.
pub struct VmSession {
    compiler: SessionCompiler,
    factory: HostFactory,
    /// `Some` between entries; taken (and put back) transiently inside [`VmSession::eval`].
    state: Option<SessionState>,
}

impl std::fmt::Debug for VmSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmSession")
            .field("compiler", &self.compiler)
            .finish_non_exhaustive()
    }
}

impl VmSession {
    /// Start a session whose entries run against host + executor pairs from `factory` (a real host for
    /// `noeta repl`, the deterministic sandbox for tests / the session differential). The factory is
    /// called once now and again on [`VmSession::reset`].
    pub fn new(factory: HostFactory) -> VmSession {
        let (host, executor) = factory();
        VmSession {
            compiler: SessionCompiler::new(),
            factory,
            state: Some(SessionState::fresh(host, executor)),
        }
    }

    /// A session **adopted from a checked compile** (tooling-unification T3): run `module` — the
    /// snapshot [`noeta_compiler::compile_with_sites_session`] returned alongside `compiler` — to
    /// completion as entry 0, then continue the session incrementally from its final state. A
    /// fragment evaluated afterwards resolves the checked program's globals, functions, types, and
    /// methods by their **original ids** (the compiler's tables are the module's own id-spaces), and
    /// values entry 0 created keep full `Rc<Shape>` identity in later entries. The initial run is
    /// fully checked; fragments are checkerless, exactly like REPL entries.
    ///
    /// Returns the session plus entry 0's output (its stdout/diagnostics/trace — a debug console or
    /// a future `repl --load` replays these before the first prompt).
    pub fn adopted(
        module: &Module,
        compiler: SessionCompiler,
        factory: HostFactory,
    ) -> (VmSession, SessionOutput) {
        let (host, executor) = factory();
        let mut state = SessionState::fresh(host, executor);
        state.sync_to(module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load_seeded(module, state);
        vm.run_top();
        let stdout = std::mem::take(&mut vm.stdout);
        let diagnostics = std::mem::take(&mut vm.diagnostics);
        let trace = std::mem::take(&mut vm.abort_trace);
        let session = VmSession {
            compiler,
            factory,
            state: Some(vm.into_state()),
        };
        (
            session,
            SessionOutput {
                stdout,
                diagnostics,
                value: None,
                trace,
            },
        )
    }

    /// Evaluate one REPL entry against the persistent scope. Returns this entry's stdout, diagnostics,
    /// and — when the final statement is a bare expression — the display form of its non-unit value.
    ///
    /// The entry is compiled by the incremental [`SessionCompiler`] (checkerless, matching the
    /// tree-walker REPL), its derived tables are appended to the persistent state, and its entry chunk
    /// runs against the shared globals with **no teardown**, so bindings and declarations survive into
    /// the next entry.
    pub fn eval(&mut self, program: &Program) -> SessionOutput {
        // Echo a trailing bare expression's **non-unit** value in its display form (`1 + 2` → `3`).
        self.run_capturing(program, None, |v| (!v.is_unit()).then(|| v.display()))
    }

    /// [`VmSession::eval`] with the checker's accumulated [`Sites`] bundle (session-checker C5):
    /// the entry compiles through [`SessionCompiler::extend_checked`] — the same site-driven
    /// codegen the file pipeline runs (packed lists, `type_of` full fidelity, method handles,
    /// precise destructor relevance). The caller is responsible for the soundness gate: only a
    /// session the checker has seen IN FULL may take this path (precise relevance from a registry
    /// that missed an unchecked entry's destructor class could skip a destructor).
    ///
    /// [`Sites`]: noeta_compiler::Sites
    pub fn eval_checked(
        &mut self,
        program: &Program,
        sites: &noeta_compiler::Sites,
    ) -> SessionOutput {
        self.run_capturing(program, Some(sites), |v| {
            (!v.is_unit()).then(|| v.display())
        })
    }

    /// `:type <expr>` — evaluate `program`'s trailing expression and report its **runtime** type. The
    /// REPL runs no checker across entries, so the type is read from the produced value (like the
    /// language's `type_of`), which means the expression is evaluated and any side effects run. Uses
    /// reflection + the `TypeRepr` surface spelling (`List<int>`), falling back to the runtime kind
    /// name for an untagged primitive — the same rendering the debugger shows.
    pub fn type_of(&mut self, program: &Program) -> SessionOutput {
        // Conservative codegen deliberately: a `:type` query defines nothing worth optimizing, and
        // the conservative path is always sound regardless of the session's checkedness.
        self.run_capturing(program, None, |v| Some(v.type_display()))
    }

    /// Compile and run one entry, then — if the final statement was a bare expression — hand its
    /// captured value to `describe` for the returned `value` field. Shared by [`VmSession::eval`]
    /// (which displays) and [`VmSession::type_of`] (which reports the type). The value is **unbound +
    /// released** after `describe`, so an evaluated value neither lingers as a binding nor pins a
    /// refcount across entries (which would leak it and suppress a later `:drop`'s destructor);
    /// releasing runs its destructor now, matching the tree-walker, which drops it at batch end.
    fn run_capturing(
        &mut self,
        program: &Program,
        sites: Option<&noeta_compiler::Sites>,
        describe: impl FnOnce(Value) -> Option<String>,
    ) -> SessionOutput {
        // A trailing bare expression is rewritten to `mut <sentinel> = expr;` so the IR path captures
        // its value in a global slot we read back below — pure AST surgery, backend-agnostic.
        let (lowerable, captures_value) = rewrite_trailing_expr(program);
        let module = match sites {
            None => self.compiler.extend(&lowerable),
            Some(sites) => self.compiler.extend_checked(&lowerable, sites),
        }
        .expect(
            "checkerless lowering is total over parsed programs (the REPL only feeds parsed input)",
        );

        let mut state = self
            .state
            .take()
            .expect("session state is present between entries");
        state.sync_to(&module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load_seeded(&module, state);
        vm.run_top();

        let value = if captures_value {
            self.sentinel_slot().and_then(|slot| {
                let v = std::mem::replace(&mut vm.globals[slot as usize], Value::unbound());
                // Unbound means the entry errored (or returned) before the sentinel binding ran.
                if v.is_unbound() {
                    return None;
                }
                let described = describe(v);
                // Free the discarded trailing value with a **plain** release (no `destruct`), matching
                // the tree-walker REPL, which drops the extracted value as a host value rather than
                // through the interpreter's destructor path. A *bound* value's destructor still fires —
                // at `:drop` or teardown (which use `release_value`) — just not on a bare-expression echo.
                release(v);
                described
            })
        } else {
            None
        };

        let stdout = std::mem::take(&mut vm.stdout);
        let diagnostics = std::mem::take(&mut vm.diagnostics);
        let trace = std::mem::take(&mut vm.abort_trace);
        self.state = Some(vm.into_state());
        SessionOutput {
            stdout,
            diagnostics,
            value,
            trace,
        }
    }

    /// Run a binding's destructor now and unbind it (`:drop` / `:free`), returning whether a binding
    /// existed. The REPL's top-level bindings are globals with extended lifetime (they never auto-fire
    /// a destructor), so this is how one is observed or an object reclaimed interactively; any
    /// destructor output lands in the returned [`SessionOutput`].
    pub fn drop_binding(&mut self, name: &str) -> (bool, SessionOutput) {
        let Some(slot) = self.compiler.global_slots().get(name).copied() else {
            return (false, SessionOutput::empty());
        };
        // Compile an empty entry to snapshot a module with the accumulated types (so a destructor
        // resolves), then release the bound slot on an ephemeral Vm and extract the state back.
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        let mut state = self.state.take().expect("state present between entries");
        state.sync_to(&module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load_seeded(&module, state);
        let found = {
            let v = std::mem::replace(&mut vm.globals[slot as usize], Value::unbound());
            if v.is_unbound() {
                false
            } else {
                vm.release_value(v);
                true
            }
        };
        let stdout = std::mem::take(&mut vm.stdout);
        let diagnostics = std::mem::take(&mut vm.diagnostics);
        self.state = Some(vm.into_state());
        (
            found,
            SessionOutput {
                stdout,
                diagnostics,
                value: None,
                trace: Vec::new(),
            },
        )
    }

    /// The live **user** binding names (`:bindings`) — every global slot that is currently bound,
    /// excluding the trailing-expression sentinel. (The VM has no prelude *globals* — prelude values
    /// like `Ok` / `panic` are intrinsic, not slots — so unlike the tree-walker there is nothing else
    /// to filter out.)
    pub fn binding_names(&self) -> Vec<String> {
        let state = self.state.as_ref().expect("state present between entries");
        self.compiler
            .global_names()
            .iter()
            .enumerate()
            .filter(|(slot, name)| {
                name.as_str() != SENTINEL
                    && state.globals.get(*slot).is_some_and(|v| !v.is_unbound())
            })
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Reset to a fresh session — a new compiler, globals, id counter, and reactive graph (`:reset`).
    /// Tears the current state down first so residency returns to zero, then rebuilds from the factory.
    pub fn reset(&mut self) {
        self.teardown();
        let (host, executor) = (self.factory)();
        self.compiler = SessionCompiler::new();
        self.state = Some(SessionState::fresh(host, executor));
    }

    /// Tear the session's runtime down, bringing heap residency to zero (destructors fire on the live
    /// globals in reverse binding order). Leaves the session with no state; call at session end, or
    /// before rebuilding in [`VmSession::reset`]. Idempotent — a second call is a no-op.
    pub fn teardown(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        // Snapshot a module with the accumulated types so each global's destructor resolves during
        // teardown, then run the VM's end-of-program teardown on it (globals destroyed in reverse
        // binding order, cycles reaped, channels drained, reactive graph cleared).
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        state.sync_to(&module);
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let mut vm = Vm::load_seeded(&module, state);
        vm.teardown(mode);
    }

    /// The global slot the trailing-expression sentinel is interned at, once any entry has used one.
    fn sentinel_slot(&self) -> Option<u32> {
        self.compiler.global_slots().get(SENTINEL).copied()
    }
}

impl SessionOutput {
    fn empty() -> SessionOutput {
        SessionOutput {
            stdout: String::new(),
            diagnostics: Vec::new(),
            value: None,
            trace: Vec::new(),
        }
    }
}

/// The reserved binding name a trailing bare REPL expression is rewritten into, so the IR path
/// captures its value in a persistent global slot. Contains a NUL so it can never collide with a user
/// identifier and never appears in `:bindings`.
const SENTINEL: &str = "\0repl-value";

/// If `program`'s final statement is a bare expression, return a copy with that statement rewritten to
/// `mut <SENTINEL> = <expr>;` (so the IR path captures its value) and `true`; otherwise return the
/// program unchanged and `false`. Only the trailing statement is touched — earlier bare expressions
/// stay discarded statements.
fn rewrite_trailing_expr(program: &Program) -> (Program, bool) {
    match program.stmts.last() {
        Some(Stmt::Expr { expr, span }) => {
            let mut stmts = program.stmts.clone();
            *stmts.last_mut().expect("non-empty: matched last") = Stmt::Binding {
                mut_decl: true,
                name: SENTINEL.to_string(),
                name_span: *span,
                ty: None,
                value: expr.clone(),
                span: *span,
            };
            (
                Program {
                    stmts,
                    span: program.span,
                },
                true,
            )
        }
        _ => (program.clone(), false),
    }
}

/// An empty program — used to snapshot a module carrying the session's accumulated types (for `:drop`
/// / teardown) without running any new top-level code.
fn empty_program() -> Program {
    Program {
        stmts: Vec::new(),
        span: Span::empty_at_in(SourceId::FIRST, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::Source;

    fn sandbox_session() -> VmSession {
        VmSession::new(Box::new(|| {
            (
                Box::new(noeta_stdlib::SandboxHost::new()),
                Box::new(noeta_stdlib::SandboxExecutor::new()),
            )
        }))
    }

    fn program(src: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "<repl>", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "test source should parse cleanly: {src:?}"
        );
        parsed.program
    }

    /// Evaluate `src` as one entry and return its stdout.
    fn eval(session: &mut VmSession, src: &str) -> String {
        session.eval(&program(src)).stdout
    }

    /// Compile `src` as a **checked** program keeping the compiler alive — the dance the debug
    /// adapter's launch path runs (T3). Panics on any parse/check/compile failure.
    fn checked_session(src: &str) -> (noeta_bytecode::Module, noeta_compiler::SessionCompiler) {
        let source = Source::new(SourceId::FIRST, "<file>", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "test source should parse cleanly: {src:?}"
        );
        let checked = noeta_check::check_all(&parsed.program);
        assert!(
            checked.diagnostics.is_empty(),
            "test source should check cleanly: {:?}",
            checked.diagnostics
        );
        noeta_compiler::compile_with_sites_session(&parsed.program, checked.sites, false, true)
            .expect("a checked program compiles")
    }

    #[test]
    fn an_adopted_session_extends_a_checked_program_with_stable_ids() {
        let before = noeta_value::live_count();
        let (module, compiler) = checked_session(
            "struct P { x: int }\n\
             fn twice(n: int): int { return n * 2 }\n\
             mut base = 10\n\
             mut p0 = P { x: 3 }\n\
             echo twice(base)\n",
        );
        // Entry 0: the checked program runs to completion under the session.
        let (mut session, out0) = VmSession::adopted(
            &module,
            compiler,
            Box::new(|| {
                (
                    Box::new(noeta_stdlib::SandboxHost::new()),
                    Box::new(noeta_stdlib::SandboxExecutor::new()),
                )
            }),
        );
        assert_eq!(out0.stdout, "20\n");
        assert!(out0.diagnostics.is_empty(), "{:?}", out0.diagnostics);

        // A fragment calls the checked program's function and reads its global — resolved by the
        // ORIGINAL proto index / global slot (stable-prefix accumulation).
        assert_eq!(eval(&mut session, "echo twice(base + 1);"), "22\n");
        // A fragment constructs the checked program's type; structural equality against a value
        // entry 0 built proves the `Rc<Shape>` is the SAME shape (pointer identity), not a re-wrap.
        assert_eq!(eval(&mut session, "echo p0 == P { x: 3 };"), "true\n");
        // A fragment-defined closure captures a checked-program global and calls a checked-program
        // function — new code (a new proto) composed with original ids.
        assert_eq!(
            eval(&mut session, "mut f = fn(n: int) => twice(n) + base;"),
            ""
        );
        assert_eq!(eval(&mut session, "echo f(5);"), "20\n");
        // Rebinding the checked program's global reuses its slot.
        assert_eq!(eval(&mut session, "base = 1; echo twice(base);"), "2\n");

        session.teardown();
        assert_eq!(
            noeta_value::live_count(),
            before,
            "teardown returns residency to the pre-session baseline"
        );
    }

    #[test]
    fn an_adopted_sessions_module_snapshots_keep_a_stable_prefix() {
        let (module, mut compiler) = checked_session(
            "fn twice(n: int): int { return n * 2 }\n\
             mut base = 10\n\
             echo twice(base)\n",
        );
        let entry = {
            let source = Source::new(SourceId::FIRST, "<fragment>", "echo twice(base);");
            let lexed = lex(&source);
            parse(&source, &lexed.tokens).program
        };
        let extended = compiler.extend(&entry).expect("fragment compiles");
        // Ids are append-only: everything the checked module assigned keeps its index.
        assert!(extended.protos.len() >= module.protos.len());
        assert_eq!(
            &extended.global_names[..module.global_names.len()],
            &module.global_names[..],
            "global slots are a stable prefix"
        );
        assert_eq!(
            &extended.names[..module.names.len()],
            &module.names[..],
            "interned names are a stable prefix"
        );
        assert!(
            extended.shapes.len() >= module.shapes.len(),
            "shapes only append"
        );
    }

    #[test]
    fn bindings_and_functions_persist_across_entries() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        assert_eq!(eval(&mut session, "mut x = 10;"), "");
        assert_eq!(
            eval(&mut session, "fn double(n: int): int { return n * 2; }"),
            ""
        );
        // A later entry sees the earlier binding and the earlier function.
        assert_eq!(eval(&mut session, "echo double(x);"), "20\n");
        // Rebinding a name in a later entry updates it (same slot).
        assert_eq!(eval(&mut session, "x = 5;"), "");
        assert_eq!(eval(&mut session, "echo double(x);"), "10\n");
        session.teardown();
        assert_eq!(
            noeta_value::live_count(),
            before,
            "teardown returns residency to the pre-session baseline"
        );
    }

    #[test]
    fn a_trailing_bare_expression_echoes_its_value() {
        let mut session = sandbox_session();
        let out = session.eval(&program("1 + 2"));
        assert_eq!(out.value.as_deref(), Some("3"));
        assert_eq!(out.stdout, "");
        // A statement (not a bare trailing expression) echoes nothing.
        let out = session.eval(&program("mut y = 7;"));
        assert_eq!(out.value, None);
        // The sentinel does not leak into the user's bindings.
        assert_eq!(session.binding_names(), vec!["y".to_string()]);
        session.teardown();
    }

    #[test]
    fn the_id_counter_is_continuous_across_entries() {
        let mut session = sandbox_session();
        // The `use` import in the first entry binds `next_id` as a global that persists into later
        // entries, and the deterministic counter carries across them (it rides on the session state).
        assert_eq!(
            eval(&mut session, "use std.id.{next_id}\necho next_id();"),
            "1\n"
        );
        assert_eq!(eval(&mut session, "echo next_id();"), "2\n");
        assert_eq!(eval(&mut session, "echo next_id();"), "3\n");
        session.teardown();
    }

    #[test]
    fn a_type_and_its_methods_defined_in_one_entry_work_in_the_next() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(
            &mut session,
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}",
        );
        // Construct in one entry, store in a global...
        assert_eq!(eval(&mut session, "mut b = Box.new(21);"), "");
        // ...and call a method on it in a later entry (cross-entry object identity + dispatch).
        assert_eq!(eval(&mut session, "echo b.doubled();"), "42\n");
        session.teardown();
        assert_eq!(noeta_value::live_count(), before);
    }

    #[test]
    fn drop_runs_a_destructor_and_unbinds() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(
            &mut session,
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}",
        );
        assert_eq!(eval(&mut session, "mut r = Res.new(7);"), "");
        assert_eq!(session.binding_names(), vec!["r".to_string()]);
        // Dropping the binding runs its destructor now and unbinds it.
        let (found, out) = session.drop_binding("r");
        assert!(found);
        assert_eq!(out.stdout, "drop 7\n");
        assert!(session.binding_names().is_empty());
        // Dropping a name that is not bound reports absence.
        let (found, _) = session.drop_binding("nope");
        assert!(!found);
        session.teardown();
        assert_eq!(noeta_value::live_count(), before);
    }

    #[test]
    fn reset_clears_bindings_and_zeroes_residency() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(&mut session, "mut xs = [1, 2, 3];");
        assert_eq!(session.binding_names(), vec!["xs".to_string()]);
        session.reset();
        assert!(session.binding_names().is_empty());
        assert_eq!(
            noeta_value::live_count(),
            before,
            "reset tears the old state down to zero residency"
        );
        // The session is usable again after reset.
        assert_eq!(eval(&mut session, "echo 1 + 1;"), "2\n");
        session.teardown();
    }
}
