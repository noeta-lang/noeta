//! A stable textual dump of the Core IR, for golden tests and human review.
//!
//! The format is deliberately compact and deterministic — temporaries print as `%n`, source
//! variables by name, and every `let`-sequenced statement on its own line — so a lowering
//! change shows up as a reviewable diff in the snapshots.

use std::fmt::Write as _;

use crate::{
    Atom, Block, ClassDef, Const, Decl, Func, InterpPart, Program, ReflectArgs, Rvalue, Stmt,
    Thunk, TypeRef,
};

/// Render a lowered [`Program`] to a stable string.
pub fn dump(program: &Program) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "program (temps: {})", program.temp_count);
    let mut p = Printer { out: &mut out };
    p.block_body(&program.top, 1);
    out
}

struct Printer<'a> {
    out: &'a mut String,
}

impl Printer<'_> {
    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// Print a block's statements (no surrounding braces) at `indent`.
    fn block_body(&mut self, block: &Block, indent: usize) {
        for stmt in &block.stmts {
            self.stmt(stmt, indent);
        }
        if let Some(tail) = &block.tail {
            self.line(indent, &format!("tail {}", atom(tail)));
        }
    }

    fn stmt(&mut self, stmt: &Stmt, indent: usize) {
        match stmt {
            Stmt::Let { dst, rvalue, .. } => {
                self.line(indent, &format!("let %{} = {}", dst.0, self.rvalue(rvalue)))
            }
            Stmt::Eval { rvalue, .. } => {
                self.line(indent, &format!("eval {}", self.rvalue(rvalue)))
            }
            Stmt::Bind {
                mut_decl,
                name,
                value,
                ..
            } => {
                let kw = if *mut_decl { "mut " } else { "" };
                self.line(indent, &format!("{kw}{name} = {}", atom(value)));
            }
            Stmt::Echo { value, .. } => self.line(indent, &format!("echo {}", atom(value))),
            Stmt::Return { value, .. } => match value {
                Some(a) => self.line(indent, &format!("return {}", atom(a))),
                None => self.line(indent, "return"),
            },
            Stmt::Break { .. } => self.line(indent, "break"),
            Stmt::Continue { .. } => self.line(indent, "continue"),
            Stmt::ScopeBegin { .. } => self.line(indent, "scope_begin"),
            Stmt::ScopeEnd { .. } => self.line(indent, "scope_end"),
            Stmt::Drop(t) => self.line(indent, &format!("drop %{}", t.0)),
            Stmt::DropVar { name, .. } => self.line(indent, &format!("drop {name}")),
            Stmt::If {
                cond,
                then_block,
                else_block,
                ..
            } => {
                self.line(indent, &format!("if {} {{", atom(cond)));
                self.block_body(then_block, indent + 1);
                if let Some(else_block) = else_block {
                    self.line(indent, "} else {");
                    self.block_body(else_block, indent + 1);
                }
                self.line(indent, "}");
            }
            Stmt::While { cond, body, .. } => {
                self.line(indent, "while {");
                self.block_body(cond, indent + 1);
                self.line(indent, "} body {");
                self.block_body(body, indent + 1);
                self.line(indent, "}");
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.line(
                    indent,
                    &format!("for {} in {} {{", for_pattern(pattern), atom(iterable)),
                );
                self.block_body(body, indent + 1);
                self.line(indent, "}");
            }
            Stmt::Match {
                scrutinee,
                arms,
                dst,
                ..
            } => {
                let head = match dst {
                    Some(t) => format!("%{} = match {} {{", t.0, atom(scrutinee)),
                    None => format!("match {} {{", atom(scrutinee)),
                };
                self.line(indent, &head);
                for arm in arms {
                    match &arm.guard {
                        Some(guard) => {
                            self.line(indent + 1, &format!("{} if {{", pattern_str(&arm.pattern)));
                            self.block_body(&guard.block, indent + 2);
                            self.line(indent + 1, "} =>");
                        }
                        None => {
                            self.line(indent + 1, &format!("{} =>", pattern_str(&arm.pattern)));
                        }
                    }
                    self.block_body(&arm.body, indent + 2);
                }
                self.line(indent, "}");
            }
            Stmt::Logical {
                dst,
                op,
                left,
                right,
                ..
            } => {
                let head = dst_prefix(dst);
                self.line(indent, &format!("{head}{} {} {{", atom(left), op.symbol()));
                self.block_body(right, indent + 1);
                self.line(indent, "}");
            }
            Stmt::Coalesce {
                dst,
                value,
                fallback,
                ..
            } => {
                let head = dst_prefix(dst);
                self.line(indent, &format!("{head}{} ?? {{", atom(value)));
                self.block_body(fallback, indent + 1);
                self.line(indent, "}");
            }
            Stmt::Decl(decl) => self.decl(decl, indent),
        }
    }

    fn decl(&mut self, decl: &Decl, indent: usize) {
        match decl {
            Decl::Fn { name, func, .. } => self.func(&format!("fn {name}"), func, indent),
            Decl::Class(class) => self.class(class, indent),
            Decl::Enum(en) => {
                if en.methods.is_empty() {
                    self.line(indent, &format!("enum {}", en.decl.name));
                } else {
                    self.line(indent, &format!("enum {} {{", en.decl.name));
                    for (name, func) in &en.methods {
                        self.func(&format!("method {name}"), func, indent + 1);
                    }
                    self.line(indent, "}");
                }
            }
            Decl::Struct(d) => self.line(indent, &format!("struct {}", d.decl.name)),
            Decl::Use { path, names, .. } => {
                let names: Vec<&str> = names.iter().map(|n| n.name.as_str()).collect();
                self.line(
                    indent,
                    &format!("use {}.{{{}}}", path.join("."), names.join(", ")),
                );
            }
        }
    }

    fn class(&mut self, class: &ClassDef, indent: usize) {
        self.line(indent, &format!("class {} {{", class.decl.name));
        for (name, func) in &class.methods {
            self.func(&format!("method {name}"), func, indent + 1);
        }
        if let Some(destructor) = &class.destructor {
            self.func("destruct", destructor, indent + 1);
        }
        self.line(indent, "}");
    }

    fn func(&mut self, head: &str, func: &Func, indent: usize) {
        self.line(
            indent,
            &format!(
                "{head}({}) (temps: {}) {{",
                func.params.join(", "),
                func.temp_count
            ),
        );
        for (i, default) in func.defaults.iter().enumerate() {
            if let Some(thunk) = default {
                self.thunk(&format!("default {}", func.params[i]), thunk, indent + 1);
            }
        }
        self.block_body(&func.body, indent + 1);
        self.line(indent, "}");
    }

    fn thunk(&mut self, head: &str, thunk: &Thunk, indent: usize) {
        self.line(indent, &format!("{head} (temps: {}) {{", thunk.temp_count));
        self.block_body(&thunk.body, indent + 1);
        self.line(indent, "}");
    }

    fn rvalue(&self, rvalue: &Rvalue) -> String {
        match rvalue {
            Rvalue::Use(a) => atom(a),
            Rvalue::Unary { op, operand, .. } => format!("{}{}", op.symbol(), atom(operand)),
            Rvalue::MaskWidth {
                operand,
                signed,
                bits,
                ..
            } => format!(
                "mask_{}{bits}({})",
                if *signed { 'i' } else { 'u' },
                atom(operand)
            ),
            Rvalue::Render { operand, .. } => format!("render({})", atom(operand)),
            Rvalue::JsonRender { operand, .. } => format!("json_render({})", atom(operand)),
            Rvalue::Binary {
                op,
                lhs,
                rhs,
                reuse,
                ..
            } => {
                // The reuse token is only rendered when set (the rare list self-append), so an
                // ordinary binary dump — and its golden — carries no marker.
                let marker = if *reuse { " reuse" } else { "" };
                format!("{} {} {}{}", atom(lhs), op.symbol(), atom(rhs), marker)
            }
            Rvalue::WideInt {
                op,
                lhs,
                rhs,
                signed,
                bits,
                ..
            } => format!(
                "{} {} {} {}{bits}",
                atom(lhs),
                op.symbol(),
                atom(rhs),
                if *signed { 'i' } else { 'u' },
            ),
            Rvalue::WidthIntMethod {
                receiver,
                method,
                args,
                bits,
                ..
            } => format!("{}.{method:?}({}) w{bits}", atom(receiver), atoms(args)),
            // A forwarding call's type arguments render as a turbofish, and are absent — so the
            // dump and its golden are unchanged — for every call that forwards nothing.
            Rvalue::Call {
                callee,
                args,
                type_args,
                ..
            } => format!(
                "call {}{}({})",
                atom(callee),
                turbofish(type_args),
                atoms(args)
            ),
            Rvalue::Method {
                receiver,
                name,
                args,
                type_args,
                reuse,
                ..
            } => {
                // The reuse token renders only when set (the rare collection method self-update), so
                // every other method dump — and its golden — is unchanged.
                let marker = if *reuse { " reuse" } else { "" };
                format!(
                    "{}.{}{}({}){}",
                    atom(receiver),
                    name,
                    turbofish(type_args),
                    atoms(args),
                    marker
                )
            }
            Rvalue::TraitMethod {
                receiver,
                trait_name,
                name,
                args,
                ..
            } => format!(
                "{}.{}({}) via trait {}",
                atom(receiver),
                name,
                atoms(args),
                trait_name
            ),
            Rvalue::Field { receiver, name, .. } => format!("{}.{}", atom(receiver), name),
            Rvalue::MethodHandle {
                ty,
                method,
                associated,
                ..
            } => format!(
                "handle {ty}.{method}{}",
                if *associated { " (assoc)" } else { "" }
            ),
            Rvalue::BoundHandle { recv, method, .. } => {
                format!("bind {}.{method}", atom(recv))
            }
            Rvalue::SetField {
                receiver,
                name,
                value,
                reuse,
                ..
            } => {
                let marker = if *reuse { " [reuse]" } else { "" };
                format!("{}.{} = {}{}", atom(receiver), name, atom(value), marker)
            }
            Rvalue::Index {
                receiver, index, ..
            } => format!("{}[{}]", atom(receiver), atom(index)),
            Rvalue::IndexField {
                receiver,
                index,
                field,
                ..
            } => format!("{}[{}].{}", atom(receiver), atom(index), field),
            Rvalue::List { items, .. } => format!("[{}]", atoms(items)),
            Rvalue::PackedListNew { layout, .. } => {
                format!("packed-list-new<{}>", layout.type_name)
            }
            Rvalue::PackedListPush { list, value, .. } => {
                format!("packed-list-push({}, {})", atom(list), atom(value))
            }
            Rvalue::Tuple { items, .. } => format!("({})", atoms(items)),
            Rvalue::TupleIndex {
                receiver, index, ..
            } => format!("{}.{index}", atom(receiver)),
            Rvalue::Map { entries, .. } => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", atom(k), atom(v)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Rvalue::Range {
                start,
                end,
                inclusive,
                ..
            } => format!(
                "{}{}{}",
                atom(start),
                if *inclusive { "..=" } else { ".." },
                atom(end)
            ),
            Rvalue::Object {
                type_name,
                fields,
                spread,
                reuse,
                ..
            } => {
                let mut parts: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, atom(&f.value)))
                    .collect();
                if let Some((a, _)) = spread {
                    parts.push(format!("...{}", atom(a)));
                }
                // The reuse token is only rendered when set, so a non-self-update object dump (and
                // its golden) carries no marker.
                let marker = if *reuse { " reuse" } else { "" };
                format!("{} {{{}}}{}", type_name, parts.join(", "), marker)
            }
            Rvalue::Interp { parts, .. } => {
                let rendered: Vec<String> = parts
                    .iter()
                    .map(|p| match p {
                        InterpPart::Literal(s) => format!("{s:?}"),
                        InterpPart::Hole { atom: a, .. } => format!("${{{}}}", atom(a)),
                    })
                    .collect();
                format!("interp({})", rendered.join(" "))
            }
            Rvalue::Closure { func, .. } => format!("closure fn({})", func.params.join(", ")),
            Rvalue::Try { operand, .. } => format!("{}?", atom(operand)),
            // A dynamic head name is rendered as the atom it arrives in, not as the erased `T` the
            // baked `ty` still spells — a snapshot must show which target the narrow will use.
            Rvalue::As {
                operand,
                ty,
                dynamic,
                ..
            } => match dynamic {
                Some(name) => format!("{}.as<name {}>()", atom(operand), atom(name)),
                None => format!("{}.as<{}>()", atom(operand), type_ref(ty)),
            },
            Rvalue::TypeTest {
                operand,
                ty,
                dynamic,
                ..
            } => match dynamic {
                Some(name) => format!("{} is name {}", atom(operand), atom(name)),
                None => format!("{} is {}", atom(operand), type_ref(ty)),
            },
            Rvalue::TypeArgName {
                operand,
                index,
                param,
                ..
            } => format!("type_name::<{param}>({}[{index}])", atom(operand)),
            Rvalue::SelfRenderSlot { operand, index, .. } => {
                format!("render_slot({}[{index}])", atom(operand))
            }
            Rvalue::ComposeTypeArg { slots, cases, .. } => format!(
                "compose_type_arg([{}], {} cases)",
                slots.iter().map(atom).collect::<Vec<_>>().join(", "),
                cases.len()
            ),
            Rvalue::TypeSlotName { slot, .. } => format!("type_name(${})", atom(slot)),
            Rvalue::MakeGen { step, .. } => format!("make_gen({})", atom(step)),
            Rvalue::MakeFuture { thunk, .. } => format!("make_future({})", atom(thunk)),
            Rvalue::RunFuture { future, .. } => format!("run_future({})", atom(future)),
            Rvalue::PollFuture { future, .. } => format!("poll_future({})", atom(future)),
            Rvalue::Pending { .. } => "pending".to_string(),
            Rvalue::Spawn { future, .. } => format!("spawn({})", atom(future)),
            Rvalue::ScopeBegin { .. } => "scope_begin()".to_string(),
            Rvalue::ScopeReady { scope, .. } => format!("scope_ready({})", atom(scope)),
            Rvalue::ScopeEndAt { scope, .. } => format!("scope_end({})", atom(scope)),
            Rvalue::SpawnIsolate { callee, args, .. } => {
                let args = args.iter().map(atom).collect::<Vec<_>>().join(", ");
                format!("isolate({}, [{args}])", atom(callee))
            }
            Rvalue::MakeChannel { capacity, .. } => format!("channel({})", atom(capacity)),
            // One arm for the whole reflection surface. The head is the keyword and the operand
            // list is a function of the `ReflectArgs` shape, so a thirteenth query prints under its
            // own name without an arm here — the printer is a *derived* view of the IR again,
            // rather than a hand-maintained copy of its variant list.
            Rvalue::Reflect { which, args, .. } => {
                let kw = which.keyword();
                match args {
                    ReflectArgs::Nothing => format!("{kw}()"),
                    ReflectArgs::One(a) => format!("{kw}({})", atom(a)),
                    ReflectArgs::Two { name, arg } => {
                        format!("{kw}({}, {})", atom(name), atom(arg))
                    }
                    ReflectArgs::Dispatch {
                        recv: Some(recv),
                        name,
                        args,
                    } => format!("{kw}({}, {}, {})", atom(recv), atom(name), atom(args)),
                    ReflectArgs::Dispatch {
                        recv: None,
                        name,
                        args,
                    } => format!("{kw}({}, {})", atom(name), atom(args)),
                    // The element type is what a reader needs here, and it comes from the layout —
                    // `?` where the checker recorded none (`T` not packable, already an error).
                    ReflectArgs::Bytes { blob, layout, .. } => {
                        let ty = layout.as_ref().map(|l| l.type_name.as_str()).unwrap_or("?");
                        format!("{kw}<{}>({})", ty, atom(blob))
                    }
                }
            }
            Rvalue::DecodeTyped { name, text, .. } => {
                format!("decode_typed({}, {})", atom(name), atom(text))
            }
            Rvalue::TypedModuleCall {
                module, func, args, ..
            } => {
                let args: Vec<String> = args.iter().map(atom).collect();
                format!("{module}.{func}::<T>({})", args.join(", "))
            }
            Rvalue::TypedMethodCall {
                recv, method, args, ..
            } => {
                let args: Vec<String> = args.iter().map(atom).collect();
                format!("{}.{method}::<T>({})", atom(recv), args.join(", "))
            }
            Rvalue::ModuleFn { module, func, .. } => format!("fn {module}.{func}"),
            Rvalue::NativeModule { module, .. } => format!("module {module}"),
        }
    }
}

fn dst_prefix(dst: &Option<crate::Temp>) -> String {
    match dst {
        Some(t) => format!("%{} = ", t.0),
        None => String::new(),
    }
}

fn atoms(atoms: &[Atom]) -> String {
    atoms.iter().map(atom).collect::<Vec<_>>().join(", ")
}

/// A call's TYPE arguments, rendered as a turbofish — and rendered as nothing at all for the
/// overwhelming majority of calls, which forward none, so existing dumps are byte-identical.
fn turbofish(type_args: &[Atom]) -> String {
    if type_args.is_empty() {
        return String::new();
    }
    format!("::<{}>", atoms(type_args))
}

fn atom(atom: &Atom) -> String {
    match atom {
        Atom::Const(c) => match c {
            Const::Unit => "unit".to_string(),
            Const::Bool(b) => b.to_string(),
            Const::Int(i) => i.to_string(),
            Const::Float(f) => format!("{f:?}"),
            Const::F32(f) => format!("{f:?}f32"),
            Const::Str(s) => format!("{s:?}"),
        },
        Atom::Temp(t) => format!("%{}", t.0),
        Atom::Var { name, .. } => name.clone(),
    }
}

fn for_pattern(pattern: &crate::ForPattern) -> String {
    match pattern {
        crate::ForPattern::Single { name, .. } => name.clone(),
        // Lowering desugars a tuple for-pattern to a `Single` hidden var, so this is unreachable in
        // practice; kept for totality.
        crate::ForPattern::Tuple { names, .. } => {
            let names: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
            format!("({})", names.join(", "))
        }
    }
}

fn pattern_str(pattern: &crate::Pattern) -> String {
    use crate::Pattern;
    match pattern {
        Pattern::Wildcard { .. } => "_".to_string(),
        Pattern::Binding { name, .. } => name.clone(),
        Pattern::Int { value, .. } => value.to_string(),
        Pattern::Str { value, .. } => format!("{value:?}"),
        Pattern::Bool { value, .. } => value.to_string(),
        Pattern::Variant {
            type_name,
            variant,
            bindings,
            ..
        } => {
            let head = match type_name {
                Some(t) => format!("{t}.{variant}"),
                None => variant.clone(),
            };
            if bindings.is_empty() {
                head
            } else {
                let subs: Vec<String> = bindings.iter().map(pattern_str).collect();
                format!("{head}({})", subs.join(", "))
            }
        }
        Pattern::IsType { ty, .. } => format!("is {}", type_ref(ty)),
        Pattern::Tuple { elements, .. } => {
            let subs: Vec<String> = elements.iter().map(pattern_str).collect();
            format!("({})", subs.join(", "))
        }
    }
}

/// A [`TypeRef`]'s surface spelling for the IR dump, names **verbatim** — an IR dump is about
/// identity, so a linker-qualified `app.models.User` must not shorten to `User`. The same
/// `shape::type_source` the AST snapshot printer uses, for the same reason; this was a third
/// hand-written copy of that walk.
fn type_ref(ty: &TypeRef) -> String {
    noeta_ast::shape::type_source(ty)
}
