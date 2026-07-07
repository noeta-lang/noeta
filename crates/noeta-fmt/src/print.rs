//! The AST → text printer.
//!
//! **F0 scope:** a deliberately minimal subset — literals, `echo`, `return`, bare-expression
//! statements, and top-level `fn` with untyped, default-less params — emitting the canonical style
//! directly as strings. Anything else returns [`FmtError::Unsupported`]. F2 replaces this with the
//! Wadler `Doc` algebra and F3 grows it to full surface coverage; the public entry point
//! ([`print_program`]) and the canonical style it emits stay stable across that change.

use noeta_ast::{Expr, FnDecl, Param, Program, Stmt};

use crate::{FmtConfig, FmtError};

const INDENT: &str = "    "; // 4 spaces — the house style.

/// Render `program` to its canonical textual form (F0 subset).
pub fn print_program(program: &Program, config: &FmtConfig) -> Result<String, FmtError> {
    let mut p = Printer {
        out: String::new(),
        indent: 0,
        _config: config,
    };
    for (i, stmt) in program.stmts.iter().enumerate() {
        if i > 0 {
            p.out.push('\n');
        }
        p.stmt(stmt)?;
    }
    // A well-formed source file ends with exactly one newline.
    if !p.out.is_empty() {
        p.out.push('\n');
    }
    Ok(p.out)
}

struct Printer<'a> {
    out: String,
    indent: usize,
    // Threaded now so F3's config-sensitive constructs (match arrows, wrapping) have it in hand.
    _config: &'a FmtConfig,
}

impl Printer<'_> {
    fn line_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str(INDENT);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), FmtError> {
        match stmt {
            Stmt::Echo { value, .. } => {
                self.line_indent();
                self.out.push_str("echo ");
                self.expr(value)?;
                Ok(())
            }
            Stmt::Return { value, .. } => {
                self.line_indent();
                self.out.push_str("return");
                if let Some(value) = value {
                    self.out.push(' ');
                    self.expr(value)?;
                }
                Ok(())
            }
            Stmt::Expr { expr, .. } => {
                self.line_indent();
                self.expr(expr)
            }
            Stmt::Fn(decl) => self.fn_decl(decl),
            other => Err(unsupported("statement", other.span())),
        }
    }

    fn fn_decl(&mut self, decl: &FnDecl) -> Result<(), FmtError> {
        // F0 handles only the plain shape; richer signatures land in F3.
        if !decl.attrs.is_empty() {
            return Err(unsupported("function attributes", decl.span));
        }
        if !decl.type_params.is_empty() {
            return Err(unsupported("generic parameters", decl.span));
        }
        if decl.is_async {
            return Err(unsupported("async fn", decl.span));
        }
        if decl.ret.is_some() {
            return Err(unsupported("return-type annotation", decl.span));
        }

        self.line_indent();
        if decl.is_public {
            self.out.push_str("pub ");
        }
        self.out.push_str("fn ");
        self.out.push_str(&decl.name);
        self.out.push('(');
        for (i, param) in decl.params.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.param(param)?;
        }
        self.out.push_str(") {");

        if !decl.body.is_empty() {
            self.indent += 1;
            for stmt in &decl.body {
                self.out.push('\n');
                self.stmt(stmt)?;
            }
            self.indent -= 1;
            self.out.push('\n');
            self.line_indent();
        }
        self.out.push('}');
        Ok(())
    }

    fn param(&mut self, param: &Param) -> Result<(), FmtError> {
        if param.ty.is_some() {
            return Err(unsupported("parameter type annotation", param.span));
        }
        if param.default.is_some() {
            return Err(unsupported("parameter default", param.span));
        }
        self.out.push_str(&param.name);
        Ok(())
    }

    fn expr(&mut self, expr: &Expr) -> Result<(), FmtError> {
        match expr {
            Expr::Int { value, .. } => {
                self.out.push_str(&value.to_string());
                Ok(())
            }
            Expr::Float { value, .. } => {
                self.out.push_str(&format_float(*value));
                Ok(())
            }
            Expr::Bool { value, .. } => {
                self.out.push_str(if *value { "true" } else { "false" });
                Ok(())
            }
            Expr::Str { value, .. } => {
                self.out.push('"');
                escape_into(&mut self.out, value);
                self.out.push('"');
                Ok(())
            }
            Expr::Ident { name, .. } => {
                self.out.push_str(name);
                Ok(())
            }
            other => Err(unsupported("expression", other.span())),
        }
    }
}

fn unsupported(kind: &str, span: noeta_span::Span) -> FmtError {
    FmtError::Unsupported {
        construct: kind.to_string(),
        span,
    }
}

/// Escape a decoded string value back into the body of a `"…"` literal.
fn escape_into(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
}

/// Render a float so it always round-trips as a float literal (never bare `2`, which would re-lex as
/// an int). Uses Rust's shortest round-tripping form and appends `.0` when it has no fraction/exponent.
fn format_float(value: f64) -> String {
    let s = format!("{value}");
    if s.contains('.')
        || s.contains('e')
        || s.contains('E')
        || s.contains("inf")
        || s.contains("NaN")
    {
        s
    } else {
        format!("{s}.0")
    }
}
