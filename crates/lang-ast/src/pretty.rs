//! A stable, indented S-expression pretty-printer for the AST.
//!
//! This is the textual form snapshot tests assert against (never `Debug` of raw
//! structs, which is noisy and unstable). Spans are rendered as `@start..end` so a
//! span regression shows up directly in a snapshot diff. It is also the printer the
//! parse→print→parse property test (Slice 9) builds on.

use crate::{
    ClassDecl, EnumDecl, Expr, FieldDecl, FnDecl, ForPattern, ObjectLit, Param, Pattern, Program,
    RecordDecl, Stmt, StrPart,
};
use lang_span::Span;

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
            Stmt::Fn(decl) => decl.pretty(out, level),
            Stmt::Enum(decl) => decl.pretty(out, level),
            Stmt::Record(decl) => decl.pretty(out, level),
            Stmt::Class(decl) => decl.pretty(out, level),
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
                    ForPattern::Pair { first, second, .. } => format!("{first} {second}"),
                };
                out.push_str(&format!("(for [{pat}] {}\n", span(*s)));
                iterable.pretty(out, level + 1);
                for stmt in body {
                    out.push('\n');
                    stmt.pretty(out, level + 1);
                }
                out.push(')');
            }
            Stmt::Expr { expr, span: s } => {
                indent(out, level);
                out.push_str(&format!("(expr-stmt {}\n", span(*s)));
                expr.pretty(out, level + 1);
                out.push(')');
            }
        }
    }
}

impl Pretty for FnDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        out.push_str(&format!(
            "(fn {} [{}] {}",
            self.name,
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
            "({kind} {} [{}] {})",
            self.name,
            variants.join(" "),
            span(self.span)
        ));
    }
}

fn field_decl_str(field: &FieldDecl) -> String {
    if field.mut_field {
        format!("mut {}", field.name)
    } else {
        field.name.clone()
    }
}

impl Pretty for RecordDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(record {} [{}] {})",
            self.name,
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
            "(class {} [{}] {}",
            self.name,
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
                body,
                span: s,
            } => {
                out.push_str(&format!("(closure [{}] {}\n", param_list(params), span(*s)));
                body.pretty(out, level + 1);
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
        }
    }
}
