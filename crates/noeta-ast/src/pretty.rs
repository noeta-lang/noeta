//! A stable, indented S-expression pretty-printer for the AST.
//!
//! This is the textual form snapshot tests assert against (never `Debug` of raw
//! structs, which is noisy and unstable). Spans are rendered as `@start..end` so a
//! span regression shows up directly in a snapshot diff. It is also the printer the
//! parse→print→parse property test (Slice 9) builds on.

use crate::{
    AttrArg, AttrValue, ClassDecl, ClosureBody, EnumDecl, Expr, FieldDecl, FnDecl, ForPattern,
    ImplDecl, ObjectLit, Param, Pattern, Program, Stmt, StrPart, StructDecl, TraitDecl, TypeParam,
    TypeRef,
};
use noeta_span::Span;

/// Render an AST node to the canonical pretty form.
pub trait Pretty {
    fn pretty(&self, out: &mut String, indent: usize);

    fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        self.pretty(&mut out, 0);
        out
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn span(s: Span) -> String {
    format!("@{}..{}", s.start, s.end)
}

fn param_list(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Pretty for Program {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        out.push_str(&format!("(program {}", span(self.span)));
        for stmt in &self.stmts {
            out.push('\n');
            stmt.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for Stmt {
    fn pretty(&self, out: &mut String, level: usize) {
        match self {
            Stmt::Echo { value, span: s } => {
                indent(out, level);
                out.push_str(&format!("(echo {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Binding {
                mut_decl,
                name,
                value,
                span: s,
                ..
            } => {
                indent(out, level);
                let kw = if *mut_decl { "binding-mut" } else { "binding" };
                out.push_str(&format!("({kw} {name} {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Destructure {
                mut_decl,
                targets,
                value,
                span: s,
            } => {
                indent(out, level);
                let kw = if *mut_decl {
                    "destructure-mut"
                } else {
                    "destructure"
                };
                let names: Vec<&str> = targets.iter().map(|(n, _)| n.as_str()).collect();
                out.push_str(&format!("({kw} [{}] {}\n", names.join(" "), span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::Fn(decl) => decl.pretty(out, level),
            Stmt::Enum(decl) => decl.pretty(out, level),
            Stmt::Struct(decl) => decl.pretty(out, level),
            Stmt::Class(decl) => decl.pretty(out, level),
            Stmt::Impl(decl) => decl.pretty(out, level),
            Stmt::Trait(decl) => decl.pretty(out, level),
            Stmt::Namespace { path, span: s } => {
                indent(out, level);
                out.push_str(&format!("(namespace {} {})", path.join("."), span(*s)));
            }
            Stmt::Use {
                path,
                names,
                span: s,
            } => {
                indent(out, level);
                // A renamed import renders `Name=Alias`; a plain one just `Name`.
                let names: Vec<String> = names
                    .iter()
                    .map(|n| match &n.alias {
                        Some(a) => format!("{}={a}", n.name),
                        None => n.name.clone(),
                    })
                    .collect();
                out.push_str(&format!(
                    "(use {} [{}] {})",
                    path.join("."),
                    names.join(" "),
                    span(*s)
                ));
            }
            Stmt::Return { value, span: s } => {
                indent(out, level);
                match value {
                    Some(value) => {
                        out.push_str(&format!("(return {}\n", span(*s)));
                        value.pretty(out, level + 1);
                        out.push(')');
                    }
                    None => out.push_str(&format!("(return {})", span(*s))),
                }
            }
            Stmt::Yield { value, span: s } => {
                indent(out, level);
                out.push_str(&format!("(yield {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span: s,
            } => {
                indent(out, level);
                out.push_str(&format!("(if {}\n", span(*s)));
                cond.pretty(out, level + 1);
                out.push('\n');
                indent(out, level + 1);
                out.push_str("(then");
                for stmt in then_body {
                    out.push('\n');
                    stmt.pretty(out, level + 2);
                }
                out.push(')');
                if let Some(else_body) = else_body {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str("(else");
                    for stmt in else_body {
                        out.push('\n');
                        stmt.pretty(out, level + 2);
                    }
                    out.push(')');
                }
                out.push(')');
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span: s,
            } => {
                indent(out, level);
                let pat = match pattern {
                    ForPattern::Single { name, .. } => name.clone(),
                    ForPattern::Tuple { names, .. } => {
                        let names: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
                        format!("({})", names.join(", "))
                    }
                };
                out.push_str(&format!("(for [{pat}] {}\n", span(*s)));
                iterable.pretty(out, level + 1);
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::While {
                cond,
                body,
                span: s,
            } => {
                indent(out, level);
                out.push_str(&format!("(while {}\n", span(*s)));
                cond.pretty(out, level + 1);
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::Concurrent { body, span: s } => {
                indent(out, level);
                out.push_str(&format!("(concurrent {}", span(*s)));
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::Break { span: s } => {
                indent(out, level);
                out.push_str(&format!("(break {})", span(*s)));
            }
            Stmt::Continue { span: s } => {
                indent(out, level);
                out.push_str(&format!("(continue {})", span(*s)));
            }
            Stmt::Expr { expr, span: s } => {
                indent(out, level);
                out.push_str(&format!("(expr-stmt {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Stmt::TierBlock {
                tier,
                args,
                items,
                doc_text,
                span: s,
                ..
            } => {
                indent(out, level);
                out.push_str(&format!("(tier {tier}{} {}", attr_args_str(args), span(*s)));
                if let Some(text) = doc_text {
                    out.push_str(&format!(" :text {text:?}"));
                }
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
        }
    }
}

impl Pretty for FnDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        // A `@tier(…)` declaration rides on its runner/handler fn — render it so structural
        // comparisons (e.g. the formatter's safety gate) see it: dropping the directive is a
        // program change. The `expr:` field (expr-tiers arc) is part of that identity.
        let tier = match &self.tier {
            Some(t) => {
                let config = match &t.config {
                    Some((c, _)) => format!(", config: {c}"),
                    None => String::new(),
                };
                let text = match &t.text {
                    Some((lang, _)) => format!(", text: {lang:?}"),
                    None => String::new(),
                };
                let expr = match &t.expr {
                    Some((ty, _)) => format!(", expr: {ty}"),
                    None => String::new(),
                };
                format!("@tier({}{config}{text}{expr}) ", t.name)
            }
            None => String::new(),
        };
        out.push_str(&format!(
            "({tier}{}fn {}{}{} [{}] {}",
            if self.is_async { "async " } else { "" },
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            param_list(&self.params),
            span(self.span)
        ));
        for stmt in &self.body {
            out.push('\n');
            stmt.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for EnumDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let kind = if self.backing.is_some() {
            "enum-backed"
        } else {
            "enum"
        };
        let variants: Vec<String> = self
            .variants
            .iter()
            .map(|v| {
                if v.fields.is_empty() {
                    v.name.clone()
                } else {
                    format!("{}({})", v.name, param_list(&v.fields))
                }
            })
            .collect();
        out.push_str(&format!(
            "({kind} {}{}{} [{}] {}",
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            variants.join(" "),
            span(self.span)
        ));
        // An enum body may carry methods (the unified body, object-model slice 3); print them like a
        // class's so a method-bearing enum is visible in the AST snapshot. A variant-only enum prints
        // exactly as before (no trailing methods).
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        out.push(')');
    }
}

fn field_decl_str(field: &FieldDecl) -> String {
    // A trailing `=` marks a field carrying a default (slice 5) — the expression itself is not
    // inlined (it can be multi-line), only its presence is surfaced in the snapshot.
    let default = if field.default.is_some() { " =" } else { "" };
    if field.mut_field {
        format!("mut {}{default}", field.name)
    } else {
        format!("{}{default}", field.name)
    }
}

/// Render a declaration's generic parameters as `<A, B>` (or `<T: Comparable + Display>` when
/// bounded), or the empty string when there are none (so non-generic declarations' pretty output
/// is unchanged, as is any unbounded generic's).
fn type_params_str(params: &[TypeParam]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = params
            .iter()
            .map(|p| {
                if p.bounds.is_empty() {
                    p.name.clone()
                } else {
                    format!("{}: {}", p.name, p.bounds.join(" + "))
                }
            })
            .collect();
        format!("<{}>", parts.join(", "))
    }
}

/// Render `pub ` for an exported declaration, or the empty string otherwise (so module-private
/// declarations' pretty output is unchanged).
fn pub_str(is_public: bool) -> &'static str {
    if is_public { "pub " } else { "" }
}

/// Render directive/attribute arguments as `(name: value, value)`, or the empty string when there
/// are none (so a bare `@test { }` block's pretty output is unchanged). Used by the tier-block
/// printer so `@bench(iterations: 1000)` surfaces in snapshots.
fn attr_args_str(args: &[AttrArg]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|a| match &a.name {
            Some(name) => format!("{name}: {}", attr_value_str(&a.value)),
            None => attr_value_str(&a.value),
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// A compact rendering of an attribute-argument literal value, enough to make a snapshot legible.
fn attr_value_str(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) | AttrValue::Set(items) => format!(
            "[{}]",
            items
                .iter()
                .map(attr_value_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k:?}: {}", attr_value_str(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::Enum {
            enum_name, variant, ..
        } => format!("{enum_name}.{variant}"),
        AttrValue::Struct { type_name, .. } => format!("{type_name} {{…}}"),
        AttrValue::TypeRef(name) => name.clone(),
    }
}

impl Pretty for StructDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(struct {}{}{} [{}] {})",
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            fields.join(" "),
            span(self.span)
        ));
    }
}

impl Pretty for ClassDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(class {}{}{} [{}] {}",
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            fields.join(" "),
            span(self.span)
        ));
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for ImplDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        out.push_str(&format!(
            "(impl {} for {} {}",
            self.trait_name,
            self.target,
            span(self.span)
        ));
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for TraitDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        out.push_str(&format!("(trait {} {}", self.name, span(self.span)));
        for method in &self.methods {
            out.push('\n');
            method.sig.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for ObjectLit {
    fn pretty(&self, out: &mut String, level: usize) {
        // The header line is already indented by the caller (`Expr::pretty` emits the
        // leading indent before delegating here), so we start the text directly.
        out.push_str(&format!("(object {} {}", self.type_name, span(self.span)));
        for field in &self.fields {
            out.push('\n');
            indent(out, level + 1);
            out.push_str(&format!("(field {} {}\n", field.name, span(field.span)));
            field.value.pretty(out, level + 2);
            out.push(')');
        }
        if let Some(spread) = &self.spread {
            out.push('\n');
            indent(out, level + 1);
            out.push_str("(spread\n");
            spread.pretty(out, level + 2);
            out.push(')');
        }
        out.push(')');
    }
}

/// Render a pattern to a compact inline form for snapshots.
fn pattern_str(pattern: &Pattern) -> String {
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
                let inner: Vec<String> = bindings.iter().map(pattern_str).collect();
                format!("{head}({})", inner.join(", "))
            }
        }
        Pattern::IsType { ty, .. } => format!("is {}", type_ref_str(ty)),
        Pattern::Tuple { elements, .. } => {
            let inner: Vec<String> = elements.iter().map(pattern_str).collect();
            format!("({})", inner.join(", "))
        }
    }
}

impl Pretty for Expr {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        match self {
            Expr::Str { value, span: s } => {
                out.push_str(&format!("(str {:?} {})", value, span(*s)));
            }
            Expr::Int { value, span: s } => {
                out.push_str(&format!("(int {value} {})", span(*s)));
            }
            Expr::Float { value, span: s } => {
                out.push_str(&format!("(float {value} {})", span(*s)));
            }
            Expr::F32 { value, span: s } => {
                out.push_str(&format!("(f32 {value} {})", span(*s)));
            }
            Expr::F64 { value, span: s } => {
                out.push_str(&format!("(f64 {value} {})", span(*s)));
            }
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span: s,
            } => {
                let suffix = if *signed { 'i' } else { 'u' };
                out.push_str(&format!("(intn {magnitude}{suffix}{bits} {})", span(*s)));
            }
            Expr::Bool { value, span: s } => {
                out.push_str(&format!("(bool {value} {})", span(*s)));
            }
            Expr::Ident { name, span: s } => {
                out.push_str(&format!("(ident {name} {})", span(*s)));
            }
            Expr::Unary {
                op,
                operand,
                span: s,
            } => {
                out.push_str(&format!("(unary {:?} {}\n", op.symbol(), span(*s)));
                operand.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Binary {
                op,
                lhs,
                rhs,
                span: s,
            } => {
                out.push_str(&format!("(binary {:?} {}\n", op.symbol(), span(*s)));
                lhs.pretty(out, level + 1);
                out.push('\n');
                rhs.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Call {
                callee,
                args,
                span: s,
            } => {
                out.push_str(&format!("(call {}\n", span(*s)));
                callee.pretty(out, level + 1);
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::Closure {
                params,
                ret: _,
                body,
                span: s,
            } => {
                out.push_str(&format!("(closure [{}] {}\n", param_list(params), span(*s)));
                match body {
                    ClosureBody::Expr(e) => e.pretty(out, level + 1),
                    ClosureBody::Block(stmts) => {
                        for stmt in stmts {
                            stmt.pretty(out, level + 1);
                        }
                    }
                }
                out.push(')');
            }
            Expr::Pipeline {
                left,
                right,
                span: s,
            } => {
                out.push_str(&format!("(pipeline {}\n", span(*s)));
                left.pretty(out, level + 1);
                out.push('\n');
                right.pretty(out, level + 1);
                out.push(')');
            }
            Expr::List { items, span: s } => {
                out.push_str(&format!("(list {}", span(*s)));
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::Tuple { items, span: s } => {
                out.push_str(&format!("(tuple {}", span(*s)));
                for item in items {
                    out.push('\n');
                    item.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::TupleIndex {
                receiver,
                index,
                span: s,
            } => {
                out.push_str(&format!("(tuple-index {index} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span: s,
            } => {
                let op = if *inclusive { "range-incl" } else { "range" };
                out.push_str(&format!("({op} {}\n", span(*s)));
                start.pretty(out, level + 1);
                out.push('\n');
                end.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Map { entries, span: s } => {
                out.push_str(&format!("(map {}", span(*s)));
                for (key, value) in entries {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str("(entry\n");
                    key.pretty(out, level + 2);
                    out.push('\n');
                    value.pretty(out, level + 2);
                    out.push(')');
                }
                out.push(')');
            }
            Expr::Member {
                receiver,
                name,
                span: s,
                ..
            } => {
                out.push_str(&format!("(member {name} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Index {
                receiver,
                index,
                span: s,
            } => {
                out.push_str(&format!("(index {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push('\n');
                index.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Interp { parts, span: s } => {
                out.push_str(&format!("(interp {}", span(*s)));
                for part in parts {
                    out.push('\n');
                    match part {
                        StrPart::Literal(text) => {
                            indent(out, level + 1);
                            out.push_str(&format!("(lit {text:?})"));
                        }
                        StrPart::Hole(expr) => {
                            indent(out, level + 1);
                            out.push_str("(hole\n");
                            expr.pretty(out, level + 2);
                            out.push(')');
                        }
                    }
                }
                out.push(')');
            }
            Expr::Match {
                scrutinee,
                arms,
                span: s,
            } => {
                out.push_str(&format!("(match {}\n", span(*s)));
                scrutinee.pretty(out, level + 1);
                for arm in arms {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str(&format!("(arm {}\n", pattern_str(&arm.pattern)));
                    arm.body.pretty(out, level + 2);
                    out.push(')');
                }
                out.push(')');
            }
            Expr::Object(lit) => lit.pretty(out, level),
            Expr::Try { expr, span: s } => {
                out.push_str(&format!("(try {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Await { expr, span: s } => {
                out.push_str(&format!("(await {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Spawn {
                future,
                isolate,
                span: s,
            } => {
                let kw = if *isolate { "isolate" } else { "spawn" };
                out.push_str(&format!("({kw} {}\n", span(*s)));
                future.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Coalesce {
                value,
                fallback,
                span: s,
            } => {
                out.push_str(&format!("(coalesce {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push('\n');
                fallback.pretty(out, level + 1);
                out.push(')');
            }
            Expr::As { expr, ty, span: s } => {
                out.push_str(&format!("(as {} {}\n", type_ref_str(ty), span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TypeTest { expr, ty, span: s } => {
                out.push_str(&format!("(is {} {}\n", type_ref_str(ty), span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
            Expr::AttributesOf { ty, span: s } => {
                out.push_str(&format!(
                    "(attributes_of {} {})",
                    type_ref_str(ty),
                    span(*s)
                ));
            }
            Expr::TypeOf { value, span: s } => {
                out.push_str(&format!("(type_of {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::FromBytes { ty, blob, span: s } => {
                out.push_str(&format!("(from_bytes {} {}\n", type_ref_str(ty), span(*s)));
                blob.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Channel {
                elem,
                capacity,
                span: s,
            } => {
                out.push_str(&format!("(channel {} {}\n", type_ref_str(elem), span(*s)));
                capacity.pretty(out, level + 1);
                out.push(')');
            }
            Expr::RolesOf { ty, span: s } => match ty {
                Some(ty) => out.push_str(&format!("(roles_of {} {})", type_ref_str(ty), span(*s))),
                None => out.push_str(&format!("(roles_of {})", span(*s))),
            },
            Expr::TypedModuleCall {
                recv,
                func,
                ty,
                args,
                span: s,
                ..
            } => {
                out.push_str(&format!(
                    "(typed-call {func} {} {}\n",
                    type_ref_str(ty),
                    span(*s)
                ));
                recv.pretty(out, level + 1);
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::FieldSet {
                receiver,
                field,
                value,
                span: s,
                ..
            } => {
                out.push_str(&format!("(field-set {field} {}\n", span(*s)));
                receiver.pretty(out, level + 1);
                out.push('\n');
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TierExpr {
                tier,
                statics,
                holes,
                span: s,
                ..
            } => {
                out.push_str(&format!("(tier-expr {tier} {}", span(*s)));
                for (i, static_) in statics.iter().enumerate() {
                    out.push('\n');
                    indent(out, level + 1);
                    out.push_str(&format!("(static {static_:?})"));
                    if let Some(hole) = holes.get(i) {
                        out.push('\n');
                        hole.pretty(out, level + 1);
                    }
                }
                out.push(')');
            }
            Expr::NativeFnRef {
                module,
                func,
                span: s,
            } => {
                out.push_str(&format!("(native-fn {module}.{func} {})", span(*s)));
            }
            Expr::Invoke {
                recv,
                name,
                args,
                span: s,
            } => {
                out.push_str(&format!("(invoke {}\n", span(*s)));
                recv.pretty(out, level + 1);
                out.push('\n');
                name.pretty(out, level + 1);
                out.push('\n');
                args.pretty(out, level + 1);
                out.push(')');
            }
        }
    }
}

/// Render a [`TypeRef`] back to its surface spelling (`int`, `List<int>`, `?User`) for snapshots.
fn type_ref_str(ty: &TypeRef) -> String {
    match ty {
        TypeRef::Optional { inner, .. } => format!("?{}", type_ref_str(inner)),
        TypeRef::DynTrait { trait_name, .. } => format!("dyn {trait_name}"),
        TypeRef::Named { name, args, .. } if args.is_empty() => name.clone(),
        TypeRef::Named { name, args, .. } => {
            let args: Vec<String> = args.iter().map(type_ref_str).collect();
            format!("{name}<{}>", args.join(", "))
        }
        TypeRef::Union { members, .. } => members
            .iter()
            .map(type_ref_str)
            .collect::<Vec<_>>()
            .join(" | "),
        TypeRef::Tuple { elements, .. } => {
            let elements: Vec<String> = elements.iter().map(type_ref_str).collect();
            format!("({})", elements.join(", "))
        }
        TypeRef::Fn { params, ret, .. } => {
            let params: Vec<String> = params.iter().map(type_ref_str).collect();
            format!("({}) -> {}", params.join(", "), type_ref_str(ret))
        }
    }
}
