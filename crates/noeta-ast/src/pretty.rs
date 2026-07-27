//! A stable, indented S-expression pretty-printer for the AST.
//!
//! This is the textual form snapshot tests assert against (never `Debug` of raw
//! structs, which is noisy and unstable). Spans are rendered as `@start..end` so a
//! span regression shows up directly in a snapshot diff. It is also the printer the
//! parse→print→parse property test (Slice 9) builds on.

use crate::{
    AttrArg, AttrValue, CallArg, ClassDecl, ClosureBody, EnumDecl, Expr, FieldDecl, FnDecl,
    ForPattern, ImplDecl, ObjectLit, Param, Pattern, Program, Stmt, StrPart, StructDecl,
    TraitBound, TraitDecl, TypeOperand, TypeParam, TypeRef,
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

/// A callable's parameters, for the S-expression dump.
///
/// Each parameter renders its `#[...]` attributes ahead of its name. The fmt safety gate compares
/// this form before and after formatting, so an unrendered attribute would be an attribute the
/// formatter could silently drop — the same failure the labelled-argument rendering below exists to
/// prevent, and a worse one here: dropping `#[Arg(short: "r")]` changes what a signature-driven
/// framework generates while leaving the program's own behaviour identical, so nothing else would
/// notice.
fn param_list(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            let attrs: String = p
                .attrs
                .iter()
                .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
                .collect();
            format!("{attrs}{}", p.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Pretty for CallArg {
    /// A labelled argument renders its label. The fmt safety gate compares this form before and
    /// after formatting, so an unrendered label would be a label the formatter could silently
    /// drop — turning `f(b: 1, a: 2)` into `f(1, 2)` and changing which parameter each value
    /// binds to. That is precisely the failure this representation exists to prevent.
    fn pretty(&self, out: &mut String, level: usize) {
        match &self.name {
            Some(name) => {
                indent(out, level);
                out.push_str(&format!("(arg {name} {}\n", span(self.span)));
                self.value.pretty(out, level + 1);
                out.push(')');
            }
            None => self.value.pretty(out, level),
        }
    }
}

/// A reflection surface's type operand. The two arms render **differently** on purpose: the fmt
/// safety gate compares this form before and after formatting, so a formatter that turned a
/// turbofish `field_specs_of::<T>()` into the string call `field_specs_of("T")` (or the reverse)
/// must show up as a diff rather than as an equal-looking `(str "T")` either way.
impl Pretty for TypeOperand {
    fn pretty(&self, out: &mut String, level: usize) {
        match self {
            TypeOperand::Static(ty) => {
                indent(out, level);
                out.push_str(&format!(
                    "(type-arg {} {})",
                    type_ref_str(ty),
                    span(ty.span())
                ));
            }
            TypeOperand::Dynamic(e) => e.pretty(out, level),
        }
    }
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
        // Leading `@<tier>` method directives (`@test`, `@doc { … }`) — rendered into the structural
        // skeleton so the formatter's safety gate treats dropping or altering one as a program
        // change (a text tier's body is part of its identity, like the `@tier` `text:` field above).
        let directives: String = self
            .directives
            .iter()
            .map(|d| {
                let args = if d.args.is_empty() {
                    String::new()
                } else {
                    format!("({})", d.args.len())
                };
                let body = match &d.doc_text {
                    Some(text) => format!(" {{{text}}}"),
                    None => String::new(),
                };
                format!("@{}{args}{body} ", d.name)
            })
            .collect();
        let attrs: String = self
            .attrs
            .iter()
            .map(|a| format!("#[{}{}] ", a.name, attr_args_str(&a.args)))
            .collect();
        // The `use (…)` capture clause is part of the sealed fn's identity: dropping it would
        // strip the body's access to its captured bindings, so the safety gate must see it.
        let captures = if self.captures.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = self.captures.iter().map(|(n, _)| n.as_str()).collect();
            format!(" use ({})", names.join(", "))
        };
        out.push_str(&format!(
            "({tier}{directives}{attrs}{}fn {}{}{}{captures} [{}] {}",
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
            "({kind} {}{}{}{} [{}] {}",
            decorators_str(&self.decorators),
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

/// The leading decorators of a declaration, rendered into the structural snapshot — the fmt safety
/// gate compares this output, so a formatter dropping a decorator (a whole `@derive`, one of its
/// `member: target` bindings or `via:`, a `@packed` layout, an `@attribute`/`@role` tag, a
/// `@validated` marker, a `#[...]` data attribute) is a DETECTED program change, not a silent one.
/// Empty when the declaration is undecorated, so undecorated snapshots are untouched.
///
/// Emission order is [`BuiltinDirective::ALL`](crate::BuiltinDirective::ALL) order, with the
/// `#[...]` data attributes last. Driving the loop off `ALL` — rather than a hand-written sequence
/// of `if let`s — is what makes a newly added directive a compile error here instead of a silently
/// unrendered (and therefore ungated) decorator. The other half of that lock is [`Decorators`]
/// itself: one struct shared by every declaration kind, so a directive cannot be rendered for a
/// struct but forgotten for an enum, which is precisely how `@validated` came to be ungated.
fn decorators_str(d: &crate::Decorators) -> String {
    let mut parts: Vec<String> = Vec::new();
    for directive in crate::BuiltinDirective::ALL {
        match directive {
            crate::BuiltinDirective::Derive => {
                for spec in &d.derives {
                    let mut s = spec.name.clone();
                    if !spec.args.is_empty() {
                        s.push_str(&format!(
                            "<{}>",
                            spec.args
                                .iter()
                                .map(type_ref_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if let Some((via, _)) = &spec.via {
                        s.push_str(&format!(", via: {via}"));
                    }
                    for b in &spec.bindings {
                        s.push_str(&format!(", {}: {}", b.member, b.target));
                    }
                    parts.push(format!("@derive({s})"));
                }
            }
            crate::BuiltinDirective::Attribute => {
                if let Some(kinds) = d.attribute.as_deref() {
                    if kinds.is_empty() {
                        parts.push("@attribute".to_string());
                    } else {
                        parts.push(format!(
                            "@attribute({})",
                            kinds
                                .iter()
                                .map(|(k, _)| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }
            crate::BuiltinDirective::Role => {
                if let Some(tags) = d.role.as_deref() {
                    parts.push(format!(
                        "@role({})",
                        tags.iter()
                            .map(|t| if t.enum_name.is_empty() {
                                t.variant.clone()
                            } else {
                                format!("{}.{}", t.enum_name, t.variant)
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            crate::BuiltinDirective::Semantic => {
                if d.semantic.is_some() {
                    parts.push("@semantic".to_string());
                }
            }
            crate::BuiltinDirective::Packed => {
                if let Some(p) = d.packed {
                    parts.push(match p.layout {
                        crate::PackedLayout::Row => "@packed".to_string(),
                        crate::PackedLayout::Column => "@packed(Layout.Column)".to_string(),
                    });
                }
            }
            crate::BuiltinDirective::Validated => {
                if d.validated.is_some() {
                    parts.push("@validated".to_string());
                }
            }
            // `@tier(...)` decorates a `fn`, not a type declaration, and is carried in
            // `FnDecl::tier` rather than in any of the fields above — it is rendered by the `fn`
            // printer. Listing it here (instead of a `_ =>` catch-all) is what keeps this match
            // exhaustive, so a future directive cannot slip through unrendered.
            crate::BuiltinDirective::Tier => {}
        }
    }
    // Directives the decorator grammar does not own (an extension's, a misplaced `@tier`, a typo).
    // They MUST appear in the gate: the formatter has to round-trip them, and a decorator the gate
    // cannot see is a decorator the formatter may silently delete — exactly how `@validated` went
    // unnoticed before it was rendered here.
    for f in &d.foreign {
        parts.push(format!("@{}{}", f.name, attr_args_str(&f.args)));
    }
    for a in &d.attrs {
        parts.push(format!("#[{}{}]", a.name, attr_args_str(&a.args)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        let mut s = parts.join(" ");
        s.push(' ');
        s
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
                    let bounds: Vec<String> = p.bounds.iter().map(trait_bound_str).collect();
                    format!("{}: {}", p.name, bounds.join(" + "))
                }
            })
            .collect();
        format!("<{}>", parts.join(", "))
    }
}

/// Render one trait bound: the bare name, or `Name<args>` for an instantiated bound.
fn trait_bound_str(b: &TraitBound) -> String {
    if b.args.is_empty() {
        b.name.clone()
    } else {
        let args: Vec<String> = b.args.iter().map(type_ref_str).collect();
        format!("{}<{}>", b.name, args.join(", "))
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
pub(crate) fn attr_value_str(value: &AttrValue) -> String {
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
        // Rendered WITH its generic arguments: the fmt safety gate compares this output, so a
        // formatter dropping the `<Json>` from `@derive(Serialize<Json>)` must be detectable.
        AttrValue::TypeRef { name, args } if args.is_empty() => name.clone(),
        AttrValue::TypeRef { name, args } => format!(
            "{name}<{}>",
            args.iter().map(type_ref_str).collect::<Vec<_>>().join(", ")
        ),
    }
}

impl Pretty for StructDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(struct {}{}{}{} [{}] {}",
            decorators_str(&self.decorators),
            pub_str(self.is_public),
            self.name,
            type_params_str(&self.type_params),
            fields.join(" "),
            span(self.span)
        ));
        // A struct body may carry methods (the unified body) — print them like a class's, so a
        // formatter dropping one is a detected change. A field-only struct prints as before.
        for method in &self.methods {
            out.push('\n');
            method.pretty(out, level + 1);
        }
        out.push(')');
    }
}

impl Pretty for ClassDecl {
    fn pretty(&self, out: &mut String, level: usize) {
        indent(out, level);
        let fields: Vec<String> = self.fields.iter().map(field_decl_str).collect();
        out.push_str(&format!(
            "(class {}{}{}{} [{}] {}",
            decorators_str(&self.decorators),
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
        let args = if self.trait_args.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                self.trait_args
                    .iter()
                    .map(type_ref_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!(
            "(impl {}{args} for {} {}",
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
        // A trait's decorators were previously not rendered at all, which left every `@derive`/
        // `@role`/`@attribute`/`@semantic`/`@packed` on a trait outside the fmt safety gate — the
        // formatter could drop one without the gate noticing. They are misplaced directives (the
        // checker reports E0054), but "the checker rejects it" is not a reason for the *formatter*
        // to be free to silently rewrite the program: fmt runs on code that does not yet check.
        out.push_str(&format!(
            "(trait {}{} {}",
            decorators_str(&self.decorators),
            self.name,
            span(self.span)
        ));
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
        // A target-typed `.{ … }` dumps its head as `.{` — the name is not in the source, and the
        // dump is a faithful view of the AST, not of what the checker will later infer.
        let head = self.type_name.as_deref().unwrap_or(".{");
        out.push_str(&format!("(object {} {}", head, span(self.span)));
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
                    if let Some(guard) = &arm.guard {
                        indent(out, level + 2);
                        out.push_str("(guard\n");
                        guard.pretty(out, level + 3);
                        out.push_str(")\n");
                    }
                    match &arm.body {
                        ClosureBody::Expr(e) => e.pretty(out, level + 2),
                        ClosureBody::Block(stmts) => {
                            indent(out, level + 2);
                            out.push_str("(block");
                            for stmt in stmts {
                                out.push('\n');
                                stmt.pretty(out, level + 3);
                            }
                            out.push(')');
                        }
                    }
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
            Expr::FieldsOf { value, span: s } => {
                out.push_str(&format!("(fields_of {}\n", span(*s)));
                value.pretty(out, level + 1);
                out.push(')');
            }
            Expr::TraitsOf { value, span: s } => {
                out.push_str(&format!("(traits_of {}\n", span(*s)));
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
            Expr::ParamsOf { target, span: s } => {
                out.push_str(&format!("(params_of {}\n", span(*s)));
                target.pretty(out, level + 1);
                out.push(')');
            }
            Expr::ReturnsOf { target, span: s } => {
                out.push_str(&format!("(returns_of {}\n", span(*s)));
                target.pretty(out, level + 1);
                out.push(')');
            }
            Expr::FieldSpecsOf { name, span: s } => {
                out.push_str(&format!("(field_specs_of {}\n", span(*s)));
                name.pretty(out, level + 1);
                out.push(')');
            }
            Expr::Construct {
                name,
                fields,
                span: s,
            } => {
                out.push_str(&format!("(construct {}\n", span(*s)));
                name.pretty(out, level + 1);
                out.push('\n');
                fields.pretty(out, level + 1);
                out.push(')');
            }
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
            Expr::TypedCall {
                name,
                type_args,
                args,
                span: s,
                ..
            } => {
                let tys: Vec<String> = type_args.iter().map(type_ref_str).collect();
                out.push_str(&format!(
                    "(typed-fn-call {name} <{}> {}",
                    tys.join(", "),
                    span(*s)
                ));
                for arg in args {
                    out.push('\n');
                    arg.pretty(out, level + 1);
                }
                out.push(')');
            }
            Expr::TypedMethodCall {
                recv,
                name,
                type_args,
                args,
                span: s,
                ..
            } => {
                let tys: Vec<String> = type_args.iter().map(type_ref_str).collect();
                out.push_str(&format!(
                    "(typed-method-call {name} <{}> {}\n",
                    tys.join(", "),
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
                // The free-fn form prints as `invoke-free`, so a snapshot can never confuse the two
                // dispatch namespaces by operand count alone.
                let head = if recv.is_some() {
                    "invoke"
                } else {
                    "invoke-free"
                };
                out.push_str(&format!("({head} {}\n", span(*s)));
                if let Some(recv) = recv {
                    recv.pretty(out, level + 1);
                    out.push('\n');
                }
                name.pretty(out, level + 1);
                out.push('\n');
                args.pretty(out, level + 1);
                out.push(')');
            }
        }
    }
}

/// Render a [`TypeRef`] back to its surface spelling (`int`, `List<int>`, `?User`) for snapshots.
///
/// Names stay **verbatim** — a snapshot is about identity, so `app.models.User` must not shorten to
/// `User` and hide a real difference in a diff. That is `shape::type_source`, the same walk the
/// extension-facing `shape::type_spelling` runs with a different name transform.
pub(crate) fn type_ref_str(ty: &TypeRef) -> String {
    crate::shape::type_source(ty)
}
