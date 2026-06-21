//! The abstract syntax tree: pure data, no behavior.
//!
//! Every node carries a [`Span`] (for diagnostics and, later, the LSP). Behavior
//! lives in `lang-eval` (and, from M1, `lang-checker`/`lang-bytecode`), never here.
//! Surface sugar (`|>`, `~`, `?`, `?T`, ...) is kept as distinct nodes rather than
//! desugared in the parser, so later passes can produce precise diagnostics.
//!
//! M0 scope is intentionally tiny; this grows one vertical slice at a time.

use lang_span::Span;

mod pretty;
mod syntax_kind;

pub use pretty::Pretty;
pub use syntax_kind::SyntaxKind;

/// A whole program: a sequence of top-level statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `echo <expr>;` — writes the expression's display form to stdout.
    Echo { value: Expr, span: Span },
    /// A binding or reassignment: `name = expr;` or `mut name = expr;`.
    ///
    /// `mut_decl` records whether the `mut` keyword was present. Whether a bare
    /// `name = expr;` introduces a new immutable binding or reassigns an existing one
    /// is a runtime/semantic decision (see `lang-eval`), not a syntactic one.
    Binding {
        mut_decl: bool,
        name: String,
        name_span: Span,
        value: Expr,
        span: Span,
    },
    /// A named function declaration: `fn name(params): Ret { body }`.
    Fn(FnDecl),
    /// An enum declaration (plain, backed, or algebraic).
    Enum(EnumDecl),
    /// A structural record type alias: `type Item = { price: float, qty: int };`.
    Record(RecordDecl),
    /// A class declaration: `class Order { fields... methods... }`.
    Class(ClassDecl),
    /// `namespace App.Orders;` — declares the file's namespace. M0 records the path but
    /// otherwise treats it as a no-op (real module scoping is M1).
    Namespace { path: Vec<String>, span: Span },
    /// `use App.Models.User;` or `use App.Billing.{Invoice, Receipt};` — imports names.
    /// `path` is the dotted prefix; `names` are the imported leaf names.
    Use {
        path: Vec<String>,
        names: Vec<UseName>,
        span: Span,
    },
    /// `return <expr>;` or `return;`.
    Return { value: Option<Expr>, span: Span },
    /// `if cond { ... } else if cond { ... } else { ... }`. An `else if` is represented
    /// as an `else_body` containing a single nested `If`.
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
        span: Span,
    },
    /// `for <pattern> in <iterable> { ... }`.
    For {
        pattern: ForPattern,
        iterable: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// A bare expression used for its effect: `expr;`.
    Expr { expr: Expr, span: Span },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Echo { span, .. }
            | Stmt::Binding { span, .. }
            | Stmt::Namespace { span, .. }
            | Stmt::Use { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::If { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Expr { span, .. } => *span,
            Stmt::Fn(decl) => decl.span,
            Stmt::Enum(decl) => decl.span,
            Stmt::Record(decl) => decl.span,
            Stmt::Class(decl) => decl.span,
        }
    }
}

/// A structural record type alias (`type Item = { price: float, qty: int };`). A value
/// type with structural equality; all fields immutable. Constructed via the all-fields
/// literal (`Item { price: 9.99, qty: 2 }`).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordDecl {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<FieldDecl>,
    /// Leading `#[...]` attributes (e.g. `#[derive(Equatable)]`). Parsed and validated by the
    /// checker; the manifest/codegen they drive arrives with M1.8b.
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// An attribute in annotation position (`#[derive(Equatable, Clone)]`, `#[Route("/x")]`). The
/// surface is a name with optional identifier arguments. `derive` is interpreted by the checker
/// (and, later, the compiler); other attributes reduce to records in the manifest (M1.8b).
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub name_span: Span,
    /// The identifier arguments inside the parentheses, each with its span (e.g. the derived
    /// trait names in `#[derive(A, B)]`). Empty for a bare `#[Marker]`.
    pub args: Vec<(String, Span)>,
    pub span: Span,
}

/// An `impl Trait { ... }` block inside a class body. Implementing a built-in trait "lights up"
/// its operator or protocol (e.g. `impl Add` enables `+`). The block's methods are flattened into
/// [`ClassDecl::methods`] for execution; the block itself is retained here so the checker can
/// validate the trait name and its required method signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplBlock {
    pub trait_name: String,
    pub trait_span: Span,
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

/// A class declaration: fields declared in the body (immutable by default, `mut` opt-in)
/// plus methods and associated functions (`fn`). There is no special constructor — `new`
/// is just a conventional associated function returning the enclosing type.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<FieldDecl>,
    /// All callable methods, including the ones flattened out of `impl` blocks — so the existing
    /// `(type, method)` dispatch machinery resolves an operator's trait method with no change.
    pub methods: Vec<FnDecl>,
    /// The `impl Trait { ... }` blocks declared in the body. Their methods also appear in
    /// `methods`; these entries let the checker validate each trait and its required signatures.
    pub impls: Vec<ImplBlock>,
    /// Leading `#[...]` attributes on the class (e.g. `#[derive(Comparable)]`).
    pub attrs: Vec<Attribute>,
    /// The optional `destruct { ... }` block — the runtime-invoked destructor. It is *not* a
    /// method (no call site, not directly callable); the GC runs it when the last reference to
    /// an instance drops. Its statements run with the instance's fields in scope.
    pub destructor: Option<Vec<Stmt>>,
    pub span: Span,
}

/// One field of a record or class: a name, an optional `mut` marker (classes only), and
/// its declared type. The type is parsed but unchecked in M0.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: String,
    pub name_span: Span,
    /// Whether the field was declared `mut` (class fields only; always false for records).
    pub mut_field: bool,
    pub ty: Option<TypeRef>,
    pub span: Span,
}

/// An enum declaration. Plain (`enum Color { Red; ... }`), backed
/// (`enum Status: string { Pending = "pending"; ... }`), or algebraic
/// (`enum OrderError { Empty; NegativePrice(index: int); }`).
#[derive(Debug, Clone, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    /// The backing primitive type for a backed enum (`: string`), if any.
    pub backing: Option<TypeRef>,
    pub variants: Vec<VariantDecl>,
    /// Leading `#[...]` attributes on the enum.
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// One variant of an enum.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantDecl {
    pub name: String,
    pub name_span: Span,
    /// Associated data fields (algebraic variant); empty otherwise.
    pub fields: Vec<Param>,
    /// The backing value (`= "pending"`) for a backed enum's variant.
    pub backed_value: Option<Expr>,
    pub span: Span,
}

/// The binding form of a `for` loop: either one variable, or a `(index, value)` pair
/// (as produced by `.enumerate()`).
#[derive(Debug, Clone, PartialEq)]
pub enum ForPattern {
    Single {
        name: String,
        name_span: Span,
    },
    Pair {
        first: String,
        first_span: Span,
        second: String,
        second_span: Span,
    },
}

/// One imported leaf name in a `use` declaration, with its span for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct UseName {
    pub name: String,
    pub span: Span,
}

/// A named function declaration. Constructors are not special in this language — a
/// `fn` declaration just introduces a callable binding.
#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// The declared return type, if any. Parsed but not yet checked in M0.
    pub ret: Option<TypeRef>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A function parameter: a name and an optional type annotation (unchecked in M0).
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub name_span: Span,
    pub ty: Option<TypeRef>,
    pub span: Span,
}

/// A type reference in source (e.g. `int`, `List<Item>`, `Result<Order, OrderError>`,
/// `?User`). Parsed and retained for M1's type checker; M0 does not interpret it.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRef {
    /// A named type with optional generic arguments.
    Named {
        name: String,
        args: Vec<TypeRef>,
        span: Span,
    },
    /// `?T`, sugar for `Option<T>`. Kept as its own node (not desugared) so M1 can
    /// produce precise diagnostics on the nullability surface.
    Optional { inner: Box<TypeRef>, span: Span },
}

impl TypeRef {
    pub fn span(&self) -> Span {
        match self {
            TypeRef::Named { span, .. } | TypeRef::Optional { span, .. } => *span,
        }
    }
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A string literal with its already-unescaped value.
    Str { value: String, span: Span },
    /// An integer literal.
    Int { value: i64, span: Span },
    /// A floating-point literal.
    Float { value: f64, span: Span },
    /// A boolean literal.
    Bool { value: bool, span: Span },
    /// A reference to a binding.
    Ident { name: String, span: Span },
    /// A prefix unary operation.
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },
    /// An infix binary operation.
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// A call: `callee(args)`.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    /// An anonymous function (arrow closure): `fn(params) => body`.
    Closure {
        params: Vec<Param>,
        body: Box<Expr>,
        span: Span,
    },
    /// The pipeline operator: `left |> right`. Kept as its own node (not desugared in
    /// the parser) so diagnostics can point at the pipeline. `x |> f(a)` means
    /// `f(x, a)`; `x |> f` means `f(x)`.
    Pipeline {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// A list literal: `[a, b, c]`.
    List { items: Vec<Expr>, span: Span },
    /// A map literal: `{"a": 1, "b": 2}`.
    Map {
        entries: Vec<(Expr, Expr)>,
        span: Span,
    },
    /// Member access: `receiver.name`. When immediately called (`receiver.name(...)`)
    /// it is a method call; bare field access lands with records (Slice 6).
    Member {
        receiver: Box<Expr>,
        name: String,
        name_span: Span,
        span: Span,
    },
    /// An interpolated string: `"Hello {name}"` becomes a sequence of literal and
    /// embedded-expression parts. A string with no holes stays a plain [`Expr::Str`].
    Interp { parts: Vec<StrPart>, span: Span },
    /// `match scrutinee { pattern => body, ... }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// An all-fields object literal: `Order { id: 1, ..base }`. Constructs a record or
    /// class instance; the evaluator requires every declared field to be set.
    Object(ObjectLit),
    /// The `?` propagation operator: `expr?`. On `Ok(x)`/`some(x)` it yields `x`; on
    /// `Err(e)`/`none` it early-returns that value from the enclosing function. Kept as
    /// its own node (not desugared) so M1 diagnostics can point at the `?`.
    Try { expr: Box<Expr>, span: Span },
    /// The `??` fallback operator: `value ?? fallback`. On `Ok(x)`/`some(x)` it yields
    /// `x`; on `Err(_)`/`none` it evaluates and yields `fallback`.
    Coalesce {
        value: Box<Expr>,
        fallback: Box<Expr>,
        span: Span,
    },
}

/// An all-fields object literal. `spread` (`..expr`) supplies values for fields not named
/// explicitly, so the full-initialization guarantee still holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectLit {
    pub type_name: String,
    pub type_name_span: Span,
    pub fields: Vec<FieldInit>,
    pub spread: Option<Box<Expr>>,
    pub span: Span,
}

/// One `name: value` initializer in an object literal.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
    pub span: Span,
}

/// One arm of a `match`: a pattern and the expression it evaluates to.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

/// A `match` pattern. Exhaustiveness is unchecked in M0 (it is a checker concern, M1).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_` — matches anything, binds nothing.
    Wildcard {
        span: Span,
    },
    /// A lowercase name — matches anything and binds it.
    Binding {
        name: String,
        span: Span,
    },
    Int {
        value: i64,
        span: Span,
    },
    Str {
        value: String,
        span: Span,
    },
    Bool {
        value: bool,
        span: Span,
    },
    /// `Type.Variant`, `Variant(sub, ...)`, or `Type.Variant(sub, ...)`. `type_name`
    /// is `None` for unqualified constructors like `Ok(x)` / `some(x)`.
    Variant {
        type_name: Option<String>,
        variant: String,
        bindings: Vec<Pattern>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Binding { span, .. }
            | Pattern::Int { span, .. }
            | Pattern::Str { span, .. }
            | Pattern::Bool { span, .. }
            | Pattern::Variant { span, .. } => *span,
        }
    }
}

/// One part of an interpolated string.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// Literal text (already unescaped).
    Literal(String),
    /// An embedded `{expr}` hole.
    Hole(Expr),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Str { span, .. }
            | Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Closure { span, .. }
            | Expr::Pipeline { span, .. }
            | Expr::List { span, .. }
            | Expr::Map { span, .. }
            | Expr::Member { span, .. }
            | Expr::Interp { span, .. }
            | Expr::Match { span, .. }
            | Expr::Try { span, .. }
            | Expr::Coalesce { span, .. } => *span,
            Expr::Object(lit) => lit.span,
        }
    }
}

/// A prefix unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation, `-x`.
    Neg,
    /// Logical negation, `!x`.
    Not,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
        }
    }
}

/// An infix binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// String concatenation, `~`.
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Concat => "~",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }

    /// The trait method a user object's class implements to overload this operator, if the
    /// operator is overloadable. `a + b` dispatches to `a`'s `add(b)` when `a` is an object whose
    /// type defines that method; otherwise the built-in semantics apply. Comparisons, equality,
    /// and the logical operators are *not* overloadable here (their trait wiring — `Equatable`,
    /// `Comparable`, returning `bool`/`Ordering` — is M1.8b); they return `None`.
    pub fn overload_method(self) -> Option<&'static str> {
        match self {
            BinaryOp::Add => Some("add"),
            BinaryOp::Sub => Some("sub"),
            BinaryOp::Mul => Some("mul"),
            BinaryOp::Div => Some("div"),
            BinaryOp::Concat => Some("concat"),
            BinaryOp::Rem
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or => None,
        }
    }

    /// How `==`/`!=` overload through the `Equatable` trait. Both dispatch to the class's `eq`
    /// method (which returns `bool`); `==` uses the result as-is, `!=` negates it. The return is
    /// `Some(negate)` for the two equality operators and `None` for every other operator. (Unlike
    /// the arithmetic group in [`BinaryOp::overload_method`], whose method returns the result
    /// directly, `eq`'s result is post-processed — hence a separate accessor.)
    pub fn equatable_negation(self) -> Option<bool> {
        match self {
            BinaryOp::Eq => Some(false),
            BinaryOp::Ne => Some(true),
            _ => None,
        }
    }

    /// The `Comparable` method `< <= > >=` dispatch to: each calls the class's `compare` method
    /// (returning an `Ordering`) and maps the result to a bool. Returns `Some("compare")` for the
    /// four ordering comparisons and `None` for every other operator. The `Ordering` → bool
    /// mapping is operator-specific and applied after the call (see [`BinaryOp::ordering_satisfies`]).
    pub fn comparable_method(self) -> Option<&'static str> {
        matches!(
            self,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        )
        .then_some("compare")
    }

    /// Map an `Ordering` variant name (`"Less"`/`"Equal"`/`"Greater"`, as returned by a
    /// `compare` method) to this comparison operator's bool result. Defined here so both backends
    /// agree on the mapping; an unrecognized variant yields `false`.
    pub fn ordering_satisfies(self, ordering_variant: &str) -> bool {
        let less = ordering_variant == "Less";
        let equal = ordering_variant == "Equal";
        let greater = ordering_variant == "Greater";
        match self {
            BinaryOp::Lt => less,
            BinaryOp::Le => less || equal,
            BinaryOp::Gt => greater,
            BinaryOp::Ge => greater || equal,
            _ => false,
        }
    }
}

/// The three `Ordering` variant names a `compare` method returns. The built-in `Ordering` enum is
/// constructed on the fly by the `.compare()` primitive method and by `Comparable` dispatch; this
/// is the canonical spelling shared by both backends so their values display and match identically.
pub const ORDERING_VARIANTS: [&str; 3] = ["Less", "Equal", "Greater"];

/// The `Ordering` variant name for a `std::cmp::Ordering`. Keeps the primitive `.compare()` in
/// both backends mapping to the same surface variant.
pub fn ordering_variant(ordering: std::cmp::Ordering) -> &'static str {
    match ordering {
        std::cmp::Ordering::Less => "Less",
        std::cmp::Ordering::Equal => "Equal",
        std::cmp::Ordering::Greater => "Greater",
    }
}
