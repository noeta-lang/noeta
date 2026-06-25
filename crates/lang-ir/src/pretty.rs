//! A stable textual dump of the Core IR, for golden tests and human review.
//!
//! The format is deliberately compact and deterministic — temporaries print as `%n`, source
//! variables by name, and every `let`-sequenced statement on its own line — so a lowering
//! change shows up as a reviewable diff in the snapshots.

use std::fmt::Write as _;

use crate::{
    Atom, Block, ClassDef, Const, Decl, Func, InterpPart, Program, Rvalue, Stmt, Thunk, TypeRef,
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
                    self.line(indent + 1, &format!("{} =>", pattern_str(&arm.pattern)));
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
            Decl::Enum(d) => self.line(indent, &format!("enum {}", d.name)),
            Decl::Record(d) => self.line(indent, &format!("record {}", d.name)),
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
            Rvalue::Binary {
                op,
                lhs,
                rhs,
                reuse,
                ..
            } => {
                // The reuse token is only rendered when set (the rare list self-append), so every
                // other binary dump — and its golden — is unchanged by Phase 5.
                let marker = if *reuse { " reuse" } else { "" };
                format!("{} {} {}{}", atom(lhs), op.symbol(), atom(rhs), marker)
            }
            Rvalue::Call { callee, args, .. } => format!("call {}({})", atom(callee), atoms(args)),
            Rvalue::Method {
                receiver,
                name,
                args,
                reuse,
                ..
            } => {
                // The reuse token renders only when set (the rare collection method self-update), so
                // every other method dump — and its golden — is unchanged.
                let marker = if *reuse { " reuse" } else { "" };
                format!("{}.{}({}){}", atom(receiver), name, atoms(args), marker)
            }
            Rvalue::Field { receiver, name, .. } => format!("{}.{}", atom(receiver), name),
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
            Rvalue::List { items, .. } => format!("[{}]", atoms(items)),
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
                // The reuse token is only rendered when set, so non-self-update object dumps (and
                // their goldens) are unchanged by Phase 5.
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
            Rvalue::As { operand, ty, .. } => format!("{}.as<{}>()", atom(operand), type_ref(ty)),
            Rvalue::TypeTest { operand, ty, .. } => {
                format!("{} is {}", atom(operand), type_ref(ty))
            }
            Rvalue::TypeOf { operand, .. } => format!("type_of({})", atom(operand)),
            Rvalue::AttributesOf { ty, .. } => format!("attributes_of<{}>", type_ref(ty)),
            Rvalue::RolesOf { .. } => "roles_of()".to_string(),
            Rvalue::Invoke {
                recv, name, args, ..
            } => format!("invoke({}, {}, {})", atom(recv), atom(name), atom(args)),
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

fn atom(atom: &Atom) -> String {
    match atom {
        Atom::Const(c) => match c {
            Const::Unit => "unit".to_string(),
            Const::Bool(b) => b.to_string(),
            Const::Int(i) => i.to_string(),
            Const::Float(f) => format!("{f:?}"),
            Const::Str(s) => format!("{s:?}"),
        },
        Atom::Temp(t) => format!("%{}", t.0),
        Atom::Var { name, .. } => name.clone(),
    }
}

fn for_pattern(pattern: &crate::ForPattern) -> String {
    match pattern {
        crate::ForPattern::Single { name, .. } => name.clone(),
        crate::ForPattern::Pair { first, second, .. } => format!("({first}, {second})"),
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
    }
}

fn type_ref(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Named { name, args, .. } => {
            if args.is_empty() {
                name.clone()
            } else {
                let args: Vec<String> = args.iter().map(type_ref).collect();
                format!("{name}<{}>", args.join(", "))
            }
        }
        TypeRef::Optional { inner, .. } => format!("?{}", type_ref(inner)),
        TypeRef::Union { members, .. } => {
            members.iter().map(type_ref).collect::<Vec<_>>().join(" | ")
        }
    }
}
