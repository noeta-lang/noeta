//! A byte-driven generator of syntactically valid Noeta programs.
//!
//! # Why generate structure rather than mutate bytes
//!
//! Byte-level mutation is the reflex for fuzzing, and for a language front-end it is close to
//! useless: essentially every mutated byte string fails to lex, the formatter declines it, and the
//! interesting invariants — idempotence, comment placement, layout preservation — are never
//! evaluated at all. The bugs worth finding live *past* the parser, so the generator's job is to
//! reach that far on nearly every input. [`parse_rate`](crate::parse_rate) measures whether it
//! still does, and a test asserts a floor, because a generator that silently rots into unparseable
//! output turns the whole suite green and vacuous.
//!
//! # The entropy contract
//!
//! Generation is driven by a plain `&[u8]`, consumed left to right, and is **total**: it never
//! fails and never blocks. When the bytes run out every subsequent choice reads as `0`, and every
//! choice list in this module is ordered so that `0` is the simplest, most terminal alternative
//! (a literal rather than a nested expression, no comment rather than a comment, end the block
//! rather than extend it). Exhaustion therefore *winds generation down* instead of truncating it
//! mid-token, which is what keeps the output parseable regardless of how few bytes were supplied.
//!
//! That contract is what lets one generator serve two drivers. `proptest` supplies `Vec<u8>` for
//! the deterministic, seeded, gate-safe suite; a libFuzzer target would hand over its mutated
//! buffer directly, and coverage-guided mutation of *these* bytes is mutation of the program's
//! shape rather than of its text. Nothing here depends on either driver.
//!
//! # What is deliberately varied
//!
//! The corpus is real code, and every corpus file has exactly one layout: the one its author
//! wrote. The formatter is source-directed by default (`wrap = false` preserves author line
//! breaks), so layout is a genuine input dimension that a corpus of any size barely samples. This
//! generator varies, independently of the program's structure: statement/expression nesting,
//! one-line versus exploded block bodies, blank-line runs, semicolon presence, header parentheses,
//! method-chain breaks, and comment placement at every nesting depth. Comments are numbered
//! (`// c7`) so a completeness or placement violation names exactly which one moved.

use std::fmt::Write as _;

/// A cursor over the driver's bytes, yielding bounded choices.
///
/// Reading past the end yields `0` forever — see the module docs for why that terminates
/// generation cleanly rather than truncating it.
struct Entropy<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Entropy<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Entropy { bytes, pos: 0 }
    }

    /// The next byte, or `0` once the buffer is spent.
    fn byte(&mut self) -> u8 {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos = self.pos.saturating_add(1);
        b
    }

    /// A choice in `0..n`, biased to `0` on exhaustion. `n == 0` is treated as `1`.
    fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        (self.byte() as usize) % n
    }

    /// True with probability roughly `percent/100`; false once the buffer is spent.
    fn chance(&mut self, percent: u8) -> bool {
        (self.byte() as u32) * 100 < (percent as u32) * 256
    }

    /// Whether the driver's bytes are spent, so generation should wind down.
    fn spent(&self) -> bool {
        self.pos >= self.bytes.len()
    }
}

/// Knobs on what the generator emits. The defaults are tuned for formatter fuzzing: comments dense
/// enough that most programs carry several at mixed depths, and layout noise on.
#[derive(Debug, Clone)]
pub struct GenOptions {
    /// Rough percentage chance of an own-line comment before any given statement.
    pub comment_density: u8,
    /// Rough percentage chance of a trailing comment after a statement.
    pub trailing_comment_density: u8,
    /// Whether to vary line breaks, blank lines, semicolons and header parens.
    pub layout_noise: bool,
    /// Maximum expression nesting depth.
    pub max_depth: u32,
    /// Node budget. Generation forces terminal choices once it is spent, bounding output size
    /// independently of how many bytes the driver supplied.
    pub budget: u32,
}

impl Default for GenOptions {
    fn default() -> Self {
        GenOptions {
            comment_density: 30,
            trailing_comment_density: 12,
            layout_noise: true,
            max_depth: 4,
            budget: 220,
        }
    }
}

/// Generate a syntactically valid Noeta program from `bytes` with the default options.
pub fn program(bytes: &[u8]) -> String {
    program_with(bytes, &GenOptions::default())
}

/// Generate a syntactically valid Noeta program from `bytes`.
pub fn program_with(bytes: &[u8], opts: &GenOptions) -> String {
    let mut g = Gen {
        e: Entropy::new(bytes),
        out: String::new(),
        indent: 0,
        depth: 0,
        budget: opts.budget,
        comment_no: 0,
        opts: opts.clone(),
        in_fn: false,
        in_loop: false,
        inline_only: false,
    };
    g.program();
    g.out
}

/// Identifiers safe to reuse anywhere: none is a keyword, and shadowing is a checker rule rather
/// than a parse rule, so reuse costs nothing here.
const IDENTS: &[&str] = &[
    "a", "b", "n", "xs", "item", "total", "acc", "value", "key", "left", "right", "count", "buf",
];

/// Type names for declarations and annotations.
const TYPE_NAMES: &[&str] = &["Alpha", "Beta", "Gamma", "Delta"];

/// Type references usable in an annotation or signature position.
const TYPES: &[&str] = &[
    "int",
    "string",
    "bool",
    "float",
    "void",
    "List<int>",
    "Map<string, int>",
    "?int",
    "Result<int, string>",
    "(int, int)",
];

/// Function names.
const FN_NAMES: &[&str] = &["step", "run", "pick", "fold", "emit", "check"];

/// Method names — reads plausibly as a chain, and none needs to exist for the source to parse.
const METHODS: &[&str] = &[
    "len", "upper", "trim", "reverse", "sum", "keys", "iter", "collect", "clone",
];

/// Method names taking one closure argument.
const HOF_METHODS: &[&str] = &["map", "filter", "each", "take_while"];

struct Gen<'a> {
    e: Entropy<'a>,
    out: String,
    indent: usize,
    depth: u32,
    budget: u32,
    comment_no: u32,
    opts: GenOptions,
    in_fn: bool,
    in_loop: bool,
    /// Inside a block collapsed onto one line, where a `//` comment would run to end of line and
    /// swallow the closing `}`. Every comment emitted under this flag uses the `/* … */` form.
    inline_only: bool,
}

impl Gen<'_> {
    // ---- output plumbing -------------------------------------------------------------------

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// Start a fresh line at the current indent.
    fn newline(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    /// Consume one unit of the node budget. `false` once it is spent, which every recursive site
    /// reads as "emit a leaf".
    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    /// Whether recursion may go deeper: budget left, depth headroom, and bytes remaining.
    fn may_nest(&self) -> bool {
        self.budget > 0 && self.depth < self.opts.max_depth && !self.e.spent()
    }

    fn ident(&mut self) -> &'static str {
        IDENTS[self.e.below(IDENTS.len())]
    }

    fn type_name(&mut self) -> &'static str {
        TYPE_NAMES[self.e.below(TYPE_NAMES.len())]
    }

    fn type_ref(&mut self) -> &'static str {
        TYPES[self.e.below(TYPES.len())]
    }

    // ---- comments --------------------------------------------------------------------------

    /// Emit an own-line comment at the current indent, if the dice say so. Numbered, so a
    /// violation report identifies exactly which comment moved or vanished.
    fn maybe_own_line_comment(&mut self) {
        if !self.e.chance(self.opts.comment_density) {
            return;
        }
        self.comment_no += 1;
        let n = self.comment_no;
        match if self.inline_only { 1 } else { self.e.below(3) } {
            0 => {
                let _ = write!(self.out, "// c{n}");
            }
            1 => {
                let _ = write!(self.out, "/* c{n} */");
            }
            _ => {
                let _ = write!(self.out, "/// c{n}");
            }
        }
        self.newline();
    }

    /// Emit a comment trailing the just-written code, if the dice say so.
    fn maybe_trailing_comment(&mut self) {
        if !self.e.chance(self.opts.trailing_comment_density) {
            return;
        }
        self.comment_no += 1;
        let n = self.comment_no;
        // A `///` doc comment is not valid trailing a statement, so only the two ordinary forms.
        if self.e.chance(50) && !self.inline_only {
            let _ = write!(self.out, " // c{n}");
        } else {
            let _ = write!(self.out, " /* c{n} */");
        }
    }

    // ---- program and blocks ----------------------------------------------------------------

    fn program(&mut self) {
        // A leading run of imports, so `sort_imports` has something to sort.
        let imports = self.e.below(4);
        for _ in 0..imports {
            self.maybe_own_line_comment();
            self.use_stmt();
            self.newline();
        }
        if imports > 0 {
            self.newline();
        }

        let mut stmts = 1 + self.e.below(6);
        while stmts > 0 && self.spend() {
            self.maybe_own_line_comment();
            self.top_level_item();
            self.newline();
            if self.opts.layout_noise && self.e.chance(30) {
                self.newline();
            }
            stmts -= 1;
        }
        // A trailing own-line comment at EOF: the `<eof>` anchor case.
        self.maybe_own_line_comment();
    }

    fn use_stmt(&mut self) {
        match self.e.below(3) {
            0 => self.push("use std.math"),
            1 => self.push("use std.{math, json}"),
            _ => self.push("use std.io.print"),
        }
        self.maybe_semicolon();
        self.maybe_trailing_comment();
    }

    /// A top-level item: a declaration, or any statement.
    fn top_level_item(&mut self) {
        match self.e.below(6) {
            0 => self.fn_decl(),
            1 => self.struct_decl(),
            2 => self.class_decl(),
            3 => self.enum_decl(),
            _ => self.stmt(),
        }
    }

    /// A braced block of statements at the current indent. `one_line` collapses it onto a single
    /// line when the contents allow — the layout the formatter must either preserve or explode
    /// consistently.
    fn block(&mut self) {
        let one_line = self.opts.layout_noise && self.budget > 0 && self.e.chance(20);
        if one_line {
            self.push("{ ");
            // A one-line body holds exactly one simple statement; anything with its own block
            // would need line breaks anyway. `inline_only` is sticky for the whole subtree: a
            // nested construct could otherwise emit a `//` comment that runs past the closing `}`.
            let was_inline = self.inline_only;
            self.inline_only = true;
            self.simple_stmt();
            self.inline_only = was_inline;
            self.push(" }");
            return;
        }
        self.push("{");
        self.indent += 1;
        let mut stmts = self.e.below(4);
        if stmts == 0 && self.e.chance(60) {
            stmts = 1;
        }
        for _ in 0..stmts {
            if !self.spend() {
                break;
            }
            self.newline();
            self.maybe_own_line_comment();
            self.stmt();
            if self.opts.layout_noise && self.e.chance(15) {
                self.newline();
            }
        }
        // A comment as the last thing in a block: the "closing brace" anchor.
        if self.e.chance(self.opts.comment_density / 2) {
            self.newline();
            self.comment_no += 1;
            let n = self.comment_no;
            if self.inline_only {
                let _ = write!(self.out, "/* c{n} */");
            } else {
                let _ = write!(self.out, "// c{n}");
            }
        }
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    // ---- declarations ----------------------------------------------------------------------

    fn maybe_attribute(&mut self) {
        if !self.e.chance(20) {
            return;
        }
        match self.e.below(3) {
            0 => self.push("@derive(Equatable)"),
            1 => self.push("@derive(Equatable, Comparable, Display)"),
            _ => self.push("@derive(Display)"),
        }
        self.newline();
    }

    /// A named function. Deliberately carries no attribute: `@derive` is a type-level directive and
    /// the parser rejects it on a `fn`, so emitting one here would only manufacture parse failures.
    fn fn_decl(&mut self) {
        let name = FN_NAMES[self.e.below(FN_NAMES.len())];
        let _ = write!(self.out, "fn {name}(");
        let params = self.e.below(3);
        for i in 0..params {
            if i > 0 {
                self.push(", ");
            }
            let p = self.ident();
            let t = self.type_ref();
            let _ = write!(self.out, "{p}: {t}");
            // A trailing default.
            if i + 1 == params && self.e.chance(25) {
                self.push(" = ");
                self.default_for(t);
            }
        }
        let ret = self.type_ref();
        let _ = write!(self.out, "): {ret} ");
        let (was_fn, was_loop) = (self.in_fn, self.in_loop);
        self.in_fn = true;
        self.in_loop = false;
        self.block();
        self.in_fn = was_fn;
        self.in_loop = was_loop;
    }

    /// A literal of the given declared type, so defaults and returns stay plausible.
    fn default_for(&mut self, ty: &str) {
        match ty {
            "int" => self.push("0"),
            "string" => self.push("\"\""),
            "bool" => self.push("true"),
            "float" => self.push("0.0"),
            "List<int>" => self.push("[]"),
            "Map<string, int>" => self.push("{}"),
            "?int" => self.push("none"),
            "Result<int, string>" => self.push("Ok(0)"),
            "(int, int)" => self.push("(0, 0)"),
            _ => self.push("0"),
        }
    }

    fn struct_decl(&mut self) {
        self.maybe_attribute();
        let name = self.type_name();
        let _ = write!(self.out, "struct {name} {{");
        self.indent += 1;
        let fields = 1 + self.e.below(3);
        for _ in 0..fields {
            self.newline();
            self.maybe_own_line_comment();
            let vis = if self.e.chance(50) { "pub " } else { "" };
            let f = self.ident();
            let t = self.type_ref();
            let _ = write!(self.out, "{vis}{f}: {t}");
            self.maybe_trailing_comment();
        }
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    fn class_decl(&mut self) {
        self.maybe_attribute();
        let name = self.type_name();
        let _ = write!(self.out, "class {name} {{");
        self.indent += 1;
        let fields = 1 + self.e.below(3);
        for _ in 0..fields {
            self.newline();
            self.maybe_own_line_comment();
            let vis = if self.e.chance(60) { "pub " } else { "" };
            let mutable = if self.e.chance(30) { "mut " } else { "" };
            let f = self.ident();
            let t = self.type_ref();
            let _ = write!(self.out, "{vis}{mutable}{f}: {t}");
            self.maybe_trailing_comment();
        }
        // A method or two, so comments can sit inside a nested body.
        let methods = self.e.below(2);
        for _ in 0..methods {
            if !self.spend() {
                break;
            }
            self.newline();
            self.newline();
            self.maybe_own_line_comment();
            self.fn_decl();
        }
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    fn enum_decl(&mut self) {
        self.maybe_attribute();
        let name = self.type_name();
        let _ = write!(self.out, "enum {name} {{");
        self.indent += 1;
        let variants = 1 + self.e.below(3);
        for i in 0..variants {
            self.newline();
            self.maybe_own_line_comment();
            // Variant names are fixed so payload-carrying and bare forms both appear.
            let v = ["First", "Second", "Third", "Fourth"][i % 4];
            self.push(v);
            if self.e.chance(25) {
                let p = self.ident();
                let t = self.type_ref();
                let _ = write!(self.out, "({p}: {t})");
            }
            self.maybe_trailing_comment();
        }
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    // ---- statements ------------------------------------------------------------------------

    fn stmt(&mut self) {
        if !self.spend() {
            self.simple_stmt();
            return;
        }
        let choice = self.e.below(10);
        match choice {
            0..=2 => self.simple_stmt(),
            3 => self.if_stmt(),
            4 => self.while_stmt(),
            5 => self.for_stmt(),
            6 if self.in_fn => self.return_stmt(),
            7 if self.in_loop => {
                if self.e.chance(50) {
                    self.push("break");
                } else {
                    self.push("continue");
                }
                self.maybe_semicolon();
                self.maybe_trailing_comment();
            }
            8 => self.fn_decl(),
            _ => self.simple_stmt(),
        }
    }

    /// A statement with no block of its own — safe to place on a collapsed one-line body.
    fn simple_stmt(&mut self) {
        match self.e.below(4) {
            0 => {
                // A binding, optionally `mut` and optionally annotated.
                if self.e.chance(30) {
                    self.push("mut ");
                }
                let name = self.ident();
                self.push(name);
                if self.e.chance(25) {
                    let t = self.type_ref();
                    let _ = write!(self.out, ": {t}");
                }
                self.push(" = ");
                // A binding's right-hand side is the one position where a bare `{…}` map literal is
                // unambiguous, so it is the only place the unparenthesized form is emitted.
                if self.e.chance(12) {
                    self.map_lit();
                } else {
                    self.expr();
                }
            }
            1 => {
                self.push("echo ");
                self.expr();
            }
            2 => {
                // A compound assignment — the resugaring path (`x = x + v` → `x += v`).
                let name = self.ident();
                let op = ["+=", "-=", "*=", "??="][self.e.below(4)];
                let _ = write!(self.out, "{name} {op} ");
                self.expr();
            }
            _ => self.expr_stmt(),
        }
        self.maybe_semicolon();
        self.maybe_trailing_comment();
    }

    /// An expression statement. Never starts with `{`, which would parse as a block.
    fn expr_stmt(&mut self) {
        let name = self.ident();
        let m = METHODS[self.e.below(METHODS.len())];
        let _ = write!(self.out, "{name}.{m}()");
    }

    fn maybe_semicolon(&mut self) {
        if self.opts.layout_noise && self.e.chance(45) {
            self.push(";");
        }
    }

    fn if_stmt(&mut self) {
        self.push("if ");
        self.header_cond();
        self.push(" ");
        self.block();
        if self.e.chance(35) {
            self.push(" else ");
            if self.e.chance(40) {
                self.push("if ");
                self.header_cond();
                self.push(" ");
            }
            self.block();
        }
    }

    /// A condition, sometimes parenthesized — the `ParenStyle` policy's input.
    fn header_cond(&mut self) {
        let parens = self.opts.layout_noise && self.e.chance(25);
        if parens {
            self.push("(");
        }
        self.depth += 1;
        self.condition();
        self.depth -= 1;
        if parens {
            self.push(")");
        }
    }

    fn condition(&mut self) {
        let a = self.ident();
        let op = ["==", "!=", "<", "<=", ">", ">="][self.e.below(6)];
        let _ = write!(self.out, "{a} {op} ");
        self.atom();
    }

    fn while_stmt(&mut self) {
        self.push("while ");
        self.header_cond();
        self.push(" ");
        let was = self.in_loop;
        self.in_loop = true;
        self.block();
        self.in_loop = was;
    }

    fn for_stmt(&mut self) {
        self.push("for ");
        if self.e.chance(25) {
            let a = self.ident();
            let b = self.ident();
            let _ = write!(self.out, "({a}, {b})");
        } else {
            let v = self.ident();
            self.push(v);
        }
        self.push(" in ");
        let parens = self.opts.layout_noise && self.e.chance(20);
        if parens {
            self.push("(");
        }
        match self.e.below(3) {
            0 => self.push("0..5"),
            1 => self.push("[1, 2, 3]"),
            _ => {
                let v = self.ident();
                self.push(v);
            }
        }
        if parens {
            self.push(")");
        }
        self.push(" ");
        let was = self.in_loop;
        self.in_loop = true;
        self.block();
        self.in_loop = was;
    }

    fn return_stmt(&mut self) {
        self.push("return");
        if self.e.chance(80) {
            self.push(" ");
            self.expr();
        }
        self.maybe_semicolon();
        self.maybe_trailing_comment();
    }

    // ---- expressions -----------------------------------------------------------------------

    fn expr(&mut self) {
        if !self.may_nest() {
            self.atom();
            return;
        }
        self.depth += 1;
        self.budget = self.budget.saturating_sub(1);
        match self.e.below(12) {
            0..=1 => self.atom(),
            2 => self.binary(),
            3 => self.call(),
            4 => self.method_chain(),
            5 => self.list_lit(),
            6 => self.closure(),
            7 => self.interp_str(),
            8 => self.if_then_else(),
            9 => self.match_expr(),
            10 => self.map_or_set_lit(),
            _ => self.postfix(),
        }
        self.depth -= 1;
    }

    /// A leaf: literal or identifier. Always terminal.
    fn atom(&mut self) {
        match self.e.below(10) {
            0 => {
                let n = self.e.below(100);
                let _ = write!(self.out, "{n}");
            }
            1 => self.push("\"text\""),
            2 => self.push("true"),
            3 => self.push("false"),
            4 => self.push("1.5"),
            5 => self.push("none"),
            6 => self.push("1_000"),
            7 => self.push("0xFF"),
            8 => self.push("'raw ${not interpolated}'"),
            _ => {
                let v = self.ident();
                self.push(v);
            }
        }
    }

    fn binary(&mut self) {
        // Precedence mixing is the point: the printer must re-parenthesize correctly, and the
        // safety gate catches it when it does not.
        // Logical conjunction is `&&`/`||`; `and`/`or` are ordinary identifiers here, so emitting
        // them produces a parse error rather than an interesting input.
        let op = [
            "+", "-", "*", "/", "%", "~", "==", "!=", "<", ">", "&&", "||", "&", "|", "^", "<<",
            ">>", "===", "!==",
        ][self.e.below(19)];
        let parens = self.e.chance(30);
        if parens {
            self.push("(");
        }
        self.expr();
        let _ = write!(self.out, " {op} ");
        self.expr();
        if parens {
            self.push(")");
        }
    }

    fn call(&mut self) {
        let name = FN_NAMES[self.e.below(FN_NAMES.len())];
        let _ = write!(self.out, "{name}(");
        let args = self.e.below(3);
        for i in 0..args {
            if i > 0 {
                self.push(", ");
            }
            // A named argument sometimes — labels bind, so the printer must keep them.
            if self.e.chance(25) {
                let label = self.ident();
                let _ = write!(self.out, "{label}: ");
            }
            self.expr();
        }
        self.push(")");
    }

    /// A method chain, sometimes broken across lines with a leading `.` — the layout that hid a
    /// comment-placement defect (a comment between two links emitted after the whole statement).
    fn method_chain(&mut self) {
        let base = self.ident();
        self.push(base);
        let links = 1 + self.e.below(3);
        let broken = self.opts.layout_noise && links > 1 && self.e.chance(40);
        if broken {
            self.indent += 1;
        }
        for _ in 0..links {
            if broken {
                self.newline();
                // A comment inside a broken chain.
                if self.e.chance(self.opts.comment_density / 2) {
                    self.comment_no += 1;
                    let n = self.comment_no;
                    if self.inline_only {
                        let _ = write!(self.out, "/* c{n} */");
                    } else {
                        let _ = write!(self.out, "// c{n}");
                    }
                    self.newline();
                }
            }
            if self.e.chance(40) && self.may_nest() {
                let m = HOF_METHODS[self.e.below(HOF_METHODS.len())];
                let _ = write!(self.out, ".{m}(");
                self.closure();
                self.push(")");
            } else {
                let m = METHODS[self.e.below(METHODS.len())];
                let _ = write!(self.out, ".{m}()");
            }
        }
        if broken {
            self.indent -= 1;
        }
    }

    fn list_lit(&mut self) {
        self.push("[");
        let items = self.e.below(4);
        for i in 0..items {
            if i > 0 {
                self.push(", ");
            }
            if self.e.chance(15) {
                self.push("...");
                self.atom();
            } else {
                self.expr();
            }
        }
        self.push("]");
    }

    /// A map or set literal.
    ///
    /// The map form is **parenthesized** here. A leading `{` is ambiguous with a block in several
    /// positions the generator can reach — after `else` in an `if … then … else`, at the head of a
    /// statement — and the parser resolves it as a block, so a bare map literal in those slots is a
    /// generator bug rather than an interesting input. `simple_stmt` emits the bare form in the one
    /// place it is unambiguous (a binding's right-hand side). Set literals need no such care: `#{`
    /// cannot begin a block.
    fn map_or_set_lit(&mut self) {
        if self.e.chance(50) {
            self.push("(");
            self.map_lit();
            self.push(")");
        } else {
            self.push("#{");
            let items = self.e.below(3);
            for i in 0..items {
                if i > 0 {
                    self.push(", ");
                }
                self.atom();
            }
            self.push("}");
        }
    }

    /// A bare `{"k": v, …}` map literal, with no disambiguating parentheses. Only valid where a
    /// leading `{` cannot be read as a block — see [`Gen::map_or_set_lit`].
    fn map_lit(&mut self) {
        self.push("{");
        let items = self.e.below(3);
        for i in 0..items {
            if i > 0 {
                self.push(", ");
            }
            let _ = write!(self.out, "\"k{i}\": ");
            self.expr();
        }
        self.push("}");
    }

    fn closure(&mut self) {
        let p = self.ident();
        let _ = write!(self.out, "fn({p}) ");
        if self.e.chance(60) {
            self.push("=> ");
            self.expr();
        } else {
            let (was_fn, was_loop) = (self.in_fn, self.in_loop);
            self.in_fn = true;
            self.in_loop = false;
            self.block();
            self.in_fn = was_fn;
            self.in_loop = was_loop;
        }
    }

    /// An interpolated string. The holes are real expressions, so the printer's hole handling is
    /// exercised — and a hole's contents must survive verbatim through a reformat.
    fn interp_str(&mut self) {
        self.push("\"prefix ");
        let holes = 1 + self.e.below(2);
        for i in 0..holes {
            if i > 0 {
                self.push(" and ");
            }
            self.push("${");
            // Holes nest a full expression, but keep them shallow: a hole containing a block
            // would be legal and unreadable, and adds nothing the outer expression does not.
            let saved = self.opts.max_depth;
            self.opts.max_depth = self.depth + 1;
            self.expr();
            self.opts.max_depth = saved;
            self.push("}");
        }
        self.push(" suffix\"");
    }

    fn if_then_else(&mut self) {
        self.push("if ");
        self.condition();
        self.push(" then ");
        self.expr();
        self.push(" else ");
        self.expr();
    }

    fn match_expr(&mut self) {
        let subject = self.ident();
        let _ = write!(self.out, "match {subject} {{");
        self.indent += 1;
        let arms = 1 + self.e.below(3);
        for i in 0..arms {
            self.newline();
            self.maybe_own_line_comment();
            self.pattern(i);
            self.push(" => ");
            self.expr();
            self.push(",");
            self.maybe_trailing_comment();
        }
        // A catch-all keeps the match plausible; exhaustiveness is a checker rule, not a parse one,
        // but a `_` arm costs nothing and exercises the wildcard pattern.
        self.newline();
        self.push("_ => ");
        self.atom();
        self.push(",");
        self.indent -= 1;
        self.newline();
        self.push("}");
    }

    fn pattern(&mut self, i: usize) {
        match self.e.below(5) {
            0 => {
                let n = self.e.below(10);
                let _ = write!(self.out, "{n}");
            }
            1 => self.push("\"lit\""),
            2 => {
                let v = ["First", "Second", "Third", "Fourth"][i % 4];
                let t = self.type_name();
                let _ = write!(self.out, "{t}.{v}");
            }
            3 => {
                let a = self.ident();
                let b = self.ident();
                let _ = write!(self.out, "({a}, {b})");
            }
            _ => {
                let v = self.ident();
                self.push(v);
            }
        }
    }

    /// A postfix chain: `?` propagation, `??` coalescing, indexing, member access, tuple
    /// projection, the checked narrowing `.as<T>()` and the type test `is T`.
    ///
    /// Note `.as<T>()`, not `x as T`: `as` is an infix operator only inside `use … as …`, and the
    /// cast is spelled as a dot-keyword postfix. Getting that wrong cost about a sixth of the
    /// generator's parse rate before it was noticed, which is the argument for
    /// [`crate::parse_rate`] existing at all.
    fn postfix(&mut self) {
        let base = self.ident();
        self.push(base);
        match self.e.below(7) {
            0 => self.push("?"),
            1 => {
                self.push(" ?? ");
                self.atom();
            }
            2 => {
                self.push("[");
                self.atom();
                self.push("]");
            }
            3 => {
                let f = self.ident();
                let _ = write!(self.out, ".{f}");
            }
            4 => {
                let n = self.e.below(3);
                let _ = write!(self.out, ".{n}");
            }
            5 => {
                let t = ["int", "float", "string", "List<int>"][self.e.below(4)];
                let _ = write!(self.out, ".as<{t}>()");
            }
            _ => {
                let t = ["int", "float", "string", "List<int>"][self.e.below(4)];
                let _ = write!(self.out, " is {t}");
            }
        }
    }
}
