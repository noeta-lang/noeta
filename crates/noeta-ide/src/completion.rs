//! Completion candidates for `textDocument/completion` (slice **L5**).
//!
//! Three forms, chosen by the caller from the cursor context:
//!
//! - [`complete`] — **identifier completion** (C1): the language keywords, the top-level
//!   declarations (functions and types), and the value bindings in scope at the cursor (locals,
//!   parameters, `for`/`match`/closure bindings, and earlier module-level bindings). In-scope
//!   bindings come from [`resolve::visible_at`], so the same scoping walk backs completion and
//!   go-to-definition.
//! - [`members_of`] — **member completion** (C2): the fields, enum variants, and methods of a named
//!   type, offered on a `receiver.member` access once the caller has resolved the receiver's type.
//! - [`directives`] — **directive completion** (C4): the decorator directives and the tier
//!   name-space, offered right after an `@` (detected textually via [`is_directive_position`] —
//!   a dangling `@` never parses).
//! - [`directive_arg_candidates`] — **directive-argument completion** (C5): the vocabulary inside
//!   a directive's parens (`@derive(` → the derivable traits, `@role(` → the semantic enums,
//!   `@packed(` → the `Layout` variants, `@bench(` → its config knobs), from the same sources the
//!   parser/checker validate against. Context via [`directive_arg_context`].
//!
//! A best-effort read of the mid-edit AST — it leans on the recovering parser and the client's own
//! prefix filtering rather than requiring a clean parse. Both return backend-neutral [`Candidate`]s
//! (label + kind + optional detail) that the server maps to LSP `CompletionItem`s.

use std::collections::HashSet;

use noeta_ast::{FnDecl, Program, Stmt, TypeRef};
use noeta_span::{SourceId, Span};

use crate::resolve;
use crate::symbols;

/// What a completion candidate is, for the client's icon and the server's `CompletionItemKind`
/// mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Keyword,
    Function,
    Struct,
    Class,
    Enum,
    /// A local, parameter, or other in-scope value binding.
    Variable,
    /// A struct/class field (member completion after `.`).
    Field,
    /// A method (member completion after `.`).
    Method,
    /// An enum variant (member completion after `.`).
    EnumMember,
    /// A built-in type name (`int`, `List`, …) offered in a type-annotation position.
    Type,
    /// A user-defined `trait` (L1) — offered as a bound and after `dyn`.
    Trait,
    /// A native module or namespace-group member (`http.client`, member completion on a group).
    Module,
    /// An `@`-directive: a decorator (`@derive`, `@packed`, …) or a dev-tier (`@test`, `@doc`,
    /// a declared `@tier`'s name) — offered right after an `@` (C4).
    Directive,
}

/// One completion candidate: the inserted/filtered text, its kind, and an optional short detail.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub label: String,
    pub kind: CandidateKind,
    pub detail: Option<String>,
}

/// The Noeta surface keywords offered everywhere. Deliberately the *statement/expression* keywords a
/// developer types; the reflection intrinsics (`type_of`, `attributes_of`, …) are omitted as niche.
const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "class",
    "concurrent",
    "continue",
    "destruct",
    "echo",
    "else",
    "enum",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "is",
    "match",
    "mut",
    "namespace",
    "pub",
    "return",
    "spawn",
    "struct",
    "then",
    "trait",
    "true",
    "type",
    "use",
    "while",
    "yield",
];

/// The completion candidates at `offset` in file `source` of `program`: the top-level declarations
/// (with their precise kinds), then the value bindings in scope at the cursor, then the keywords.
/// De-duplicated by label, keeping the earliest — the scoping walk also binds a top-level function's
/// name into the module scope, so listing the declarations first is what stamps `greet` as a
/// `Function` rather than a bare `Variable`.
pub fn complete(program: &Program, offset: u32, source: SourceId) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    // Top-level declarations, with their precise kinds (a name usable as a call, constructor, or
    // type reference).
    for stmt in &program.stmts {
        let (name, kind) = match stmt {
            Stmt::Fn(decl) => (&decl.name, CandidateKind::Function),
            Stmt::Struct(decl) => (&decl.name, CandidateKind::Struct),
            Stmt::Class(decl) => (&decl.name, CandidateKind::Class),
            Stmt::Enum(decl) => (&decl.name, CandidateKind::Enum),
            Stmt::Trait(decl) => (&decl.name, CandidateKind::Trait),
            _ => continue,
        };
        candidates.push(Candidate {
            label: name.clone(),
            kind,
            detail: None,
        });
    }

    // In-scope value bindings (locals, parameters, loop/match/closure bindings, earlier module-level
    // bindings) — the names relevant where the cursor is. A top-level function's name is also here
    // (bound into the module scope) but was already emitted above with its precise kind, so dedup
    // drops it; a genuine local keeps its `Variable` kind.
    for (name, _span) in resolve::visible_at(program, offset, source) {
        candidates.push(Candidate {
            label: name,
            kind: CandidateKind::Variable,
            detail: None,
        });
    }

    // Keywords last: a same-spelled user name (rare) is the more useful suggestion.
    for keyword in KEYWORDS {
        candidates.push(Candidate {
            label: (*keyword).to_string(),
            kind: CandidateKind::Keyword,
            detail: None,
        });
    }

    dedupe_by_label(candidates)
}

/// The namespace-group bindings a program introduces: local (or aliased) name → root-qualified
/// prefix, from each `use` that binds a group (`use std.http` → `http` → `std.http`, `use std.http
/// as h` → `h` → `std.http`). What member completion consults to recognize a group receiver.
pub fn namespace_bindings(program: &Program) -> std::collections::HashMap<String, String> {
    use noeta_stdlib::registry::UseKind;
    let reg = noeta_stdlib::registry::single_registry_process();
    let mut map = std::collections::HashMap::new();
    for stmt in &program.stmts {
        if let Stmt::Use { path, names, .. } = stmt {
            for n in names {
                if let UseKind::Namespace(prefix) = reg.classify_use(path, &n.name) {
                    map.insert(n.local().to_string(), prefix);
                }
            }
        }
    }
    map
}

/// The member candidates of a namespace group whose root-qualified prefix is `prefix` (`std.http` →
/// `client`, `server`, `Response`) — its submodules and types one hop down, for completion after
/// `http.`. A submodule/sub-namespace child is a `Module`; an extension type is a `Type`.
pub fn namespace_members(prefix: &str) -> Vec<Candidate> {
    use noeta_stdlib::registry::NsChild;
    let reg = noeta_stdlib::registry::single_registry_process();
    reg.namespace_children(prefix)
        .into_iter()
        .map(|name| {
            let kind = match reg.resolve_namespace_child(prefix, &name) {
                NsChild::Type(_) => CandidateKind::Type,
                _ => CandidateKind::Module,
            };
            Candidate {
                label: name,
                kind,
                detail: None,
            }
        })
        .collect()
}

/// The short usage detail shown beside a built-in decorator directive — the closed set the
/// statement grammar dispatches on ([`noeta_parser::DECORATOR_DIRECTIVES`]).
fn decorator_detail(name: &str) -> &'static str {
    match name {
        "derive" => "@derive(Trait, …) — derive implementations for a type",
        "attribute" => "@attribute(…) — declare this struct as a data attribute",
        "role" => "@role(Enum.Variant, …) — tag an attribute/trait with architectural roles",
        "semantic" => "@semantic — mark an enum's variants as role names",
        "packed" => "@packed(Layout.Row|Layout.Column) — flat value-struct layout",
        "tier" => "@tier(name, …) — declare a dev-tier and its runner",
        _ => "decorator directive",
    }
}

/// The directive candidates offered right after an `@` (**directive completion**, C4): the built-in
/// decorator directives (the parser's closed set, so completion and the grammar can never drift)
/// followed by the **tier name-space** — the installed extensions' tiers (`test`/`bench`/`doc`/
/// `debug` plus any native package's) and the program's own `@tier` declarations, read from the
/// same [`noeta_check::tiers::TierRegistry`] the checker validates `@<tier>` blocks against.
/// `program` should be the merged workspace program so an imported package's declared tier is
/// offered. De-duplicated by label (a program re-declaration of an extension tier — a second
/// *provider* — is still one name).
pub fn directives(program: &Program) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for name in noeta_parser::DECORATOR_DIRECTIVES {
        candidates.push(Candidate {
            label: (*name).to_string(),
            kind: CandidateKind::Directive,
            detail: Some(decorator_detail(name).to_string()),
        });
    }
    // The registry-scoped tier name-space (LSP/IDE run single-registry: the seeded process global).
    let reg = noeta_stdlib::registry::single_registry_process();
    let tiers = noeta_check::tiers::TierRegistry::collect_with_registry(program, reg);
    for tier in tiers.extension_tiers() {
        let detail = match (tier.expr, tier.text) {
            (Some(ty), _) => format!("expression tier — @{} {{ … }} : {ty}", tier.name),
            (None, Some(lang)) => format!("text tier ({lang})"),
            (None, None) => "dev-tier".to_string(),
        };
        candidates.push(Candidate {
            label: tier.name.to_string(),
            kind: CandidateKind::Directive,
            detail: Some(detail),
        });
    }
    for tier in tiers.declared_tiers() {
        let provider = if tier.root.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tier.root)
        };
        let detail = match (&tier.expr, &tier.text) {
            (Some(ty), _) => format!("expression tier — @{} {{ … }} : {ty}{provider}", tier.name),
            (None, Some(lang)) => format!("text tier ({lang}){provider}"),
            (None, None) => format!("dev-tier{provider}"),
        };
        candidates.push(Candidate {
            label: tier.name.clone(),
            kind: CandidateKind::Directive,
            detail: Some(detail),
        });
    }
    dedupe_by_label(candidates)
}

/// The directive-argument position the cursor is in: inside the parens of `@<directive>(…)`,
/// with enough of the current argument decoded to pick the right vocabulary (C5). Detection is
/// textual (a half-typed directive argument does not parse) and stays within the cursor's line
/// (a directive's argument list never spans lines).
#[derive(Debug, Clone, PartialEq)]
pub struct DirectiveArgContext {
    /// The directive name before the open paren (`derive`, `role`, `packed`, `tier`, `attribute`,
    /// or a tier name like `bench`).
    pub directive: String,
    /// 0-based index of the argument the cursor is in (top-level comma count).
    pub active: usize,
    /// `Some(head)` when the current argument is a dotted qualifier in progress (`Layout.`,
    /// `Semantic.Entry|`) — completion offers `head`'s members.
    pub after_dot: Option<String>,
    /// `Some(head)` when the cursor is inside an unclosed generic-argument list (`Serialize<|`) —
    /// completion offers `head`'s type arguments (the serialization formats).
    pub in_generic: Option<String>,
}

/// Detect the [`DirectiveArgContext`] at byte `offset` of `text`, or `None` when the cursor is not
/// inside a directive's parens.
pub fn directive_arg_context(text: &str, offset: u32) -> Option<DirectiveArgContext> {
    let bytes = text.as_bytes();
    let offset = (offset as usize).min(bytes.len());
    let line_start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);

    // The innermost unclosed `(` on the cursor's line.
    let mut opens: Vec<usize> = Vec::new();
    for (i, b) in bytes[line_start..offset].iter().enumerate() {
        match b {
            b'(' => opens.push(line_start + i),
            b')' => {
                opens.pop();
            }
            _ => {}
        }
    }
    let open = *opens.last()?;

    // It must be preceded by `@name` to be a directive's argument list.
    let mut i = open;
    while i > line_start && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    if i == open || i == line_start || bytes[i - 1] != b'@' {
        return None;
    }
    let directive = text[i..open].to_string();

    // The argument region so far: the active index is the top-level comma count (commas inside a
    // generic list `Serialize<K, V>` don't separate directive arguments), and the current argument
    // is what follows the last top-level comma.
    let args = &text[open + 1..offset];
    let (mut active, mut angle_depth, mut arg_start) = (0usize, 0i32, 0usize);
    for (i, c) in args.char_indices() {
        match c {
            '<' => angle_depth += 1,
            '>' => angle_depth = (angle_depth - 1).max(0),
            ',' if angle_depth == 0 => {
                active += 1;
                arg_start = i + 1;
            }
            _ => {}
        }
    }
    let current = args[arg_start..].trim_start();

    // Inside an unclosed generic list? (`Serialize<`, `Serialize<Js`)
    let in_generic = current.find('<').and_then(|lt| {
        let closed = current[lt..].contains('>');
        (!closed).then(|| current[..lt].trim().to_string())
    });
    // A dotted qualifier in progress? (`Layout.`, `Semantic.Entry`)
    let after_dot = (in_generic.is_none())
        .then(|| {
            current
                .rfind('.')
                .map(|dot| current[..dot].trim().to_string())
        })
        .flatten()
        .filter(|head| !head.is_empty() && head.chars().all(|c| c.is_alphanumeric() || c == '_'));

    Some(DirectiveArgContext {
        directive,
        active,
        after_dot,
        in_generic,
    })
}

/// The completion candidates inside a directive's argument list (C5), from the directive's own
/// vocabulary — the same sources the parser/checker validate against, so completion can never
/// offer what they would reject. `program` should be the merged workspace program (an imported
/// `@semantic` enum or a dependency's `@tier` declaration is offered). Unknown vocabulary (a
/// `@tier` fresh name, an unknown tier) yields an empty list — the caller still suppresses the
/// general identifier completion, which would be pure noise inside a directive.
pub fn directive_arg_candidates(ctxt: &DirectiveArgContext, program: &Program) -> Vec<Candidate> {
    match ctxt.directive.as_str() {
        "derive" => derive_candidates(ctxt),
        "role" => role_candidates(ctxt, program),
        "attribute" => noeta_ast::reflect::ATTRIBUTE_TARGET_KINDS
            .iter()
            .map(|kind| Candidate {
                label: (*kind).to_string(),
                kind: CandidateKind::Keyword,
                detail: Some(format!("attach to every {}", kind.to_lowercase())),
            })
            .collect(),
        "packed" => packed_candidates(ctxt),
        "semantic" => Vec::new(), // takes no arguments
        "tier" => {
            // First positional is the fresh tier name; the rest are the named forms.
            if ctxt.active == 0 {
                Vec::new()
            } else {
                [
                    ("config: ", "config: Type — the tier's knob attribute"),
                    ("text: ", "text: \"<lang>\" — a verbatim text tier"),
                    (
                        "expr: ",
                        "expr: Type — an expression tier (blocks are values)",
                    ),
                ]
                .into_iter()
                .map(|(label, detail)| Candidate {
                    label: label.to_string(),
                    kind: CandidateKind::Keyword,
                    detail: Some(detail.to_string()),
                })
                .collect()
            }
        }
        // Any other directive is a tier annotation (`@bench(…)`): its arguments are the knobs of
        // the tier's config attribute.
        tier => tier_config_candidates(tier, program),
    }
}

/// `@derive(…)`: the derivable built-in traits; inside `Serialize<…>`/`Deserialize<…>`, the
/// serialization formats.
fn derive_candidates(ctxt: &DirectiveArgContext) -> Vec<Candidate> {
    if ctxt.in_generic.is_some() {
        return noeta_types::SERIALIZE_FORMATS
            .iter()
            .map(|format| Candidate {
                label: (*format).to_string(),
                kind: CandidateKind::Type,
                detail: Some("serialization format".to_string()),
            })
            .collect();
    }
    noeta_types::BUILTIN_TRAITS
        .iter()
        .filter(|t| t.derivable())
        .map(|t| {
            let detail = match t.generic_arity() {
                0 => "derivable trait".to_string(),
                _ => format!("derivable trait — takes a format: {}<Json>", t.name()),
            };
            Candidate {
                label: t.name().to_string(),
                kind: CandidateKind::Trait,
                detail: Some(detail),
            }
        })
        .collect()
}

/// `@role(…)`: the role-eligible (`@semantic`) enums — the built-in `Semantic` plus the program's
/// own — and, after `Enum.`, that enum's variants.
fn role_candidates(ctxt: &DirectiveArgContext, program: &Program) -> Vec<Candidate> {
    if let Some(head) = &ctxt.after_dot {
        let variants: Vec<String> = if head == noeta_ast::reflect::SEMANTIC_ENUM {
            noeta_ast::reflect::SEMANTIC_VARIANTS
                .iter()
                .map(|v| (*v).to_string())
                .collect()
        } else {
            program
                .stmts
                .iter()
                .find_map(|stmt| match stmt {
                    Stmt::Enum(decl) if decl.name == *head && decl.semantic.is_some() => {
                        Some(decl.variants.iter().map(|v| v.name.clone()).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };
        return variants
            .into_iter()
            .map(|name| Candidate {
                label: name,
                kind: CandidateKind::EnumMember,
                detail: Some(format!("role — {head} variant")),
            })
            .collect();
    }
    let mut candidates = vec![Candidate {
        label: noeta_ast::reflect::SEMANTIC_ENUM.to_string(),
        kind: CandidateKind::Enum,
        detail: Some("built-in role vocabulary".to_string()),
    }];
    for stmt in &program.stmts {
        if let Stmt::Enum(decl) = stmt
            && decl.semantic.is_some()
        {
            candidates.push(Candidate {
                label: decl.name.clone(),
                kind: CandidateKind::Enum,
                detail: Some("@semantic enum".to_string()),
            });
        }
    }
    dedupe_by_label(candidates)
}

/// `@packed(…)`: the `Layout` enum — the qualified variants at head position, the bare variants
/// after `Layout.`.
fn packed_candidates(ctxt: &DirectiveArgContext) -> Vec<Candidate> {
    let detail = |variant: &str| match variant {
        "Row" => "row-major (AoS) — the bare-@packed default",
        "Column" => "column-major (SoA) — fastest for the bulk kernels",
        _ => "storage layout",
    };
    match &ctxt.after_dot {
        Some(head) if head == noeta_ast::reflect::LAYOUT_ENUM => {
            noeta_ast::reflect::LAYOUT_VARIANTS
                .iter()
                .map(|v| Candidate {
                    label: (*v).to_string(),
                    kind: CandidateKind::EnumMember,
                    detail: Some(detail(v).to_string()),
                })
                .collect()
        }
        Some(_) => Vec::new(),
        None => noeta_ast::reflect::LAYOUT_VARIANTS
            .iter()
            .map(|v| Candidate {
                label: format!("{}.{v}", noeta_ast::reflect::LAYOUT_ENUM),
                kind: CandidateKind::EnumMember,
                detail: Some(detail(v).to_string()),
            })
            .collect(),
    }
}

/// A tier annotation's arguments (`@bench(iterations: 1000)`): the fields of the tier's config
/// attribute, as `name:` keys. The tier resolves through the same [`TierRegistry`] name-space the
/// checker uses; the config attribute is an extension attribute (`Bench { iterations }`) or a
/// program-declared `@attribute` struct.
fn tier_config_candidates(tier: &str, program: &Program) -> Vec<Candidate> {
    let reg = noeta_stdlib::registry::single_registry_process();
    let tiers = noeta_check::tiers::TierRegistry::collect_with_registry(program, reg);
    let config = reg
        .find_ext_tier(tier)
        .and_then(|t| t.config.map(String::from))
        .or_else(|| tiers.declared(tier).and_then(|d| d.config.clone()));
    let Some(config) = config else {
        return Vec::new(); // knob-less tier (`@test`) or unknown name
    };
    // Extension attribute (`Bench { iterations: int }`)…
    if let Some(attr) = noeta_stdlib::registry::find_ext_attribute(&config) {
        return attr
            .fields
            .iter()
            .map(|f| Candidate {
                label: format!("{}: ", f.name),
                kind: CandidateKind::Field,
                detail: Some(format!(
                    "{}{}",
                    render_attr_field_type(&f.ty),
                    if f.default.is_some() {
                        " (optional)"
                    } else {
                        ""
                    }
                )),
            })
            .collect();
    }
    // …or a program-declared `@attribute` struct.
    program
        .stmts
        .iter()
        .find_map(|stmt| match stmt {
            Stmt::Struct(decl) if decl.name == config => Some(
                decl.fields
                    .iter()
                    .map(|f| Candidate {
                        label: format!("{}: ", f.name),
                        kind: CandidateKind::Field,
                        detail: f.ty.as_ref().map(symbols::render_type_ref),
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// Render an extension attribute field's type for a completion detail.
fn render_attr_field_type(ty: &noeta_stdlib::registry::AttrFieldType) -> &'static str {
    use noeta_stdlib::registry::AttrFieldType;
    match ty {
        AttrFieldType::Int => "int",
        AttrFieldType::Str => "string",
        AttrFieldType::Dyn => "dyn",
    }
}

/// Whether the cursor at byte `offset` of `text` sits in an `@`-directive position: immediately
/// after an `@`, or after `@` plus a partial directive name (`@te|`). Textual — it must work
/// mid-edit, where a dangling `@` never reaches the AST.
pub fn is_directive_position(text: &str, offset: u32) -> bool {
    let bytes = text.as_bytes();
    let mut i = (offset as usize).min(bytes.len());
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    i > 0 && bytes[i - 1] == b'@'
}

/// The member candidates of the type named `type_name` in `program`: its fields, enum variants, and
/// methods, each with a signature/type detail — for member completion after `.`, once the receiver's
/// type is known. Empty if no such type is declared (or it declares no members). The `program`
/// should be the merged workspace program so a type imported from a sibling resolves.
pub fn members_of(program: &Program, type_name: &str) -> Vec<Candidate> {
    let mut members = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Struct(decl) if decl.name == type_name => {
                for field in &decl.fields {
                    members.push(Candidate {
                        label: field.name.clone(),
                        kind: CandidateKind::Field,
                        detail: field.ty.as_ref().map(symbols::render_type_ref),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            Stmt::Class(decl) if decl.name == type_name => {
                for field in &decl.fields {
                    members.push(Candidate {
                        label: field.name.clone(),
                        kind: CandidateKind::Field,
                        detail: field.ty.as_ref().map(symbols::render_type_ref),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            Stmt::Enum(decl) if decl.name == type_name => {
                for variant in &decl.variants {
                    members.push(Candidate {
                        label: variant.name.clone(),
                        kind: CandidateKind::EnumMember,
                        detail: symbols::variant_detail(variant),
                    });
                }
                push_methods(&mut members, &decl.methods);
            }
            _ => {}
        }
    }
    dedupe_by_label(members)
}

/// Append each method as a `Method` candidate carrying its signature.
/// The methods a **bundle binding** contributes to a receiver (kernel-methods K4): resolve each
/// of the type's recorded `(module, bundle)` bindings against the registry and list the methods
/// matching the receiver kind (`Element` on a `T` receiver, `Bulk` on a `List<T>`), with the
/// declared signature as detail.
pub fn bundle_members(
    bindings: &[(String, String)],
    receiver: noeta_stdlib::BundleReceiver,
) -> Vec<Candidate> {
    let mut members = Vec::new();
    for (module, bundle) in bindings {
        let Some(bundle) = noeta_stdlib::registry::find_bundle(module, bundle) else {
            continue;
        };
        for m in bundle.methods.iter().filter(|m| m.receiver == receiver) {
            members.push(Candidate {
                label: m.sig.name.to_string(),
                kind: CandidateKind::Method,
                // Parameter types via the canonical registry-signature renderer
                // (`SigType::render` in noeta-ext-abi, shared with the MCP `stdlib_api` tool),
                // with the bundle provenance as a suffix.
                detail: Some(format!(
                    "fn {}({}) [{}.{}]",
                    m.sig.name,
                    m.sig
                        .params
                        .iter()
                        .map(noeta_stdlib::SigType::render)
                        .collect::<Vec<_>>()
                        .join(", "),
                    module,
                    bundle.name
                )),
            });
        }
    }
    members
}

fn push_methods(members: &mut Vec<Candidate>, methods: &[noeta_ast::FnDecl]) {
    for method in methods {
        members.push(Candidate {
            label: method.name.clone(),
            kind: CandidateKind::Method,
            detail: Some(symbols::fn_signature(method)),
        });
    }
}

/// The built-in type names offered in type-annotation position — the primitives, the container
/// generics, and the fixed-width integers.
const BUILTIN_TYPES: &[&str] = &[
    "int", "float", "f32", "bool", "string", "bytes", "unit", "dyn", "List", "Map", "Set",
    "Option", "Result", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
];

/// The type names offered in a type-annotation position (C3): the user-declared `struct`/`class`/
/// `enum` types (with their precise kinds) followed by the built-in types. De-duplicated by label.
/// `program` should be the merged workspace program so an imported type is offered.
pub fn type_names(program: &Program) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for stmt in &program.stmts {
        let (name, kind) = match stmt {
            Stmt::Struct(decl) => (&decl.name, CandidateKind::Struct),
            Stmt::Class(decl) => (&decl.name, CandidateKind::Class),
            Stmt::Enum(decl) => (&decl.name, CandidateKind::Enum),
            // A trait is valid in type position after `dyn` and as a `<T: …>` bound.
            Stmt::Trait(decl) => (&decl.name, CandidateKind::Trait),
            _ => continue,
        };
        candidates.push(Candidate {
            label: name.clone(),
            kind,
            detail: None,
        });
    }
    for name in BUILTIN_TYPES {
        candidates.push(Candidate {
            label: (*name).to_string(),
            kind: CandidateKind::Type,
            detail: None,
        });
    }
    dedupe_by_label(candidates)
}

/// Whether `offset` falls inside a type-annotation position of `program` — a parameter/field/return/
/// binding type, an enum backing, or a nested type argument. The caller splices a synthetic type name
/// in at the cursor and re-parses first, so an *empty* annotation (`x: |`) is detected as the
/// synthetic name's `TypeRef` covering the cursor, while a map-literal value (`{ "k": | }`) is not (it
/// parses as a value expression, not a `TypeRef`).
pub fn is_type_position(program: &Program, offset: u32) -> bool {
    let mut spans = Vec::new();
    collect_type_spans(&program.stmts, &mut spans);
    spans
        .iter()
        .any(|span| span.start <= offset && offset <= span.end)
}

/// Collect the spans of every type annotation reachable through `stmts`, recursing into declaration
/// bodies (so a binding annotation inside a function is covered) and into nested type arguments.
fn collect_type_spans(stmts: &[Stmt], out: &mut Vec<Span>) {
    for stmt in stmts {
        match stmt {
            Stmt::Binding { ty: Some(ty), .. } => push_type_span(ty, out),
            Stmt::Fn(decl) => collect_fn_type_spans(decl, out),
            Stmt::Struct(decl) => {
                push_field_type_spans(decl.fields.iter().filter_map(|f| f.ty.as_ref()), out);
                decl.methods
                    .iter()
                    .for_each(|m| collect_fn_type_spans(m, out));
            }
            Stmt::Class(decl) => {
                push_field_type_spans(decl.fields.iter().filter_map(|f| f.ty.as_ref()), out);
                decl.methods
                    .iter()
                    .for_each(|m| collect_fn_type_spans(m, out));
            }
            Stmt::Enum(decl) => {
                if let Some(backing) = &decl.backing {
                    push_type_span(backing, out);
                }
                for variant in &decl.variants {
                    push_field_type_spans(variant.fields.iter().filter_map(|f| f.ty.as_ref()), out);
                }
                decl.methods
                    .iter()
                    .for_each(|m| collect_fn_type_spans(m, out));
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_type_spans(then_body, out);
                if let Some(else_body) = else_body {
                    collect_type_spans(else_body, out);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } | Stmt::Concurrent { body, .. } => {
                collect_type_spans(body, out)
            }
            _ => {}
        }
    }
}

/// The type spans of a function/method: its parameters, its return type, and — recursing — its body.
fn collect_fn_type_spans(decl: &FnDecl, out: &mut Vec<Span>) {
    push_field_type_spans(decl.params.iter().filter_map(|p| p.ty.as_ref()), out);
    if let Some(ret) = &decl.ret {
        push_type_span(ret, out);
    }
    collect_type_spans(&decl.body, out);
}

fn push_field_type_spans<'a>(types: impl Iterator<Item = &'a TypeRef>, out: &mut Vec<Span>) {
    for ty in types {
        push_type_span(ty, out);
    }
}

/// Record a type reference's span and recurse into its nested arguments (so completion inside
/// `List<|>` or `A | |` is a type position too).
fn push_type_span(ty: &TypeRef, out: &mut Vec<Span>) {
    out.push(ty.span());
    match ty {
        TypeRef::Named { args, .. } => args.iter().for_each(|a| push_type_span(a, out)),
        TypeRef::DynTrait { .. } => {}
        TypeRef::Optional { inner, .. } => push_type_span(inner, out),
        TypeRef::Union { members, .. } => members.iter().for_each(|m| push_type_span(m, out)),
        TypeRef::Tuple { elements, .. } => elements.iter().for_each(|e| push_type_span(e, out)),
        TypeRef::Fn { params, ret, .. } => {
            params.iter().for_each(|p| push_type_span(p, out));
            push_type_span(ret, out);
        }
    }
}

/// Keep the first candidate for each label, preserving order.
fn dedupe_by_label(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.label.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::Source;

    fn complete_at(src: &str, offset: u32) -> Vec<Candidate> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        complete(&program, offset, SourceId::FIRST)
    }

    fn labels_of(candidates: &[Candidate], kind: CandidateKind) -> Vec<&str> {
        candidates
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| c.label.as_str())
            .collect()
    }

    #[test]
    fn bundle_members_list_the_bound_methods_by_receiver_kind() {
        // The std `vec.Kernels` bundle (registered via the lazy default): Bulk methods appear
        // on a List receiver, Element methods (none today) on the type itself.
        let bindings = vec![("std.vec".to_string(), "Kernels".to_string())];
        let bulk = bundle_members(&bindings, noeta_stdlib::BundleReceiver::Bulk);
        let labels: Vec<&str> = bulk.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"add_all"), "got {labels:?}");
        assert!(labels.contains(&"dot_all"));
        assert!(
            bulk.iter().all(|c| c.kind == CandidateKind::Method),
            "bundle members complete as methods"
        );
        let detail = &bulk.iter().find(|c| c.label == "dot_all").unwrap().detail;
        assert!(
            detail.as_deref().unwrap_or("").contains("Kernels"),
            "detail names the bundle: {detail:?}"
        );
        let element = bundle_members(&bindings, noeta_stdlib::BundleReceiver::Element);
        let element_labels: Vec<&str> = element.iter().map(|c| c.label.as_str()).collect();
        assert!(element_labels.contains(&"dot"), "got {element_labels:?}");
        assert!(
            !element_labels.contains(&"dot_all"),
            "Bulk methods stay off the element receiver"
        );
    }

    #[test]
    fn keywords_are_always_offered() {
        let cands = complete_at("", 0);
        let kws = labels_of(&cands, CandidateKind::Keyword);
        assert!(kws.contains(&"fn"));
        assert!(kws.contains(&"match"));
        assert!(kws.contains(&"struct"));
    }

    #[test]
    fn top_level_declarations_are_offered_with_their_kinds() {
        let src = "fn greet(): int { return 1 }\nstruct Point { x: int }\nenum Color { Red }";
        // Cursor at end of file.
        let cands = complete_at(src, src.len() as u32);
        assert!(labels_of(&cands, CandidateKind::Function).contains(&"greet"));
        assert!(labels_of(&cands, CandidateKind::Struct).contains(&"Point"));
        assert!(labels_of(&cands, CandidateKind::Enum).contains(&"Color"));
    }

    #[test]
    fn parameters_and_locals_are_offered_inside_a_function() {
        let src = "fn f(count: int): int {\n  total = count + 1\n  return total\n}";
        // Cursor on the `return total` line — both the parameter and the local are in scope.
        let offset = src.find("return").unwrap() as u32;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"count"), "parameter in scope; got {vars:?}");
        assert!(vars.contains(&"total"), "local in scope; got {vars:?}");
    }

    #[test]
    fn a_binding_is_not_visible_before_its_own_initializer() {
        // Inside `x`'s initializer, `x` is not yet in scope, but the parameter `n` is.
        let src = "fn f(n: int): int {\n  x = n + 1\n  return x\n}";
        let offset = src.find("n + 1").unwrap() as u32 + 1; // on the `n` in the initializer
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"n"));
        assert!(!vars.contains(&"x"), "x not visible in its own initializer");
    }

    #[test]
    fn whitespace_gap_in_a_body_offers_params_and_prior_locals() {
        // A blank line inside the function body — no AST node covers the cursor (C1.1).
        let src = "fn f(count: int): int {\n  total = count\n  \n  return total\n}";
        let offset = src.find("\n  \n").unwrap() as u32 + 2; // on the blank line's whitespace
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(
            vars.contains(&"count"),
            "param visible in whitespace; got {vars:?}"
        );
        assert!(
            vars.contains(&"total"),
            "prior local visible in whitespace; got {vars:?}"
        );
    }

    #[test]
    fn whitespace_gap_does_not_offer_a_later_local() {
        // In the gap *before* `later` is declared, it is not yet in scope.
        let src = "fn f() {\n  early = 1\n  \n  later = 2\n}";
        let offset = src.find("\n  \n").unwrap() as u32 + 2;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"early"));
        assert!(
            !vars.contains(&"later"),
            "a later local must not leak backward; got {vars:?}"
        );
    }

    #[test]
    fn trailing_whitespace_in_a_body_offers_all_locals() {
        // After the last statement but before the closing brace — every body local is visible.
        let src = "fn f() {\n  a = 1\n  b = 2\n  \n}";
        let offset = src.rfind("\n  \n").unwrap() as u32 + 2;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(vars.contains(&"a") && vars.contains(&"b"), "got {vars:?}");
    }

    #[test]
    fn whitespace_in_one_body_does_not_leak_another() {
        let src = "fn a() {\n  inner = 1\n}\nfn b() {\n  \n}";
        let offset = src.rfind("\n  \n").unwrap() as u32 + 2; // blank line inside b's body
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(
            !vars.contains(&"inner"),
            "a's local leaked into b's whitespace; got {vars:?}"
        );
    }

    #[test]
    fn out_of_scope_locals_are_not_offered() {
        // `inner` is local to `a`; completing in `b` must not see it.
        let src = "fn a() {\n  inner = 1\n}\nfn b() {\n  return 0\n}";
        let offset = src.find("return 0").unwrap() as u32;
        let cands = complete_at(src, offset);
        let vars = labels_of(&cands, CandidateKind::Variable);
        assert!(
            !vars.contains(&"inner"),
            "leaked a's local into b; got {vars:?}"
        );
    }

    fn members(src: &str, type_name: &str) -> Vec<Candidate> {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        let program = parse(&source, &lexed.tokens).program;
        members_of(&program, type_name)
    }

    #[test]
    fn members_of_a_class_lists_fields_and_methods() {
        let src = "class Counter { n: int\n  fn get(): int { return self.n }\n}";
        let ms = members(src, "Counter");
        let field = ms.iter().find(|c| c.label == "n").unwrap();
        assert_eq!(field.kind, CandidateKind::Field);
        assert_eq!(field.detail.as_deref(), Some("int"));
        let method = ms.iter().find(|c| c.label == "get").unwrap();
        assert_eq!(method.kind, CandidateKind::Method);
        assert_eq!(method.detail.as_deref(), Some("() -> int"));
    }

    #[test]
    fn members_of_an_enum_lists_variants() {
        let src = "enum Shape {\n  Dot\n  Circle(radius: int)\n}";
        let ms = members(src, "Shape");
        assert_eq!(
            ms.iter().find(|c| c.label == "Dot").unwrap().kind,
            CandidateKind::EnumMember
        );
        let circle = ms.iter().find(|c| c.label == "Circle").unwrap();
        assert_eq!(circle.detail.as_deref(), Some("(radius: int)"));
    }

    #[test]
    fn members_of_unknown_type_is_empty() {
        assert!(members("struct Point { x: int }", "Nope").is_empty());
    }

    fn program_of(src: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        parse(&source, &lexed.tokens).program
    }

    #[test]
    fn type_names_include_user_types_and_builtins() {
        let names: Vec<String> =
            type_names(&program_of("struct Point { x: int }\nenum Color { Red }"))
                .into_iter()
                .map(|c| c.label)
                .collect();
        assert!(names.contains(&"Point".to_string()));
        assert!(names.contains(&"Color".to_string()));
        assert!(names.contains(&"int".to_string()));
        assert!(names.contains(&"List".to_string()));
    }

    #[test]
    fn is_type_position_true_in_annotations() {
        // A parameter annotation `count: Point`.
        let src = "fn f(count: Point) {}";
        let program = program_of(src);
        let offset = src.find("Point").unwrap() as u32 + 2; // inside the type name
        assert!(is_type_position(&program, offset));
    }

    #[test]
    fn is_type_position_false_in_value_context() {
        // A map-literal value position — the `Point` here is a value expression, not a type.
        let src = "m = { \"k\": 1 }\nx = 2";
        let program = program_of(src);
        let offset = src.find("x = 2").unwrap() as u32 + 4; // on the `2`
        assert!(!is_type_position(&program, offset));
    }

    #[test]
    fn is_type_position_true_for_nested_type_argument() {
        let src = "fn f(xs: List<Point>) {}";
        let program = program_of(src);
        let offset = src.find("Point").unwrap() as u32 + 1; // inside the `List<Point>` argument
        assert!(is_type_position(&program, offset));
    }

    #[test]
    fn directives_offer_decorators_and_builtin_tiers() {
        let cands = directives(&program_of(""));
        let labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
        // The parser's closed decorator set…
        for name in ["derive", "attribute", "role", "semantic", "packed", "tier"] {
            assert!(
                labels.contains(&name),
                "missing decorator {name}: {labels:?}"
            );
        }
        // …and the extension-declared built-in tiers.
        for name in ["test", "bench", "doc", "debug"] {
            assert!(labels.contains(&name), "missing tier {name}: {labels:?}");
        }
        assert!(
            cands.iter().all(|c| c.kind == CandidateKind::Directive),
            "directive completion is homogeneous"
        );
        let doc = cands.iter().find(|c| c.label == "doc").unwrap();
        assert!(
            doc.detail.as_deref().unwrap_or("").contains("markdown"),
            "the doc text tier names its body language: {:?}",
            doc.detail
        );
    }

    #[test]
    fn directives_offer_program_declared_tiers() {
        let src = "@tier(sql, text: \"sql\", expr: Query)\nfn q(statics: List<string>, holes: List<() -> int>): Query { return Query {} }\nstruct Query {}\n";
        let cands = directives(&program_of(src));
        let sql = cands
            .iter()
            .find(|c| c.label == "sql")
            .expect("declared tier offered");
        assert_eq!(sql.kind, CandidateKind::Directive);
        assert!(
            sql.detail
                .as_deref()
                .unwrap_or("")
                .contains("expression tier"),
            "detail describes the tier form: {:?}",
            sql.detail
        );
    }

    #[test]
    fn a_redeclared_extension_tier_is_offered_once() {
        // A program `@tier(bench)` is a second *provider* of the extension tier, not a new name.
        let src = "@tier(bench)\nfn run_bench(roots: List<TierRoot>): void {}\n";
        let cands = directives(&program_of(src));
        assert_eq!(
            cands.iter().filter(|c| c.label == "bench").count(),
            1,
            "one candidate per tier name"
        );
    }

    fn arg_ctx(src: &str) -> Option<DirectiveArgContext> {
        directive_arg_context(src, src.len() as u32)
    }

    #[test]
    fn directive_arg_context_detects_directive_and_active_argument() {
        let c = arg_ctx("@derive(").unwrap();
        assert_eq!((c.directive.as_str(), c.active), ("derive", 0));
        let c = arg_ctx("@derive(Equatable, Compa").unwrap();
        assert_eq!((c.directive.as_str(), c.active), ("derive", 1));
        // A generic list's comma is not an argument separator.
        let c = arg_ctx("@derive(Serialize<Json, ").unwrap();
        assert_eq!(c.active, 0);
        assert_eq!(c.in_generic.as_deref(), Some("Serialize"));
        // Dotted qualifier in progress.
        let c = arg_ctx("@role(Semantic.").unwrap();
        assert_eq!(c.after_dot.as_deref(), Some("Semantic"));
        let c = arg_ctx("@packed(Layout.R").unwrap();
        assert_eq!(c.after_dot.as_deref(), Some("Layout"));
        // Not a directive context: an ordinary call, a closed paren, plain code.
        assert!(arg_ctx("f(1, ").is_none());
        assert!(arg_ctx("@derive(Comparable) struct P ").is_none());
        assert!(arg_ctx("x = 1").is_none());
    }

    #[test]
    fn derive_arguments_offer_the_derivable_traits_and_formats() {
        let program = program_of("");
        let cands = directive_arg_candidates(&arg_ctx("@derive(").unwrap(), &program);
        let labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
        for t in [
            "Equatable",
            "Comparable",
            "Display",
            "Clone",
            "Serialize",
            "Deserialize",
        ] {
            assert!(labels.contains(&t), "missing {t}: {labels:?}");
        }
        assert!(
            !labels.contains(&"Add"),
            "non-derivable trait offered: {labels:?}"
        );
        assert!(cands.iter().all(|c| c.kind == CandidateKind::Trait));
        // Inside `Serialize<…>` the formats are offered instead.
        let formats = directive_arg_candidates(&arg_ctx("@derive(Serialize<").unwrap(), &program);
        assert_eq!(
            formats.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            noeta_types::SERIALIZE_FORMATS.to_vec()
        );
    }

    #[test]
    fn role_arguments_offer_semantic_enums_then_variants() {
        let program = program_of("@semantic\nenum Zone { Frontend\n Backend }\nenum Plain { A }");
        let heads = directive_arg_candidates(&arg_ctx("@role(").unwrap(), &program);
        let labels: Vec<&str> = heads.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"Semantic"), "got {labels:?}");
        assert!(
            labels.contains(&"Zone"),
            "user @semantic enum offered; got {labels:?}"
        );
        assert!(
            !labels.contains(&"Plain"),
            "non-semantic enum must not be offered"
        );
        // After `Semantic.` / `Zone.` the variants come.
        let ctx = DirectiveArgContext {
            directive: "role".into(),
            active: 0,
            after_dot: Some("Semantic".into()),
            in_generic: None,
        };
        let vars = directive_arg_candidates(&ctx, &program);
        assert!(vars.iter().any(|c| c.label == "EntryPoint"), "got {vars:?}");
        let ctx = DirectiveArgContext {
            after_dot: Some("Zone".into()),
            ..ctx
        };
        let vars = directive_arg_candidates(&ctx, &program);
        assert!(vars.iter().any(|c| c.label == "Backend"), "got {vars:?}");
    }

    #[test]
    fn packed_and_attribute_arguments_offer_their_vocabularies() {
        let program = program_of("");
        let layouts = directive_arg_candidates(&arg_ctx("@packed(").unwrap(), &program);
        let labels: Vec<&str> = layouts.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["Layout.Row", "Layout.Column"]);
        let ctx = arg_ctx("@packed(Layout.").unwrap();
        let vars = directive_arg_candidates(&ctx, &program);
        assert_eq!(
            vars.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            vec!["Row", "Column"]
        );
        let kinds = directive_arg_candidates(&arg_ctx("@attribute(").unwrap(), &program);
        let labels: Vec<&str> = kinds.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, noeta_ast::reflect::ATTRIBUTE_TARGET_KINDS.to_vec());
    }

    #[test]
    fn tier_annotation_arguments_offer_the_config_knobs() {
        // `@bench(…)`'s config attribute is std's `Bench { iterations: int }`.
        let program = program_of("");
        let knobs = directive_arg_candidates(&arg_ctx("@bench(").unwrap(), &program);
        assert!(
            knobs.iter().any(|c| c.label == "iterations: "),
            "got {knobs:?}"
        );
        // A knob-less tier offers nothing (but the context still suppresses identifier noise).
        assert!(directive_arg_candidates(&arg_ctx("@test(").unwrap(), &program).is_empty());
    }

    #[test]
    fn is_directive_position_after_at_and_partial_name() {
        assert!(is_directive_position("@", 1));
        assert!(is_directive_position("@te", 3));
        assert!(is_directive_position("fn f() {}\n@do", 13));
        assert!(!is_directive_position("x = 1", 5));
        assert!(!is_directive_position("te", 2), "no @ before the word");
        assert!(!is_directive_position("@", 0), "cursor before the @ itself");
    }

    #[test]
    fn labels_are_unique() {
        let src = "fn f() { return 0 }";
        let cands = complete_at(src, src.len() as u32);
        let mut labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
        let total = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total, "duplicate completion labels");
    }
}
