//! [`Name`] — the type of a **namespace-qualifiable** name.
//!
//! # Why this type exists
//!
//! Four bugs shipped in one week with a single shape: a name or type reference that the linker's
//! qualification pass (`noeta_loader::qualify`) never rewrote, producing a **silent wrong answer**
//! under a `namespace` — usually with no diagnostic at all. `field_specs_of::<Todo>()` under
//! `namespace app.storage` asked for `Todo` and got the empty schema; `gen::<T>(x)` was `E0005`
//! while `gen(x)` resolved; `@role(WebRole.Controller)` was unreachable by any spelling.
//!
//! Two structural fixes preceded this one. The qualifier now binds every AST field by name, so
//! *adding* a field is a compile error there, and `noeta_loader::ast_walk_coverage` runs the real
//! walk over a probe to prove each bound field is actually *visited*. Both are **detection**: they
//! notice the mistake after it is written.
//!
//! This type is the third fix, and it works on the other side — it makes the mistake harder to
//! write. Every one of those bugs began with the same primitive: `String`. In this AST a `String`
//! is a qualifiable declaration name, *or* a local binding, *or* a member name, *or* a tier name in
//! a global namespace, *or* the text of a string literal — five unrelated things wearing one type,
//! so nothing stopped a type reference from being stored where literal text goes (bug 1: the parser
//! flattened `::<Todo>` into an `Expr::Str`, which the qualifier correctly treats as a leaf).
//!
//! Splitting that one primitive in two is the whole idea:
//!
//! * **[`Name`]** — a name the qualification pass is responsible for. If a field holds one, the
//!   walk must reach it, and `ast_walk_coverage` **derives** that requirement from the declared
//!   type with no human judgement: a newly added `Name` field that the walk misses fails the gate
//!   on its own.
//! * **`String`** — everything else, including the genuinely dynamic surfaces that must *not* be
//!   rewritten: `field_specs_of(name)` / `construct(name, …)` with a runtime string,
//!   `json.decode_typed(name, text)`, `invoke(recv, name, args)`, a member name, a local binding.
//!
//! # The static/dynamic boundary
//!
//! The dynamic reflection surfaces are the reason this cannot be "make every name a `Name`".
//! `field_specs_of::<Todo>()` and `field_specs_of(user_input)` are the *same* call with different
//! operands, and the AST already distinguishes them structurally — [`crate::TypeOperand::Static`]
//! holds a [`crate::TypeRef`] (whose leaf is now a [`Name`], so it qualifies), and
//! [`crate::TypeOperand::Dynamic`] holds an [`crate::Expr`] (a runtime string, which must not).
//! That split is the boundary, and this type is what keeps the two halves from being assignable to
//! each other: there is no `From<String> for Name` and no `From<Name> for String`.
//!
//! # What this type deliberately does not do
//!
//! It does **not** carry a per-name "has the pass run over me" bit, and the reasoning is worth
//! recording because the shape is tempting.
//!
//! Qualification is a **per-module** pass: it runs over a whole unit's statements with that unit's
//! rewrite map, and for a module with no `namespace` the map is empty and the pass is a deliberate
//! no-op. So "qualified" means *the pass has run over this module* — not *this string contains a
//! dot*, and not *this particular name changed*. Three consequences, and each one sinks the flag:
//!
//! 1. The flag would have to be stamped by the walk, on every name the walk reaches. But "does the
//!    walk reach every name?" is precisely the property in question, so the stamp cannot also be
//!    its verification — a missed field would arrive unstamped *and* unqualified, and the flag
//!    would report exactly what the missing rewrite already did.
//! 2. It would be `Written` for every name in the compiler's legitimate loader-free entry points. A
//!    single-file conformance case runs straight from source text with no loader at all, and the
//!    REPL and the docs fragments check unlinked code on purpose. An assertion that fires on
//!    correct programs is one nobody may enable, and a flag nobody may assert on is decoration.
//! 3. The granularity is wrong anyway. Qualification's unit is a module, not a name.
//!
//! At *module* granularity the property already holds without new machinery: the merged, qualified
//! program leaves the loader as a `noeta_loader::Linked`, which nothing outside the linker can
//! construct, and every consumer downstream takes one. So the field type says a name **must** be
//! qualified, and the existing loader seam says a program **has** been. What was missing, and is
//! what this type supplies, is the part in between — that a `String` and a `Name` are not
//! interchangeable in either direction, so the qualifiable and the dynamic cannot be swapped by
//! accident.
//!
//! # Constructing one
//!
//! There is no implicit conversion in either direction. A `Name` is built by naming its
//! **provenance**, so every entry point into the qualifiable world is greppable:
//!
//! * [`Name::written`] — the identifier a source file spelled, before qualification. The parser's
//!   constructor, and the only one that should appear in a parser.
//! * [`Name::canonical`] — a name that is already in final qualified form: compiler-synthesized
//!   after the pass ran, a built-in's fixed identity, or a name reconstructed from a serialized
//!   artifact that was qualified when it was written.
//!
//! ```
//! use noeta_ast::Name;
//! let written = Name::written("User");
//! let canonical = Name::canonical("std.id.Uuid");
//! assert_eq!(written.as_str(), "User");
//! assert_eq!(canonical.as_str(), "std.id.Uuid");
//! ```
//!
//! A bare `String` cannot become one by assignment:
//!
//! ```compile_fail
//! # use noeta_ast::Name;
//! let s: String = "User".to_string();
//! let n: Name = s; // no `From<String> for Name`
//! ```
//!
//! ```compile_fail
//! # use noeta_ast::Name;
//! let s: String = "User".to_string();
//! let n: Name = s.into(); // and no `Into` either
//! ```
//!
//! …nor can a `Name` be stored where a runtime string belongs. This is bug 1's exact shape — a
//! type reference routed through a string literal, which the qualifier treats as a leaf:
//!
//! ```compile_fail
//! # use noeta_ast::{Expr, Name};
//! # use noeta_span::{SourceId, Span};
//! # let span = Span { start: 0, end: 0, source: SourceId(0) };
//! let ty = Name::written("User");
//! let e = Expr::Str { value: ty, span }; // `Expr::Str::value` is literal text, not a name
//! ```
//!
//! …and the reverse, a raw string dropped into a qualifiable slot, is equally rejected:
//!
//! ```compile_fail
//! # use noeta_ast::Expr;
//! # use noeta_span::{SourceId, Span};
//! # let span = Span { start: 0, end: 0, source: SourceId(0) };
//! let e = Expr::Ident { name: "User".to_string(), span };
//! ```
//!
//! ```compile_fail
//! # use noeta_ast::TypeRef;
//! # use noeta_span::{SourceId, Span};
//! # let span = Span { start: 0, end: 0, source: SourceId(0) };
//! // A member name is a `String` — it resolves against the receiver's type, never the module map
//! // — so it cannot be stored where a type reference's nominal leaf goes.
//! let member: String = "field".to_string();
//! let ty = TypeRef::Named { name: member, args: vec![], span };
//! ```

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A name that the linker's namespace-qualification pass is responsible for: a type reference's
/// nominal leaf, a declaration's own identity, a trait bound, an `impl` target, an object literal's
/// head, a bare identifier in expression position, an attribute's name.
///
/// Distinct from `String` — which in this AST means a local binding, a member name, a tier name, or
/// literal text, none of which qualify — with no conversion in either direction. See the
/// [module documentation](self) for why, and for the static/dynamic boundary.
///
/// The wire format is the bare string: this is `#[serde(transparent)]`, so a `.noeb` bundle or a
/// serialized reflection manifest written before this type existed still reads back.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Name(String);

impl Name {
    /// The identifier a source file **spelled**, before qualification. The parser's constructor.
    ///
    /// Naming provenance rather than offering `From<String>` is the point: `Name::written` marks
    /// the exact places raw source text enters the qualifiable world, and there are few enough of
    /// them to read.
    pub fn written(text: impl Into<String>) -> Self {
        Name(text.into())
    }

    /// A name that is **already in final form** — nothing downstream should rewrite it.
    ///
    /// Three legitimate sources: a node the compiler synthesizes *after* qualification ran (a
    /// desugar's call to an already-qualified handler, `Expr::NativeFnRef`), a built-in's fixed
    /// identity (`int`, `List`, `Iterator` — never in any module's map), and a name read back from
    /// an artifact that was qualified when it was written (a `.noeb` bundle, a reflection
    /// manifest, an extension's declared types).
    pub fn canonical(text: impl Into<String>) -> Self {
        Name(text.into())
    }

    /// The name's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name's text, consuming it.
    pub fn into_string(self) -> String {
        self.0
    }

    /// **The qualifier's one mutation point.** Replace this name with its qualified identity.
    ///
    /// A `Name`'s text is otherwise immutable, so `grep -rn 'qualify_to'` enumerates every place in
    /// the compiler where a name's meaning changes. It should return exactly the rewrite sites in
    /// `noeta_loader::qualify`.
    pub fn qualify_to(&mut self, qualified: impl Into<String>) {
        self.0 = qualified.into();
    }

    /// Whether the name is empty — the "absent" spelling a few parser positions use in place of an
    /// `Option` (`RoleTag::enum_name` for an unqualified `@role(Variant)`).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// So a `HashMap<Name, _>` can be probed with a `&str`. Reading a name never needed protecting —
/// only building one out of a raw string, and consuming a raw string as one, do.
impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Name> for str {
    fn eq(&self, other: &Name) -> bool {
        self == other.0
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

#[cfg(test)]
mod tests {
    use super::Name;

    #[test]
    fn provenance_constructors_agree_on_the_text() {
        assert_eq!(Name::written("User").as_str(), "User");
        assert_eq!(Name::canonical("App.User").as_str(), "App.User");
    }

    #[test]
    fn qualify_to_is_the_only_rewrite() {
        let mut n = Name::written("User");
        n.qualify_to("App.Models.User");
        assert_eq!(n, "App.Models.User");
    }

    #[test]
    fn serde_is_transparent() {
        let json = serde_json::to_string(&Name::written("User")).unwrap();
        assert_eq!(json, "\"User\"");
        let back: Name = serde_json::from_str("\"App.User\"").unwrap();
        assert_eq!(back, "App.User");
    }
}
