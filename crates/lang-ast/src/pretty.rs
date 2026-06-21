//! A stable, indented S-expression pretty-printer for the AST.
//!
//! This is the textual form snapshot tests assert against (never `Debug` of raw
//! structs, which is noisy and unstable). Spans are rendered as `@start..end` so a
//! span regression shows up directly in a snapshot diff. It is also the printer the
//! parse→print→parse property test (Slice 9) builds on.

use crate::{Expr, Program, Stmt};
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
        }
    }
}
