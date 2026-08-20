//! Signature help for `textDocument/signatureHelp`.
//!
//! While the cursor is inside a function call's argument list, surface the called function's
//! signature and highlight the argument being typed. Detection is **token-based**, not AST-based: a
//! half-typed call (`foo(1, |`) has an unbalanced paren and does not parse, so a scan over the token
//! stream finds the innermost unclosed call paren before the cursor and counts the top-level commas
//! up to it. The callee name then resolves against the top-level function declarations.
//!
//! Covers calls of top-level functions (`f(`) and method calls (`recv.m(`); the caller resolves the
//! receiver's type for the latter. [`enclosing_call`] finds the call context from the token stream;
//! [`from_decl`] renders a resolved function/method declaration into a backend-neutral
//! [`SignatureData`] the server maps to an LSP `SignatureHelp`.

use noeta_ast::{BuiltinDirective, FnDecl};
use noeta_lexer::{Token, TokenKind};
use noeta_span::Span;

use crate::symbols;

/// The resolved signature to show: the full label (`name(a: T, b: U) -> R`), each parameter rendered
/// on its own (for per-parameter highlighting), and the index of the argument the cursor is in.
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureData {
    pub label: String,
    pub parameters: Vec<String>,
    /// 0-based index of the active argument, clamped to the last parameter.
    pub active_param: usize,
}

/// Render a resolved function/method declaration into the signature to show, with `active` clamped to
/// the last parameter (so a trailing/extra comma still highlights the final one).
pub fn from_decl(decl: &FnDecl, active: usize) -> SignatureData {
    let parameters: Vec<String> = decl.params.iter().map(symbols::param_detail).collect();
    let label = format!("{}({})", decl.name, parameters.join(", "));
    let label = match &decl.ret {
        Some(ret) => format!("{label} -> {}", symbols::render_type_ref(ret)),
        None => label,
    };
    let active_param = active.min(decl.params.len().saturating_sub(1));
    SignatureData {
        label,
        parameters,
        active_param,
    }
}

/// The signature to show while the cursor is inside a **directive's** argument list (C5): a
/// synthetic signature naming the directive's own vocabulary — decorators from their fixed
/// grammar, a tier annotation (`@bench(…)`) from its config attribute's fields. `None` when the
/// directive takes no arguments the cursor could be typing (an unknown tier, a knob-less `@test`).
pub fn directive_signature(
    ctxt: &crate::completion::DirectiveArgContext,
    program: &noeta_ast::Program,
) -> Option<SignatureData> {
    let one = |label: &str, param: &str| SignatureData {
        label: label.to_string(),
        parameters: vec![param.to_string()],
        active_param: 0,
    };
    match BuiltinDirective::from_name(&ctxt.directive) {
        Some(BuiltinDirective::Derive) => {
            let traits = noeta_types::BUILTIN_TRAITS
                .iter()
                .filter(|t| t.has_builtin_recipe())
                .map(|t| match t.generic_arity() {
                    0 => t.name().to_string(),
                    _ => format!("{}<Format>", t.name()),
                })
                .collect::<Vec<_>>()
                .join(" | ");
            Some(one(
                &format!("@derive(Trait, …) — {traits} | a fully-defaulted user trait"),
                "Trait",
            ))
        }
        Some(BuiltinDirective::Role) => Some(one(
            &format!(
                "@role(Enum.Variant, …) — {}.{{{}}} or any @semantic enum",
                noeta_ast::reflect::SEMANTIC_ENUM,
                noeta_ast::reflect::SEMANTIC_VARIANTS.join(", ")
            ),
            "Enum.Variant",
        )),
        Some(BuiltinDirective::Attribute) => Some(one(
            &format!(
                "@attribute(Kind, …) — {}",
                noeta_ast::reflect::ATTRIBUTE_TARGET_KINDS.join(", ")
            ),
            "Kind",
        )),
        Some(BuiltinDirective::Packed) => {
            let variants = noeta_ast::reflect::LAYOUT_VARIANTS
                .iter()
                .map(|v| format!("{}.{v}", noeta_ast::reflect::LAYOUT_ENUM))
                .collect::<Vec<_>>()
                .join(" | ");
            Some(one(&format!("@packed({variants})"), &variants))
        }
        // Directives whose signature is fully described by the metadata table: the parameter names
        // are static, so there is nothing to compute. A directive that takes no arguments has an
        // empty `params` and correctly yields no signature.
        //
        // The arms above stay hand-written because their labels interpolate a *vocabulary* that
        // this table deliberately does not own — the derivable traits come from `noeta-types`, the
        // `Layout` variants and semantic-enum variants from `reflect`. The table says `@packed`
        // takes one argument; it does not say which layouts exist.
        Some(directive) => {
            let info = directive.info();
            if info.params.is_empty() {
                return None;
            }
            let parameters: Vec<String> = info.params.iter().map(|p| p.to_string()).collect();
            Some(SignatureData {
                label: format!("@{directive}({})", parameters.join(", ")),
                active_param: ctxt.active.min(parameters.len() - 1),
                parameters,
            })
        }
        // Not a built-in directive — a tier annotation: its signature is the config attribute's
        // field list. Both halves of the tier name-space resolve through the one registry lookup,
        // so the dispatch is total over `DirectiveKind` (a future kind must state its signature
        // here) rather than a pair of `or_else`d probes ending in an implicit "must be unknown".
        None => {
            use noeta_check::directives::{DirectiveKind, DirectiveRegistry};
            let tier = ctxt.directive.as_str();
            let reg = noeta_stdlib::registry::single_registry_process();
            let directives = DirectiveRegistry::collect_with_registry(program, reg);
            let config = match directives.lookup(tier)? {
                DirectiveKind::ExtTier(t) => t.config.map(String::from)?,
                DirectiveKind::DeclaredTier(d) => d.config.clone()?,
                // An extension's own directive carries its parameter names in its declaration —
                // no config attribute to go and read.
                DirectiveKind::ExtDirective(d) => {
                    if d.params.is_empty() {
                        return None;
                    }
                    let parameters: Vec<String> =
                        d.params.iter().map(|p| (*p).to_string()).collect();
                    return Some(SignatureData {
                        label: format!("@{}({}) — {}", d.name, parameters.join(", "), d.detail),
                        active_param: ctxt.active.min(parameters.len() - 1),
                        parameters,
                    });
                }
                // `from_name` already returned `None`, so the lookup cannot answer `Builtin`.
                DirectiveKind::Builtin(_) => return None,
            };
            let parameters: Vec<String> =
                if let Some(attr) = noeta_stdlib::registry::find_ext_attribute(&config) {
                    attr.fields
                        .iter()
                        .map(|f| {
                            use noeta_stdlib::registry::AttrFieldType;
                            let ty = match f.ty {
                                AttrFieldType::Int => "int",
                                AttrFieldType::Str => "string",
                                AttrFieldType::Dyn => "dyn",
                            };
                            format!(
                                "{}: {ty}{}",
                                f.name,
                                if f.default.is_some() { "?" } else { "" }
                            )
                        })
                        .collect()
                } else {
                    program.stmts.iter().find_map(|stmt| match stmt {
                        noeta_ast::Stmt::Struct(decl) if decl.name == config => Some(
                            decl.fields
                                .iter()
                                .map(|f| {
                                    let ty =
                                        f.ty.as_ref()
                                            .map(symbols::render_type_ref)
                                            .unwrap_or_else(|| "dyn".to_string());
                                    format!("{}: {ty}", f.name)
                                })
                                .collect(),
                        ),
                        _ => None,
                    })?
                };
            if parameters.is_empty() {
                return None;
            }
            Some(SignatureData {
                label: format!("@{tier}({}) — {config} knobs", parameters.join(", ")),
                active_param: ctxt.active.min(parameters.len() - 1),
                parameters,
            })
        }
    }
}

/// The call the cursor is inside: the callee name, the 0-based active argument index, whether the
/// call head carried a turbofish, and — when the call is a method call `recv.callee(` — the span of
/// the receiver `recv` (so the caller can resolve its type and find the method). `receiver` is
/// `None` for a plain function call.
#[derive(Debug)]
pub struct CallContext {
    pub callee: String,
    /// The 0-based index of the argument the cursor is **in** — the top-level comma count. This is
    /// the LSP `activeParameter`, the one to highlight, and it is deliberately *not* a count of
    /// arguments: `f(` and `f(x` are both index 0.
    pub active: usize,
    pub receiver: Option<Span>,
    /// Whether the head was written `callee::<…>(` rather than `callee(`. Read by the reflection
    /// intrinsics, several of which have a turbofish form and a bare form that differ in arity
    /// (`construct::<T>(fields)` vs `construct(name, fields)`) — the two cannot be told apart from
    /// the callee and the comma count alone.
    pub turbofish: bool,
    /// How many arguments have been **written or begun** — `f(` is 0, `f(x` is 1, `f(x,` is 2.
    ///
    /// A second count rather than a richer [`Self::active`] because the two answer different
    /// questions and only one of them is ambiguous. "Which parameter do I highlight?" is the comma
    /// count and is right for an empty list as much as a full one. "Which *shape* is this call?"
    /// needs to know whether the list is empty at all, and the comma count cannot say: `roles_of()`
    /// and `roles_of(x` are both zero commas, and they are two different queries — the whole index
    /// and one enum's. Folding the fact into `active` as an `Option` would have made every reader of
    /// the highlight index handle a case that never mattered to it.
    pub arity_so_far: usize,
}

/// One bracket frame on the scan stack. A call frame (a `(` opened right after a callee head) counts
/// its own top-level commas; a plain grouping/list/map frame does not, so a comma inside an argument
/// (`foo([a, b|])`) is not mistaken for an argument separator of the call.
struct Frame {
    callee: Option<String>,
    receiver: Option<Span>,
    turbofish: bool,
    active: usize,
    /// Whether any token at all has appeared inside this frame — the one bit that separates an empty
    /// argument list from a first argument being typed.
    started: bool,
}

/// Scan the tokens before `offset`, tracking bracket nesting, and return the innermost enclosing
/// *call* frame — the callee named just before an unclosed `(`, its receiver when the callee is
/// preceded by `.`, and the count of top-level commas inside it as the active-argument index.
pub fn enclosing_call(tokens: &[Token], text: &str, offset: u32) -> Option<CallContext> {
    let mut stack: Vec<Frame> = Vec::new();

    for (i, token) in tokens
        .iter()
        .enumerate()
        .take_while(|(_, t)| t.span.start < offset)
    {
        // Any token inside the innermost open frame means its argument list is no longer empty.
        // Marked *before* the match so an opening bracket counts for the frame it appears in
        // (`f([` has begun an argument) rather than for the frame it is about to open, and so the
        // `(` that opens a call marks the caller's frame, never the fresh one.
        if let Some(frame) = stack.last_mut() {
            frame.started = true;
        }
        match token.kind {
            TokenKind::LParen => {
                let head = call_head(tokens, i, text);
                stack.push(Frame {
                    callee: head.callee,
                    receiver: head.receiver,
                    turbofish: head.turbofish,
                    active: 0,
                    started: false,
                });
            }
            TokenKind::LBracket | TokenKind::LBrace => stack.push(Frame {
                callee: None,
                receiver: None,
                turbofish: false,
                active: 0,
                started: false,
            }),
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                stack.pop();
            }
            TokenKind::Comma => {
                if let Some(frame) = stack.last_mut()
                    && frame.callee.is_some()
                {
                    frame.active += 1;
                }
            }
            _ => {}
        }
    }

    // The innermost enclosing call (nearest call frame from the top of the stack).
    stack.iter().rev().find_map(|frame| {
        frame.callee.clone().map(|callee| CallContext {
            callee,
            active: frame.active,
            receiver: frame.receiver,
            turbofish: frame.turbofish,
            // Commas separate arguments, so `n` commas means `n + 1` arguments — but only once one
            // exists at all. A comma is itself a token, so `started` is always true when `active`
            // is non-zero, and the sum is exact rather than a clamp.
            arity_so_far: frame.active + usize::from(frame.started),
        })
    })
}

/// What sits in front of a `(`: the callee, its receiver, and whether a turbofish came between.
struct CallHead {
    callee: Option<String>,
    receiver: Option<Span>,
    turbofish: bool,
}

/// Read the `receiver . callee ::<…>` head from the tokens before the `(` at `lparen`. `callee` is
/// `None` when the `(` opens plain grouping rather than a call.
///
/// Three shapes reach here, and the third is why this reads the token slice by index rather than a
/// fixed window of the preceding tokens:
///
/// - `callee(` — an ordinary call.
/// - `recv.callee(` — a method call; `recv`'s span goes back for type resolution.
/// - `callee::<T, U>(` — a turbofish head. Its length is unbounded (`construct::<Map<K, V>>(`), so
///   the `::` and the callee are found by scanning back from the closing `>` to its match. Without
///   this, every reflection intrinsic written in its turbofish form — and every explicitly
///   instantiated generic call — looked like plain grouping and got no signature help at all.
fn call_head(tokens: &[Token], lparen: usize, text: &str) -> CallHead {
    let nothing = CallHead {
        callee: None,
        receiver: None,
        turbofish: false,
    };
    let Some(mut head) = lparen.checked_sub(1) else {
        return nothing;
    };

    // Step back over a `::<…>` turbofish. The `::` is required: `a < b > (c)` also ends in `>`
    // just before a `(`, and it is a comparison, not a call head.
    let mut turbofish = false;
    if tokens[head].kind == TokenKind::Gt {
        let Some(open) = matching_lt(tokens, head) else {
            return nothing;
        };
        let Some(colons) = open.checked_sub(1) else {
            return nothing;
        };
        if tokens[colons].kind != TokenKind::ColonColon {
            return nothing;
        }
        let Some(before) = colons.checked_sub(1) else {
            return nothing;
        };
        head = before;
        turbofish = true;
    }

    let callee = callee_name(&tokens[head], text);
    let receiver = match (head.checked_sub(1), head.checked_sub(2)) {
        (Some(dot), Some(recv))
            if tokens[dot].kind == TokenKind::Dot && tokens[recv].kind == TokenKind::Ident =>
        {
            Some(tokens[recv].span)
        }
        _ => None,
    };
    CallHead {
        callee,
        receiver,
        turbofish,
    }
}

/// The callee a token can name: an identifier, or a **reflection primitive**.
///
/// The reflection primitives are the one keyword family that is a call head — `type_of(v)`,
/// `construct::<T>(fields)` — so `type_of(` lexes as `TypeOfKw LParen` and never as an identifier.
/// Which words those are, and how each is spelled, is asked of the lexer rather than restated:
/// [`TokenKind::reserved_word`] derives the spelling from the token's own `#[token("…")]`, and the
/// role filter keeps `while (x)` and `if (x)` from reading as calls.
fn callee_name(token: &Token, text: &str) -> Option<String> {
    match token.kind {
        TokenKind::Ident => Some(slice(text, token.span).to_string()),
        kind => kind
            .reserved_word()
            .filter(|word| word.role == noeta_lexer::ReservedRole::Reflection)
            .map(|word| word.word.to_string()),
    }
}

/// The index of the `<` that the `>` at `gt` closes, counting nesting (`::<Map<K, V>>`). `None` if
/// the scan leaves what a turbofish could possibly be — a bracket, a brace, or a statement end.
fn matching_lt(tokens: &[Token], gt: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = gt;
    loop {
        match tokens[i].kind {
            TokenKind::Gt => depth += 1,
            TokenKind::Lt => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            TokenKind::LParen
            | TokenKind::RParen
            | TokenKind::LBrace
            | TokenKind::RBrace
            | TokenKind::LBracket
            | TokenKind::RBracket
            | TokenKind::Semicolon => return None,
            _ => {}
        }
        i = i.checked_sub(1)?;
    }
}

/// The signature to show for a **reflection intrinsic** call — the thirteen reserved words that have
/// no [`FnDecl`] for [`from_decl`] to render, and so had no signature help at all.
///
/// Everything shown comes from [`noeta_builtins::REFLECTION_INTRINSICS`]: the form is chosen by what
/// the user has typed so far (whether a turbofish opened the call, and which argument the cursor is
/// in — the two `invoke` arities and the two `construct` surfaces are told apart by exactly that),
/// and its parameters and result are rendered from the entry. Nothing about the thirteen is spelled
/// out in this crate.
pub fn from_intrinsic(
    intrinsic: &noeta_builtins::ReflectionIntrinsic,
    call: &CallContext,
) -> SignatureData {
    // Which form: how many arguments have been written. Which parameter to highlight within it: the
    // one the cursor is in. The two are different questions — see `CallContext::arity_so_far`.
    let form = intrinsic.form_for(call.turbofish, call.arity_so_far);
    let parameters: Vec<String> = form
        .params
        .iter()
        .map(noeta_builtins::ReflectParam::render)
        .collect();
    SignatureData {
        label: format!("{} — {}", form.render(intrinsic.name), intrinsic.summary),
        active_param: call.active.min(parameters.len().saturating_sub(1)),
        parameters,
    }
}

fn slice(text: &str, span: Span) -> &str {
    &text[span.range()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    /// Resolve as the store does for a top-level function: find the call context, look up the fn.
    fn sig_at(src: &str, offset: u32) -> Option<SignatureData> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        let call = enclosing_call(&lexed.tokens, src, offset)?;
        let decl = program.stmts.iter().find_map(|stmt| match stmt {
            noeta_ast::Stmt::Fn(decl) if decl.name == call.callee => Some(decl),
            _ => None,
        })?;
        Some(from_decl(decl, call.active))
    }

    fn call_at(src: &str, offset: u32) -> Option<CallContext> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        enclosing_call(&lexed.tokens, src, offset)
    }

    #[test]
    fn shows_the_signature_and_first_parameter() {
        let src = "fn add(a: int, b: int): int { return a + b }\nx = add(";
        let offset = src.len() as u32; // just after the `(`
        let sig = sig_at(src, offset).expect("inside the call");
        assert_eq!(sig.label, "add(a: int, b: int) -> int");
        assert_eq!(sig.parameters, vec!["a: int", "b: int"]);
        assert_eq!(sig.active_param, 0);
    }

    #[test]
    fn tracks_the_active_parameter_across_commas() {
        let src = "fn add(a: int, b: int): int { return a + b }\nx = add(1, ";
        let offset = src.len() as u32; // after the first comma
        assert_eq!(sig_at(src, offset).unwrap().active_param, 1);
    }

    #[test]
    fn a_comma_inside_a_nested_argument_is_not_an_argument_separator() {
        let src = "fn f(xs: List<int>): int { return 1 }\ny = f([1, 2, ";
        let offset = src.len() as u32; // inside the list literal argument
        // Still the first (and only) parameter of `f`, despite the commas inside the list.
        assert_eq!(sig_at(src, offset).unwrap().active_param, 0);
    }

    #[test]
    fn reports_the_innermost_call() {
        let src = "fn outer(a: int): int { return 1 }\nfn inner(b: int): int { return 2 }\nz = outer(inner(";
        let offset = src.len() as u32;
        assert_eq!(sig_at(src, offset).unwrap().label, "inner(b: int) -> int");
    }

    #[test]
    fn outside_any_call_is_none() {
        let src = "fn f(a: int): int { return 1 }\nx = 1";
        assert!(sig_at(src, src.len() as u32).is_none());
    }

    #[test]
    fn a_call_to_an_unknown_function_is_none() {
        let src = "y = mystery(";
        assert!(sig_at(src, src.len() as u32).is_none());
    }

    #[test]
    fn a_method_call_reports_its_receiver() {
        // `c.get(` — the callee is `get`, and the receiver `c` is captured for type resolution.
        let src = "v = c.get(";
        let call = call_at(src, src.len() as u32).expect("inside a call");
        assert_eq!(call.callee, "get");
        let receiver = call.receiver.expect("method call has a receiver");
        assert_eq!(&src[receiver.range()], "c");
    }

    #[test]
    fn a_plain_call_has_no_receiver() {
        let src = "v = f(";
        assert!(call_at(src, src.len() as u32).unwrap().receiver.is_none());
    }

    /// **The comma count and the argument count are different numbers**, and the scan reports both.
    ///
    /// `f(` and `f(x` are both "argument 0" — that is what `active` means and it is the right answer
    /// for the highlight in each — but one call has no arguments and the other has one. Only
    /// `arity_so_far` separates them, and a caller choosing between an intrinsic's zero-operand and
    /// one-operand forms needs exactly that.
    #[test]
    fn the_argument_count_is_not_the_comma_count() {
        let counts = |src: &str| {
            let call = call_at(src, src.len() as u32).expect("inside a call");
            (call.active, call.arity_so_far)
        };
        assert_eq!(counts("v = f("), (0, 0), "nothing written");
        assert_eq!(counts("v = f(  "), (0, 0), "whitespace is not an argument");
        assert_eq!(counts("v = f(x"), (0, 1), "one argument begun");
        assert_eq!(counts("v = f(x,"), (1, 2), "a comma commits to a second");
        assert_eq!(counts("v = f(x, y"), (1, 2));
        assert_eq!(counts("v = f(x, y, z"), (2, 3));
        // A nested argument has begun the outer call's first argument, and its own commas belong to
        // the inner frame.
        assert_eq!(counts("v = f([1, 2"), (0, 1));
        // A turbofish head does not leak into the argument list.
        assert_eq!(counts("v = f::<int>("), (0, 0));
        assert_eq!(counts("v = f::<int>(x"), (0, 1));
    }
}
