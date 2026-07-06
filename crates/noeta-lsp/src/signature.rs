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

use noeta_ast::FnDecl;
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

/// The call the cursor is inside: the callee name, the 0-based active argument index, and — when the
/// call is a method call `recv.callee(` — the span of the receiver `recv` (so the caller can resolve
/// its type and find the method). `receiver` is `None` for a plain function call.
pub struct CallContext {
    pub callee: String,
    pub active: usize,
    pub receiver: Option<Span>,
}

/// One bracket frame on the scan stack. A call frame (a `(` opened right after an identifier) counts
/// its own top-level commas; a plain grouping/list/map frame does not, so a comma inside an argument
/// (`foo([a, b|])`) is not mistaken for an argument separator of the call.
struct Frame {
    callee: Option<String>,
    receiver: Option<Span>,
    active: usize,
}

/// Scan the tokens before `offset`, tracking bracket nesting, and return the innermost enclosing
/// *call* frame — the callee named just before an unclosed `(`, its receiver when the callee is
/// preceded by `.`, and the count of top-level commas inside it as the active-argument index.
pub fn enclosing_call(tokens: &[Token], text: &str, offset: u32) -> Option<CallContext> {
    let mut stack: Vec<Frame> = Vec::new();
    // The three tokens preceding the current one (`window[2]` is the most recent), for reading the
    // `receiver . callee (` head of a call.
    let mut window: [Option<(TokenKind, Span)>; 3] = [None; 3];

    for token in tokens.iter().take_while(|t| t.span.start < offset) {
        match token.kind {
            TokenKind::LParen => {
                let (callee, receiver) = call_head(&window, text);
                stack.push(Frame {
                    callee,
                    receiver,
                    active: 0,
                });
            }
            TokenKind::LBracket | TokenKind::LBrace => stack.push(Frame {
                callee: None,
                receiver: None,
                active: 0,
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
        window = [window[1], window[2], Some((token.kind, token.span))];
    }

    // The innermost enclosing call (nearest call frame from the top of the stack).
    stack.iter().rev().find_map(|frame| {
        frame.callee.clone().map(|callee| CallContext {
            callee,
            active: frame.active,
            receiver: frame.receiver,
        })
    })
}

/// Read the `receiver . callee` head from the tokens just before a `(`: the callee is the immediately
/// preceding identifier; the receiver is the identifier before a `.` (a method call). `(None, _)`
/// when the `(` does not follow an identifier — plain grouping, not a call.
fn call_head(
    window: &[Option<(TokenKind, Span)>; 3],
    text: &str,
) -> (Option<String>, Option<Span>) {
    let callee = match window[2] {
        Some((TokenKind::Ident, span)) => Some(slice(text, span).to_string()),
        _ => None,
    };
    let receiver = match (window[1], window[0]) {
        (Some((TokenKind::Dot, _)), Some((TokenKind::Ident, span))) => Some(span),
        _ => None,
    };
    (callee, receiver)
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
}
