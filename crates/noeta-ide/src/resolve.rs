//! Definition resolution for go-to-definition, in three layers:
//!
//! - [`DefUse`] — a **scope-aware value index** (L3.2): a use of a local, parameter, `for`/`match`/
//!   closure binding, or top-level function resolves to the precise binding in scope at the cursor,
//!   respecting shadowing. Built by one AST walk that mirrors the language's scoping.
//! - [`MemberTable`] — a **per-type member table** (L3.3): a member access `x.foo` resolves to the
//!   field, enum variant, or method `foo` on `x`'s type. The [`DefUse`] walk records each
//!   `receiver.member` access; the caller supplies the receiver's type (from the checker) and looks
//!   the member up here.
//! - [`Definitions`] — a **top-level name table** (L3): the fallback for what the value index does
//!   not cover — type references and constructors — resolved by the identifier text under the cursor
//!   against the top-level `fn` / `struct` / `class` / `enum` declarations.
//!
//! Go-to-definition tries the value index first, then the member table, then the name table (see
//! [`crate`]).
//!
//! Deliberately *not* here yet (a documented follow-on): cross-module definitions (need the linked
//! workspace). A reference none of this covers simply yields no jump — never a wrong one.

use std::collections::HashMap;

use noeta_ast::{ClosureBody, Expr, FnDecl, ForPattern, Param, Pattern, Program, Stmt, StrPart};
use noeta_span::{SourceId, Span};

/// The top-level definitions a document offers for go-to-definition, keyed by name → the span of the
/// **declared name** (what the editor jumps to). Two namespaces because a value reference (a call)
/// and a type reference resolve independently; the same spelling could name both.
#[derive(Debug, Default)]
pub struct Definitions {
    /// Top-level function names.
    values: HashMap<String, Span>,
    /// Top-level `struct` / `class` / `enum` names.
    types: HashMap<String, Span>,
}

impl Definitions {
    /// Collect the top-level definitions of `program`. The first declaration of a name wins (a
    /// redeclaration is a checker error surfaced separately; here we just keep resolution stable).
    pub fn collect(program: &Program) -> Definitions {
        let mut defs = Definitions::default();
        for stmt in &program.stmts {
            match stmt {
                Stmt::Fn(decl) => {
                    defs.values
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Struct(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Class(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                Stmt::Enum(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                // A user trait's name resolves like a type name (L1) — so goto-def / hover on a
                // `dyn Trait` annotation or a `<T: Trait>` bound lands on the declaration, and it is
                // emitted as a `Type` semantic token.
                Stmt::Trait(decl) => {
                    defs.types
                        .entry(decl.name.clone())
                        .or_insert(decl.name_span);
                }
                _ => {}
            }
        }
        defs
    }

    /// The declaration span for `name`, or `None` if it names no top-level definition. Types are
    /// checked before values: the two namespaces rarely collide, and a PascalCase type reference is
    /// the more likely intent when they do.
    ///
    /// `name` may be a fully-qualified identity (`App.Models.User`) or a bare source name (`User`).
    /// A qualified `name` — the linker rewrites declarations to these (arc Phase B) — matches exactly;
    /// a bare source token that no declaration carries verbatim falls back to matching the **leaf** of
    /// a qualified declaration (`User` → `App.Models.User`), so cross-module go-to-definition still
    /// lands on the merged, now-qualified declaration.
    pub fn resolve(&self, name: &str) -> Option<Span> {
        self.types
            .get(name)
            .or_else(|| self.values.get(name))
            .copied()
            .or_else(|| self.resolve_by_leaf(name))
    }

    /// Fallback for a bare source name against qualified declarations: match a declaration whose leaf
    /// (post-final-`.`) segment equals `name`. Types before values, as in [`resolve`].
    fn resolve_by_leaf(&self, name: &str) -> Option<Span> {
        let leaf = |k: &str| noeta_ast::short_type_name(k) == name;
        self.types
            .iter()
            .find(|(k, _)| leaf(k))
            .or_else(|| self.values.iter().find(|(k, _)| leaf(k)))
            .map(|(_, span)| *span)
    }

    /// The declared-name spans of the top-level functions — for classifying them in semantic tokens.
    pub fn value_spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.values.values().copied()
    }

    /// The declared-name spans of the top-level types (`struct`/`class`/`enum`).
    pub fn type_spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.types.values().copied()
    }
}

/// Map each of `program`'s imports from its **local** binding name (the alias when present, else the
/// imported leaf) to the **qualified identity** it names (`use App.A.User as AUser` → `AUser` →
/// `App.A.User`). Lets go-to-definition on an aliased reference resolve to the qualified declaration
/// the linker merged in (arc Phase B) — a leaf match alone can't, since the alias shares no segment
/// with the target. Purely syntactic (no module pool); an entry that resolves to no such declaration
/// simply misses and the caller falls back to leaf matching.
pub fn import_targets(program: &Program) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for stmt in &program.stmts {
        if let Stmt::Use { path, names, .. } = stmt {
            let prefix = path.join(".");
            for n in names {
                map.insert(n.local().to_string(), format!("{prefix}.{}", n.name));
            }
        }
    }
    map
}

/// The members (fields, enum variants, and methods) each top-level type declares, keyed by
/// `(type name, member name)` → the span of the member's declared name. Powers go-to-definition on a
/// member access `x.foo` once the receiver `x`'s type is known (from the checker's `expr_types`).
/// Methods flattened out of in-body `impl` blocks are already present in each decl's `methods`, so
/// no separate `impl` walk is needed.
#[derive(Debug, Default)]
pub struct MemberTable {
    by_type_member: HashMap<(String, String), Span>,
}

impl MemberTable {
    pub fn collect(program: &Program) -> MemberTable {
        let mut table = MemberTable::default();
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(decl) => {
                    table.add_fields(
                        &decl.name,
                        decl.fields.iter().map(|f| (&f.name, f.name_span)),
                    );
                    table.add_methods(&decl.name, &decl.methods);
                }
                Stmt::Class(decl) => {
                    table.add_fields(
                        &decl.name,
                        decl.fields.iter().map(|f| (&f.name, f.name_span)),
                    );
                    table.add_methods(&decl.name, &decl.methods);
                }
                Stmt::Enum(decl) => {
                    table.add_fields(
                        &decl.name,
                        decl.variants.iter().map(|v| (&v.name, v.name_span)),
                    );
                    table.add_methods(&decl.name, &decl.methods);
                }
                // A trait's method signatures register under the trait name (L1) — so member
                // resolution on a `dyn Trait` receiver's `.method` resolves to the contract.
                Stmt::Trait(decl) => {
                    let sigs: Vec<FnDecl> = decl.methods.iter().map(|m| m.sig.clone()).collect();
                    table.add_methods(&decl.name, &sigs);
                }
                _ => {}
            }
        }
        table
    }

    fn add_fields<'a>(&mut self, ty: &str, members: impl Iterator<Item = (&'a String, Span)>) {
        for (name, span) in members {
            self.by_type_member
                .entry((ty.to_string(), name.clone()))
                .or_insert(span);
        }
    }

    fn add_methods(&mut self, ty: &str, methods: &[FnDecl]) {
        for method in methods {
            self.by_type_member
                .entry((ty.to_string(), method.name.clone()))
                .or_insert(method.name_span);
        }
    }

    /// The declaration span of `member` on type `ty`, if any.
    pub fn lookup(&self, ty: &str, member: &str) -> Option<Span> {
        self.by_type_member
            .get(&(ty.to_string(), member.to_string()))
            .copied()
    }

    /// The `(type, member)` whose declared-name span in file `source` contains `offset` — i.e. the
    /// member declaration the cursor is on (a field, variant, or method name in a type body). For
    /// find-references / rename started from the declaration itself.
    pub fn declaration_at(&self, offset: u32, source: SourceId) -> Option<(&str, &str)> {
        self.by_type_member
            .iter()
            .find(|(_, span)| span.source == source && span.start <= offset && offset <= span.end)
            .map(|((ty, member), _)| (ty.as_str(), member.as_str()))
    }
}

/// A member access `receiver.name` recorded during the def/use walk: the span of the member *name*
/// (the go-to-definition target when the cursor is on it) and the span of the *receiver* expression
/// (whose type resolves which declaration the member belongs to).
#[derive(Debug)]
struct MemberRef {
    name: String,
    name_span: Span,
    receiver_span: Span,
}

/// A scope-aware **value** def/use index: every identifier *use* (a variable, parameter, or
/// function reference) mapped to the span of the *definition* it resolves to. Built by one AST walk
/// that mirrors the language's scoping — parameters, block-local bindings, `for`/`match`/closure
/// bindings, and the bare-assignment locality rule (`x = v` reassigns an enclosing binding if one
/// exists, else declares a fresh local). It also records every `receiver.member` access for the
/// [`MemberTable`] step. Method bodies inside `struct`/`class`/`enum` declarations are walked (so
/// their parameters and locals resolve); `@tier` blocks are not.
#[derive(Debug, Default)]
pub struct DefUse {
    /// `(use span, definition span)` for each value identifier that resolves to a binding.
    refs: Vec<(Span, Span)>,
    /// Every `receiver.member` access, for member go-to-definition.
    member_refs: Vec<MemberRef>,
    /// The declared-name span of every binding introduced (functions, parameters, locals, loop/match/
    /// closure variables) — including ones never referenced. For semantic-token classification of
    /// declarations, which the use→def `refs` alone would miss.
    bindings: Vec<Span>,
}

impl DefUse {
    /// Build the value def/use index for `program`.
    pub fn build(program: &Program) -> DefUse {
        let mut resolver = Resolver::default();
        // Top-level functions resolve regardless of textual order (mutual recursion), so seed them
        // before walking any body.
        for stmt in &program.stmts {
            if let Stmt::Fn(decl) = stmt {
                resolver
                    .functions
                    .entry(decl.name.clone())
                    .or_insert(decl.name_span);
            }
        }
        resolver.scopes.push(HashMap::new()); // the module scope, for top-level bindings
        for stmt in &program.stmts {
            resolver.walk_stmt(stmt);
        }
        DefUse {
            refs: resolver.refs,
            member_refs: resolver.member_refs,
            bindings: resolver.bindings,
        }
    }

    /// The definition span for the value reference in file `source` whose use-span contains
    /// `offset`, if any. The `source` filter matters over a merged multi-file program, where the
    /// same byte offset exists in every file; the cursor is in one specific file. The tightest
    /// containing use wins (value-ident uses do not nest, but the guard is cheap). The returned
    /// definition span may belong to a *different* file (a cross-module reference).
    pub fn definition_at(&self, offset: u32, source: SourceId) -> Option<Span> {
        self.refs
            .iter()
            .filter(|(use_span, _)| {
                use_span.source == source
                    && use_span.end > use_span.start
                    && use_span.start <= offset
                    && offset <= use_span.end
            })
            .min_by_key(|(use_span, _)| use_span.end - use_span.start)
            .map(|(_, def)| *def)
    }

    /// The definition span the cursor identifies for find-references — whether it sits on a *use*
    /// (its resolved definition) or on the *definition* name itself (that binding). `None` if the
    /// cursor is on neither. The returned span uniquely keys the symbol; [`references_to`](Self::
    /// references_to) then collects every use of it.
    pub fn symbol_at(&self, offset: u32, source: SourceId) -> Option<Span> {
        // On a use → its definition.
        if let Some(def) = self.definition_at(offset, source) {
            return Some(def);
        }
        // On the declaration name itself → that definition (it is the `def` of its own uses).
        self.refs
            .iter()
            .map(|(_, def)| *def)
            .find(|def| def.source == source && def.start <= offset && offset <= def.end)
    }

    /// Every `(use span, definition span)` pair in the index — the raw material a caller-side
    /// join reads (the call graph resolves function-name uses through it).
    pub fn refs(&self) -> impl Iterator<Item = (Span, Span)> + '_ {
        self.refs.iter().copied()
    }

    /// Every *use* span that resolves to definition `def` — the references to the symbol. The
    /// declaration itself is not included (the caller adds it when `includeDeclaration` is set).
    /// Spans may span multiple files (a cross-module symbol).
    pub fn references_to(&self, def: Span) -> Vec<Span> {
        self.refs
            .iter()
            .filter(|(_, d)| *d == def)
            .map(|(use_span, _)| *use_span)
            .collect()
    }

    /// Every recorded `(use span, definition span)` — for semantic-token classification: a use whose
    /// definition is a top-level function is a function reference, otherwise a variable/parameter.
    pub fn all_refs(&self) -> impl Iterator<Item = (Span, Span)> + '_ {
        self.refs.iter().copied()
    }

    /// The declared-name span of every binding (used or not) — for classifying declarations in
    /// semantic tokens; the caller decides function vs variable by whether the span is a top-level
    /// function.
    pub fn binding_spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.bindings.iter().copied()
    }

    /// Every recorded member access as `(member name, member-name span, receiver span)` — for member
    /// find-references / rename: the caller keeps those whose `receiver` has the target type and whose
    /// name matches, and renames each name span.
    pub fn member_occurrences(&self) -> impl Iterator<Item = (&str, Span, Span)> {
        self.member_refs
            .iter()
            .map(|m| (m.name.as_str(), m.name_span, m.receiver_span))
    }

    /// The `(receiver span, member name)` of the member access in file `source` whose member-name
    /// span contains `offset`, if the cursor is on a `.member`. The caller resolves the receiver's
    /// type (via the checker's `expr_types`) and looks the member up in a [`MemberTable`].
    pub fn member_at(&self, offset: u32, source: SourceId) -> Option<(Span, &str)> {
        self.member_refs
            .iter()
            .find(|m| {
                m.name_span.source == source
                    && m.name_span.start <= offset
                    && offset <= m.name_span.end
            })
            .map(|m| (m.receiver_span, m.name.as_str()))
    }
}

/// The value **bindings visible at `offset`** in file `source` — the locals, parameters, and
/// `for`/`match`/closure bindings whose lexical scope encloses the cursor, plus the module-level
/// bindings declared before it — each as `(name, definition span)`. Powers in-scope identifier
/// completion. Runs the same scoping walk as [`DefUse::build`]: it snapshots the scope stack at the
/// deepest AST node that contains the cursor, so a name is offered exactly where it is in scope
/// (shadowing included). Top-level functions and type declarations are *not* here — the caller lists
/// those directly, with their precise kinds. Empty when the cursor is not inside any statement.
pub fn visible_at(program: &Program, offset: u32, source: SourceId) -> Vec<(String, Span)> {
    let mut resolver = Resolver {
        cursor: Some((offset, source)),
        ..Resolver::default()
    };
    // Seed top-level functions (mutual recursion) exactly as `DefUse::build` does, so a snapshot
    // taken inside a function body sees the same scope shape; the functions themselves live outside
    // the scope stack and so are excluded from the snapshot.
    for stmt in &program.stmts {
        if let Stmt::Fn(decl) = stmt {
            resolver
                .functions
                .entry(decl.name.clone())
                .or_insert(decl.name_span);
        }
    }
    resolver.scopes.push(HashMap::new()); // the module scope
    // The module region spans the whole file (offsets 0..∞ in the cursor's source), so a cursor on a
    // blank top-level line still captures the module-level bindings declared before it.
    let module_region = Span::new_in(source, 0, u32::MAX);
    resolver.walk_seq(module_region, &program.stmts, None);
    resolver
        .snapshot
        .map(|(bindings, _)| bindings)
        .unwrap_or_default()
}

/// The mutable state of one [`DefUse::build`] walk: the top-level function table, the lexical scope
/// stack of value bindings (innermost last), and the accumulating use→def references.
#[derive(Default)]
struct Resolver {
    functions: HashMap<String, Span>,
    scopes: Vec<HashMap<String, Span>>,
    refs: Vec<(Span, Span)>,
    member_refs: Vec<MemberRef>,
    /// Every declared-name span passed to [`bind`](Self::bind), for [`DefUse::binding_spans`].
    bindings: Vec<Span>,
    /// When set (completion), the `(offset, source)` whose in-scope bindings to capture. `None` for
    /// the def/use walk, which pays no snapshot cost.
    cursor: Option<(u32, SourceId)>,
    /// The visible bindings captured for `cursor`, paired with the *width* of the span they were
    /// captured at. The tightest (smallest-width) containing span wins: a node the cursor sits on
    /// beats the enclosing scope's region, which is only used when the cursor is in whitespace with
    /// no node under it. See [`visible_at`].
    snapshot: Option<(Vec<(String, Span)>, u32)>,
}

impl Resolver {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Whether the cursor is in `region`'s file and within it.
    fn cursor_within(&self, region: Span) -> bool {
        matches!(self.cursor, Some((offset, source)) if source == region.source && region.start <= offset && offset <= region.end)
    }

    /// Snapshot the bindings currently in scope, tagged with `width`. Keeps the capture from the
    /// *tightest* span seen so far (`width <= previous`): a node the cursor sits on (small width)
    /// overrides an enclosing scope-region capture (large width). The scope stack at the call site
    /// holds exactly the bindings visible there — enclosing scopes plus earlier siblings, not any
    /// not-yet-processed binding.
    fn capture(&mut self, width: u32) {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|(_, prev)| width <= *prev)
        {
            let bindings = self
                .scopes
                .iter()
                .flat_map(|scope| scope.iter().map(|(name, def)| (name.clone(), *def)))
                .collect();
            self.snapshot = Some((bindings, width));
        }
    }

    /// If the cursor sits on `span` (a node), capture the bindings visible there. Called at the entry
    /// of every statement and expression, so a cursor on any token gets the precise in-scope set.
    fn maybe_snapshot(&mut self, span: Span) {
        if self.cursor_within(span) {
            self.capture(span.len());
        }
    }

    /// Walk a scope's body (statements plus an optional tail expression), also capturing bindings when
    /// the cursor falls in *whitespace* with no node under it: at the first item starting after the
    /// cursor (a gap between statements) or, failing that, past the last item but still inside
    /// `region` (trailing whitespace). These captures use `region`'s width, so the on-node captures
    /// inside `walk_stmt`/`walk_expr` still win wherever the cursor is actually on a node.
    fn walk_seq(&mut self, region: Span, stmts: &[Stmt], tail: Option<&Expr>) {
        let mut captured = false;
        let mut last_end = region.start;
        for stmt in stmts {
            self.gap_capture(region, stmt.span().start, &mut captured);
            self.walk_stmt(stmt);
            last_end = last_end.max(stmt.span().end);
        }
        if let Some(expr) = tail {
            self.gap_capture(region, expr.span().start, &mut captured);
            self.walk_expr(expr);
            last_end = last_end.max(expr.span().end);
        }
        // Cursor past every item but still inside the region → all of the body's bindings are visible.
        if !captured
            && self.cursor_within(region)
            && matches!(self.cursor, Some((offset, _)) if offset >= last_end)
        {
            self.capture(region.len());
        }
    }

    /// Capture (once) if the cursor sits in the gap before an item starting at `next_start`.
    fn gap_capture(&mut self, region: Span, next_start: u32, captured: &mut bool) {
        if !*captured
            && self.cursor_within(region)
            && matches!(self.cursor, Some((offset, _)) if offset < next_start)
        {
            self.capture(region.len());
            *captured = true;
        }
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str, span: Span) {
        self.bindings.push(span);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), span);
        }
    }

    fn in_scope(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains_key(name))
    }

    /// Resolve a value name to its definition span: the nearest enclosing binding, else a top-level
    /// function.
    fn resolve(&self, name: &str) -> Option<Span> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .or_else(|| self.functions.get(name).copied())
    }

    /// Record a use of `name` at `span` if it resolves to a definition.
    fn use_ident(&mut self, name: &str, span: Span) {
        if let Some(def) = self.resolve(name) {
            self.refs.push((span, def));
        }
    }

    /// Introduce a bound name: a fresh local for a `mut` declaration or a name not already in scope;
    /// otherwise a bare reassignment, whose target is a *use* of the existing binding.
    fn declare_or_reassign(&mut self, mut_decl: bool, name: &str, span: Span) {
        if mut_decl || !self.in_scope(name) {
            self.bind(name, span);
        } else {
            self.use_ident(name, span);
        }
    }

    fn walk_block_scoped(&mut self, stmts: &[Stmt]) {
        self.push_scope();
        // The block's region is the span of its statements (used only for whitespace completion); an
        // empty block has nothing to walk or capture.
        if let Some(region) = seq_region(stmts) {
            self.walk_seq(region, stmts, None);
        }
        self.pop_scope();
    }

    /// Walk a function/closure: parameter defaults resolve in the *definition* scope (not against the
    /// parameters), so they are walked before the parameter scope is pushed. `region` is the whole
    /// callable's span — broad enough that completion in an empty or whitespace-only body still offers
    /// the parameters.
    fn walk_callable(
        &mut self,
        region: Span,
        params: &[Param],
        body_stmts: &[Stmt],
        body_expr: Option<&Expr>,
    ) {
        for param in params {
            if let Some(default) = &param.default {
                self.walk_expr(default);
            }
        }
        self.push_scope();
        for param in params {
            self.bind(&param.name, param.name_span);
        }
        self.walk_seq(region, body_stmts, body_expr);
        self.pop_scope();
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        self.maybe_snapshot(stmt.span());
        match stmt {
            Stmt::Echo { value, .. } => self.walk_expr(value),
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => {
                self.walk_expr(value);
                self.declare_or_reassign(*mut_decl, name, *name_span);
            }
            Stmt::Destructure {
                mut_decl,
                targets,
                value,
                ..
            } => {
                self.walk_expr(value);
                for (name, span) in targets {
                    self.declare_or_reassign(*mut_decl, name, *span);
                }
            }
            Stmt::Fn(decl) => {
                // The fn name is visible to siblings and to itself (recursion).
                self.bind(&decl.name, decl.name_span);
                self.walk_callable(decl.span, &decl.params, &decl.body, None);
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.walk_expr(value);
                }
            }
            Stmt::Yield { value, .. } => self.walk_expr(value),
            Stmt::Expr { expr, .. } => self.walk_expr(expr),
            Stmt::Concurrent { body, .. } => self.walk_block_scoped(body),
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.walk_expr(cond);
                self.walk_block_scoped(then_body);
                if let Some(else_body) = else_body {
                    self.walk_block_scoped(else_body);
                }
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                self.walk_expr(iterable);
                self.push_scope();
                match pattern {
                    ForPattern::Single { name, name_span } => self.bind(name, *name_span),
                    ForPattern::Tuple { names, .. } => {
                        for (name, span) in names {
                            self.bind(name, *span);
                        }
                    }
                }
                // The loop's own span is the region, so completion in the body (including whitespace)
                // sees the loop variable alongside the body's locals.
                self.walk_seq(*span, body, None);
                self.pop_scope();
            }
            Stmt::While { cond, body, .. } => {
                self.walk_expr(cond);
                self.walk_block_scoped(body);
            }
            // A type's methods each open their own scope (parameters + body locals); walk them so
            // resolution works inside method bodies. Fields are not bound as bare names — a bare
            // field reference in a method simply falls through rather than mis-resolving.
            Stmt::Struct(decl) => self.walk_methods(&decl.methods),
            Stmt::Class(decl) => self.walk_methods(&decl.methods),
            Stmt::Enum(decl) => self.walk_methods(&decl.methods),
            // A tier block's items (server-hmr W3) resolve like top-level statements: a `@test`
            // fn's body references program declarations, and both the editor (goto/refs inside a
            // test body) and the impact engine (which tests call a changed fn) need the edges. A
            // `@doc` text tier carries no items, so this is naturally a no-op for it.
            Stmt::TierBlock { items, .. } => {
                for item in items {
                    self.walk_stmt(item);
                }
            }
            // A trait's default-method bodies reference declarations — walk them for goto/refs
            // inside a default body (L1). Required (bodiless) sigs have empty bodies, so this is a
            // no-op for them.
            Stmt::Trait(decl) => {
                let sigs: Vec<FnDecl> = decl.methods.iter().map(|m| m.sig.clone()).collect();
                self.walk_methods(&sigs);
            }
            // Control-flow leaves and module statements bind and reference nothing.
            Stmt::Impl(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
        }
    }

    fn walk_methods(&mut self, methods: &[FnDecl]) {
        for method in methods {
            self.walk_callable(method.span, &method.params, &method.body, None);
        }
    }

    fn walk_expr(&mut self, expr: &Expr) {
        self.maybe_snapshot(expr.span());
        match expr {
            Expr::Ident { name, span } => self.use_ident(name, *span),
            Expr::Unary { operand, .. } => self.walk_expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            Expr::Call { callee, args, .. } => {
                self.walk_expr(callee);
                self.walk_exprs(args);
            }
            Expr::Closure {
                params, body, span, ..
            } => match body {
                ClosureBody::Expr(inner) => self.walk_callable(*span, params, &[], Some(inner)),
                ClosureBody::Block(stmts) => self.walk_callable(*span, params, stmts, None),
            },
            Expr::Pipeline { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            Expr::List { items, .. } | Expr::Tuple { items, .. } => self.walk_exprs(items),
            Expr::TupleIndex { receiver, .. } => self.walk_expr(receiver),
            Expr::Range { start, end, .. } => {
                self.walk_expr(start);
                self.walk_expr(end);
            }
            Expr::Map { entries, .. } => {
                for (key, value) in entries {
                    self.walk_expr(key);
                    self.walk_expr(value);
                }
            }
            // `receiver.name` — the member name is a field/method, resolved by the receiver's type
            // (recorded here, resolved by the caller against a `MemberTable`); the receiver is a
            // value expression walked normally.
            Expr::Member {
                receiver,
                name,
                name_span,
                ..
            } => {
                self.member_refs.push(MemberRef {
                    name: name.clone(),
                    name_span: *name_span,
                    receiver_span: receiver.span(),
                });
                self.walk_expr(receiver);
            }
            Expr::Index {
                receiver, index, ..
            } => {
                self.walk_expr(receiver);
                self.walk_expr(index);
            }
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(expr) = part {
                        self.walk_expr(expr);
                    }
                }
            }
            // An expression-tier block's holes are ordinary expressions (its statics are text).
            Expr::TierExpr { holes, .. } => {
                for hole in holes {
                    self.walk_expr(hole);
                }
            }
            // Compiler-synthesized, never in parsed source the IDE walks.
            Expr::NativeFnRef { .. } => {}
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.push_scope();
                    bind_pattern(&arm.pattern, &mut |name, span| self.bind(name, span));
                    self.walk_expr(&arm.body);
                    self.pop_scope();
                }
            }
            Expr::Object(lit) => {
                if let Some(spread) = &lit.spread {
                    self.walk_expr(spread);
                }
                for init in &lit.fields {
                    self.walk_expr(&init.value);
                }
            }
            Expr::Try { expr, .. }
            | Expr::Await { expr, .. }
            | Expr::TypeTest { expr, .. }
            | Expr::As { expr, .. } => self.walk_expr(expr),
            Expr::Spawn { future, .. } => self.walk_expr(future),
            Expr::TypeOf { value, .. } => self.walk_expr(value),
            Expr::ParamsOf { target, .. } => self.walk_expr(target),
            Expr::FromBytes { blob, .. } => self.walk_expr(blob),
            Expr::Channel { capacity, .. } => self.walk_expr(capacity),
            Expr::TypedModuleCall { recv, args, .. } => {
                self.walk_expr(recv);
                self.walk_exprs(args);
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                self.walk_expr(recv);
                self.walk_expr(name);
                self.walk_expr(args);
            }
            Expr::Coalesce {
                value, fallback, ..
            } => {
                self.walk_expr(value);
                self.walk_expr(fallback);
            }
            Expr::FieldSet {
                receiver, value, ..
            } => {
                self.walk_expr(receiver);
                self.walk_expr(value);
            }
            // Literals and operand-free reflection queries reference no value binding.
            Expr::Str { .. }
            | Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::F32 { .. }
            | Expr::F64 { .. }
            | Expr::IntN { .. }
            | Expr::Bool { .. }
            | Expr::AttributesOf { .. }
            | Expr::RolesOf { .. } => {}
        }
    }

    fn walk_exprs(&mut self, exprs: &[Expr]) {
        for expr in exprs {
            self.walk_expr(expr);
        }
    }
}

/// The span covering a statement sequence (its first statement's start to its last's end), or `None`
/// for an empty sequence. Used as the completion region of a block body.
fn seq_region(stmts: &[Stmt]) -> Option<Span> {
    Some(stmts.first()?.span().merge(stmts.last()?.span()))
}

/// Invoke `bind` for each name a match pattern introduces (recursing into variant and tuple
/// sub-patterns). Literal, wildcard, and `is T` patterns bind nothing.
fn bind_pattern(pattern: &Pattern, bind: &mut impl FnMut(&str, Span)) {
    match pattern {
        Pattern::Binding { name, span } => bind(name, *span),
        Pattern::Variant { bindings, .. } => {
            for sub in bindings {
                bind_pattern(sub, bind);
            }
        }
        Pattern::Tuple { elements, .. } => {
            for sub in elements {
                bind_pattern(sub, bind);
            }
        }
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. }
        | Pattern::IsType { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn program_of(src: &str) -> noeta_ast::Program {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        parse(&source, &lexed.tokens).program
    }

    fn defs_of(src: &str) -> Definitions {
        Definitions::collect(&program_of(src))
    }

    /// The definition byte-offset the value index resolves for the cursor at `use_offset`.
    fn def_start_at(src: &str, use_offset: u32) -> Option<u32> {
        DefUse::build(&program_of(src))
            .definition_at(use_offset, SourceId::FIRST)
            .map(|span| span.start)
    }

    #[test]
    fn collects_functions_and_types() {
        let defs =
            defs_of("fn greet(): int { return 1 }\nstruct Point { x: int }\nenum Color { Red }");
        assert!(defs.resolve("greet").is_some());
        assert!(defs.resolve("Point").is_some());
        assert!(defs.resolve("Color").is_some());
        assert!(defs.resolve("missing").is_none());
    }

    #[test]
    fn resolves_to_the_name_span_not_the_whole_decl() {
        // `fn greet` — the name starts at byte 3.
        let defs = defs_of("fn greet(): int { return 1 }");
        let span = defs.resolve("greet").unwrap();
        assert_eq!(span.start, 3);
        assert_eq!(span.end, 8); // "greet"
    }

    #[test]
    fn value_index_resolves_a_local_binding() {
        let src = "total = 1 + 2\necho total";
        let use_at = src.rfind("total").unwrap() as u32; // the `total` in `echo total`
        let def_at = src.find("total").unwrap() as u32; // the binding
        assert_eq!(def_start_at(src, use_at + 2), Some(def_at));
    }

    #[test]
    fn value_index_resolves_a_parameter() {
        let src = "fn f(count: int): int { return count }";
        let use_at = src.rfind("count").unwrap() as u32; // `return count`
        let def_at = src.find("count").unwrap() as u32; // the parameter
        assert_eq!(def_start_at(src, use_at + 2), Some(def_at));
    }

    #[test]
    fn value_index_resolves_a_for_loop_variable() {
        let src = "for item in [1, 2] {\n  echo item\n}";
        let use_at = src.rfind("item").unwrap() as u32;
        let def_at = src.find("item").unwrap() as u32;
        assert_eq!(def_start_at(src, use_at + 1), Some(def_at));
    }

    #[test]
    fn value_index_respects_shadowing() {
        // The inner closure parameter `x` shadows the outer binding `x`; a use in the body resolves
        // to the parameter, not the outer binding.
        let src = "x = 1\nf = fn(x: int) => x + 1";
        let param_at = src.find("fn(x").unwrap() as u32 + 3; // the parameter `x`
        let use_at = src.rfind('x').unwrap() as u32; // `x + 1`
        assert_eq!(def_start_at(src, use_at), Some(param_at));
    }

    #[test]
    fn value_index_ignores_an_unbound_name() {
        // `greet` is a top-level fn (handled by the name-table fallback, not the value index).
        let src = "fn greet(): int { return 1 }\nx = 1";
        // A cursor on a nonexistent name resolves to nothing in the value index.
        assert_eq!(def_start_at(src, 0), None);
    }
}
