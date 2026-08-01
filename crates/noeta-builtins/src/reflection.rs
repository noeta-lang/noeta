//! **The thirteen reflection intrinsics, and what each one's call looks like.**
//!
//! `type_of`, `type_name`, `attributes_of`, `fields_of`, `traits_of`, `from_bytes`, `roles_of`,
//! `params_of`, `returns_of`, `invoke`, `field_specs_of`, `variants_of` and `construct` are reserved
//! *words*, not declarations: the lexer holds each one back
//! ([`ReservedRole::Reflection`](noeta_lexer::ReservedRole::Reflection)), the parser gives each its
//! own production, and the checker computes each one's result type in its own `synth` arm. Nothing
//! anywhere held their **signatures**, so every consumer that is not the checker had to either
//! restate them or do without: completion offered none of the thirteen, and signature help — which
//! resolves through `FnDecl`s — could answer for none of them, because an intrinsic has no
//! declaration to resolve to.
//!
//! This table is that missing home. One entry per intrinsic, carrying the surface forms it has (a
//! turbofish `f::<T>()`, a runtime-string `f(name)`, a value `f(x)`, or several), and per form its
//! parameter names and types and the result type the checker gives the expression.
//!
//! The result is a property of the **form**, not of the intrinsic. That is not a modelling
//! preference: `attributes_of::<T>()` is `List<Attributed<T>>` and `attributes_of(name)` is
//! `List<Attributed<dyn>>`, because a runtime string names no type for the checker to substitute.
//! One result per intrinsic could only have said one of the two.
//!
//! # Why here
//!
//! Beside [`PRELUDE`](crate::PRELUDE), which is the same kind of fact: per-name metadata about what
//! the *language* provides, held in the one crate that is small enough for everyone to depend on
//! (`noeta-check` already does, and `noeta-ide` now does). The alternative — a hand-written table in
//! `noeta-ide` — would be a restatement in the crate furthest from the truth, which is the bug class
//! this file exists to close, not join.
//!
//! # How it is kept honest
//!
//! Three ties, none of them "remember to update the table":
//!
//! - **Completeness** is derived from the lexer's own census:
//!   [`tests::every_reflection_reserved_word_has_an_entry`] walks
//!   [`noeta_lexer::ReservedWord::all`] and fails if a `Reflection`-role word has no entry (and if an
//!   entry names a word the lexer does not reserve as one). A fourteenth intrinsic cannot be added
//!   without an entry here. This is the same technique — and the same dev-dependency — that
//!   [`crate::tests::the_keyword_form_is_the_lexers_own_answer`] uses for `PRELUDE`'s forms.
//! - **The result types** are checked against the checker's own answers, over live editor buffers,
//!   by `noeta-ide`'s `tests/reflection_intrinsics.rs`. Unification (the checker *reading* this
//!   table) is not possible in this slice — each arm computes its result while also validating its
//!   operands, and several results depend on the turbofish type (`from_bytes::<T>` is `List<T>`) —
//!   so the tie is an assertion, and it fails if either side moves.
//! - **The surface forms** are checked against the parser, by the same test: for every intrinsic it
//!   drives the whole 2 × 5 grid of (turbofish?, arity) and asserts a snippet parses **exactly**
//!   when this table has a matching form. A surface added to the grammar without an entry here fails
//!   there.
//!
//! The reason all three matter is [`isolate`]: a word the lexer reserved that the editor never
//! offered, silent for as long as the two lists were maintained by hand.
//!
//! [`isolate`]: noeta_lexer::TokenKind::IsolateKw

/// One parameter of one call form: the name to show, and its type in surface spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectParam {
    /// The parameter's name, as the documentation and this repo's own prose call it (`blob`,
    /// `target`, `fields`). Intrinsics have no declaration, so there is no `FnDecl` to read it from.
    pub name: &'static str,
    /// The parameter's type in surface spelling (`string`, `bytes`, `List<dyn>`, `dyn`). What the
    /// checker's arm requires of the operand — a lenient arm (one that accepts anything and defers)
    /// is spelled `dyn`.
    pub ty: &'static str,
}

/// One **call form** of an intrinsic — the shape a call is actually written in, and the type it
/// evaluates to.
///
/// Several intrinsics have more than one, and they are genuinely different shapes rather than
/// optional arguments: `construct::<T>(fields)` and `construct(name, fields)` differ in arity *and*
/// in whether a type is named statically, and the parser tells them apart on the token after the
/// keyword. Signature help picks the form the user is writing; completion shows the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectForm {
    /// The turbofish type parameter's display name (`T`, `RoleEnum`) when this form is written
    /// `f::<…>`, `None` when it is written bare. It is a *name*, not a flag, because [`Self::result`]
    /// may mention it.
    pub turbofish: Option<&'static str>,
    /// The positional parameters inside the parentheses, in order. Empty for `type_name::<T>()`.
    pub params: &'static [ReflectParam],
    /// The result type in surface spelling, with [`Self::turbofish`]'s name standing for the type
    /// argument (`List<Attributed<T>>`, `List<T>`).
    ///
    /// **The result belongs to the form, not to the intrinsic**, because the two surfaces of one
    /// name-keyed query do not answer the same type: `attributes_of::<T>()` is
    /// `List<Attributed<T>>`, and `attributes_of(name)` — whose operand is a runtime string, so
    /// there is no `T` to substitute — is `List<Attributed<dyn>>`. That is not an editor detail, it
    /// is what the checker computes, and a table that could only say one of the two would have to
    /// say a wrong thing about the other. It lived on the intrinsic for exactly as long as every
    /// intrinsic's forms happened to agree, which is to say until the manifest queries grew their
    /// dynamic arms.
    pub result: &'static str,
}

/// One reflection intrinsic: its spelling, its call forms, and a one-line summary for the editor to
/// show. The result type is per-[`form`](ReflectForm::result), not per-intrinsic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionIntrinsic {
    /// The reserved word, exactly as it is written. Every one of these is a
    /// [`ReservedRole::Reflection`](noeta_lexer::ReservedRole::Reflection) word, and every such word
    /// is here — see the module docs.
    pub name: &'static str,
    /// The call forms, **canonical first** — the one completion shows in its `detail`, and the one a
    /// reader should take as "how this is written".
    ///
    /// For the name-keyed queries that is the turbofish form, because it is the compile-checked one:
    /// an unresolvable `T` is an `E0013` where an unknown runtime name is a lenient empty answer.
    /// `roles_of` is the exception the rule earns — its operand is *optional*, so the canonical
    /// spelling is the bare `roles_of()` that asks for the whole index, and both scoping forms are
    /// narrowings of it.
    pub forms: &'static [ReflectForm],
    /// One line, in the imperative, for the completion item and the signature label.
    pub summary: &'static str,
}

/// `List<dyn>` — the runtime argument/field list `invoke` and `construct` take.
const LIST_DYN: &str = "List<dyn>";

/// `name: string` — the runtime-string operand every **name-keyed** query takes. One constant
/// because it is one contract: `attributes_of`, `roles_of`, `field_specs_of`, `variants_of` and
/// `construct` all resolve the same qualified type name that `type_name::<T>()` produces.
const TYPE_NAME_OPERAND: ReflectParam = ReflectParam {
    name: "name",
    ty: "string",
};

/// **Every** reflection intrinsic, in the order the lexer's token table declares them.
///
/// Kept in the lexer's order deliberately: the completeness test walks
/// [`noeta_lexer::ReservedWord::all`], which is that order, so a reader diffing the two lists reads
/// them side by side. Result types are the checker's, verbatim — see the module docs for the test
/// that holds them to it.
pub const REFLECTION_INTRINSICS: &[ReflectionIntrinsic] = &[
    ReflectionIntrinsic {
        name: "attributes_of",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[],
                result: "List<Attributed<T>>",
            },
            // The name-keyed arm. Its element is `dyn`, not `T`: a runtime string names no type the
            // checker can substitute, so the manifest's materialized values arrive erased.
            ReflectForm {
                turbofish: None,
                params: &[TYPE_NAME_OPERAND],
                result: "List<Attributed<dyn>>",
            },
        ],
        summary: "the materialized `#[T(…)]` attributes in the build manifest, each with its target",
    },
    ReflectionIntrinsic {
        name: "type_of",
        forms: &[ReflectForm {
            turbofish: None,
            params: &[ReflectParam {
                name: "value",
                ty: "dyn",
            }],
            result: "Type",
        }],
        summary: "the runtime `Type` descriptor of a value",
    },
    ReflectionIntrinsic {
        name: "type_name",
        forms: &[ReflectForm {
            turbofish: Some("T"),
            params: &[],
            result: "string",
        }],
        summary: "a type's qualified runtime identity, the key the name-keyed queries take",
    },
    ReflectionIntrinsic {
        name: "fields_of",
        forms: &[ReflectForm {
            turbofish: None,
            params: &[ReflectParam {
                name: "value",
                ty: "dyn",
            }],
            result: "List<FieldEntry>",
        }],
        summary: "an instance's field names and values, in declaration order",
    },
    ReflectionIntrinsic {
        name: "traits_of",
        forms: &[ReflectForm {
            turbofish: None,
            params: &[ReflectParam {
                name: "value",
                ty: "dyn",
            }],
            result: "List<string>",
        }],
        summary: "the qualified trait names a value's type has a registered `impl` for",
    },
    // Turbofish only, and the one name-keyed-looking query that is deliberately *not* name-keyed:
    // the packed layout table is keyed on `@packed` struct names, so a scalar element type has no
    // name to pass and nothing could bound a runtime name to a packable type. A dynamic arm would
    // trade a compile-time `E0038` for a runtime abort.
    ReflectionIntrinsic {
        name: "from_bytes",
        forms: &[ReflectForm {
            turbofish: Some("T"),
            params: &[ReflectParam {
                name: "blob",
                ty: "bytes",
            }],
            result: "List<T>",
        }],
        summary: "decode a flat packed buffer into a list of `@packed` values",
    },
    // Three forms, and the only intrinsic whose operand is *optional* — which is why the bare form
    // leads and why signature help must be able to tell an empty argument list from an argument
    // about to be typed.
    ReflectionIntrinsic {
        name: "roles_of",
        forms: &[
            ReflectForm {
                turbofish: None,
                params: &[],
                result: "List<RoleBinding>",
            },
            ReflectForm {
                turbofish: Some("RoleEnum"),
                params: &[],
                result: "List<RoleBinding>",
            },
            ReflectForm {
                turbofish: None,
                params: &[TYPE_NAME_OPERAND],
                result: "List<RoleBinding>",
            },
        ],
        summary: "the `(declaration, role)` index built from `@role(…)` tags, optionally one enum's",
    },
    ReflectionIntrinsic {
        name: "params_of",
        forms: &[ReflectForm {
            turbofish: None,
            params: &[ReflectParam {
                name: "target",
                ty: "string",
            }],
            result: "List<ParamInfo>",
        }],
        summary: "a callable's declared parameters, named as `fn` or `Type.method`",
    },
    ReflectionIntrinsic {
        name: "returns_of",
        forms: &[ReflectForm {
            turbofish: None,
            params: &[ReflectParam {
                name: "target",
                ty: "string",
            }],
            result: "?Type",
        }],
        summary: "a callable's declared return type — `none` when the target names nothing",
    },
    ReflectionIntrinsic {
        name: "invoke",
        forms: &[
            ReflectForm {
                turbofish: None,
                params: &[
                    ReflectParam {
                        name: "name",
                        ty: "string",
                    },
                    ReflectParam {
                        name: "args",
                        ty: LIST_DYN,
                    },
                ],
                result: "Result<dyn, dyn>",
            },
            ReflectForm {
                turbofish: None,
                params: &[
                    ReflectParam {
                        name: "receiver",
                        ty: "dyn",
                    },
                    ReflectParam {
                        name: "name",
                        ty: "string",
                    },
                    ReflectParam {
                        name: "args",
                        ty: LIST_DYN,
                    },
                ],
                result: "Result<dyn, dyn>",
            },
        ],
        summary: "call a function by name (or, with a receiver, a method) — `Err` on miss or arity",
    },
    ReflectionIntrinsic {
        name: "field_specs_of",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[],
                result: "List<FieldSpec>",
            },
            ReflectForm {
                turbofish: None,
                params: &[TYPE_NAME_OPERAND],
                result: "List<FieldSpec>",
            },
        ],
        summary: "a struct or class TYPE's declared field schema",
    },
    ReflectionIntrinsic {
        name: "variants_of",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[],
                result: "List<VariantSpec>",
            },
            ReflectForm {
                turbofish: None,
                params: &[TYPE_NAME_OPERAND],
                result: "List<VariantSpec>",
            },
        ],
        summary: "an enum TYPE's declared variant schema",
    },
    ReflectionIntrinsic {
        name: "construct",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[ReflectParam {
                    name: "fields",
                    ty: LIST_DYN,
                }],
                result: "Result<dyn, string>",
            },
            ReflectForm {
                turbofish: None,
                params: &[
                    TYPE_NAME_OPERAND,
                    ReflectParam {
                        name: "fields",
                        ty: LIST_DYN,
                    },
                ],
                result: "Result<dyn, string>",
            },
        ],
        summary: "build a value from field values at runtime, through the literal's own path",
    },
];

/// The intrinsic spelled `name`, or `None` — the lookup both editor features start from.
pub fn reflection_intrinsic(name: &str) -> Option<&'static ReflectionIntrinsic> {
    REFLECTION_INTRINSICS.iter().find(|i| i.name == name)
}

impl ReflectParam {
    /// `blob: bytes` — the parameter as signature help shows it, and as the rendered call spells it.
    pub fn render(&self) -> String {
        format!("{}: {}", self.name, self.ty)
    }
}

impl ReflectForm {
    /// This form's call surface for the intrinsic `name`, with parameters rendered but no result:
    /// `construct::<T>(fields: List<dyn>)`.
    pub fn render_call(&self, name: &str) -> String {
        let turbofish = match self.turbofish {
            Some(t) => format!("::<{t}>"),
            None => String::new(),
        };
        let params: Vec<String> = self.params.iter().map(ReflectParam::render).collect();
        format!("{name}{turbofish}({})", params.join(", "))
    }

    /// This form rendered whole, for the intrinsic `name` —
    /// `construct::<T>(fields: List<dyn>): Result<dyn, string>`. The completion item's `detail` and
    /// the signature help's label are both this.
    pub fn render(&self, name: &str) -> String {
        format!("{}: {}", self.render_call(name), self.result)
    }
}

impl ReflectionIntrinsic {
    /// The form the *canonical* spelling is: the first, by [`Self::forms`]'s ordering rule.
    pub fn primary_form(&self) -> &'static ReflectForm {
        self.forms.first().expect("an intrinsic has a call form")
    }

    /// The canonical rendering: [`ReflectForm::render`] of [`Self::primary_form`].
    pub fn signature(&self) -> String {
        self.primary_form().render(self.name)
    }

    /// Whether **every** form of this intrinsic is written with a turbofish — so a completion that
    /// inserts the bare word can only ever produce a syntax error. True for `type_name` and
    /// `from_bytes`; false for the eleven that have a bare form, `attributes_of` included since it
    /// gained its name-keyed arm.
    pub fn requires_turbofish(&self) -> bool {
        self.forms.iter().all(|f| f.turbofish.is_some())
    }

    /// The form a call being written matches: the **narrowest** form that can still hold the
    /// `arity_so_far` arguments the user has written, among the forms whose turbofish presence
    /// matches what was typed.
    ///
    /// `arity_so_far` is a count of arguments *begun*, not the index of the one under the cursor,
    /// and the difference is the whole reason it is the input. `roles_of()` and `roles_of(x|)` are
    /// both "argument 0" — the cursor is in the first argument's place in each — but the first has
    /// no arguments and is the bare index query, and the second has one and is the name-scoped
    /// query. Selecting on the cursor's index made the empty call render `roles_of(name: string)`,
    /// which is a signature the user had not asked for and could not get to without deleting.
    ///
    /// Narrowest-that-fits, rather than first-that-fits, so an intrinsic whose forms are declared
    /// canonical-first still picks by shape: `roles_of` leads with its bare form for completion's
    /// sake, and `roles_of(x` must still find the one-operand form behind it.
    ///
    /// Falls back to the **widest** candidate when more arguments have been written than any form
    /// takes (a call with too many arguments still shows the widest signature rather than none), and
    /// to [`Self::primary_form`] when the turbofish matches no form at all — a `type_of::<T>(` that
    /// will not parse should still explain what `type_of` is.
    pub fn form_for(&self, turbofish: bool, arity_so_far: usize) -> &'static ReflectForm {
        let matching = || {
            self.forms
                .iter()
                .filter(|f| f.turbofish.is_some() == turbofish)
        };
        matching()
            .filter(|f| f.params.len() >= arity_so_far)
            .min_by_key(|f| f.params.len())
            .or_else(|| matching().max_by_key(|f| f.params.len()))
            .unwrap_or_else(|| self.primary_form())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every reserved word the lexer files under `Reflection` has an entry, and nothing else
    /// does.**
    ///
    /// The completeness half of the table's honesty. A fourteenth intrinsic is a new
    /// `#[token("…")]` plus an arm in `TokenKind::reserved_role`, and the moment both land this
    /// fails until the word has an entry here — which is what makes completion and signature help
    /// cover it. The reverse direction matters just as much: an entry for a word the lexer has since
    /// retired would have the editor offering a name that no longer parses.
    ///
    /// Checked here rather than in `noeta-ide` because the *table* is the thing that must be
    /// complete; a consumer that filtered it would pass a consumer-side check while the table stayed
    /// short. The lexer is a dev-dependency for exactly this, as it already is for `PRELUDE`.
    #[test]
    fn every_reflection_reserved_word_has_an_entry() {
        use noeta_lexer::{ReservedRole, ReservedWord};

        let reflection: Vec<&'static str> = ReservedWord::all()
            .into_iter()
            .filter(|w| w.role == ReservedRole::Reflection)
            .map(|w| w.word)
            .collect();
        assert!(
            !reflection.is_empty(),
            "the reflection family has emptied out"
        );
        for word in &reflection {
            assert!(
                reflection_intrinsic(word).is_some(),
                "`{word}` is a reflection primitive with no entry in REFLECTION_INTRINSICS — the \
                 editor cannot offer it or describe its call without one"
            );
        }
        for intrinsic in REFLECTION_INTRINSICS {
            let role = ReservedWord::from_spelling(intrinsic.name)
                .unwrap_or_else(|| {
                    panic!(
                        "`{}` has an entry but the lexer does not reserve it",
                        intrinsic.name
                    )
                })
                .role;
            assert_eq!(
                role,
                ReservedRole::Reflection,
                "`{}` has an entry but the lexer files it as {role:?}",
                intrinsic.name
            );
        }
        assert_eq!(
            REFLECTION_INTRINSICS.len(),
            reflection.len(),
            "the table and the census disagree on how many reflection primitives there are"
        );
        // The order is the lexer's, so the two lists read side by side.
        let listed: Vec<&str> = REFLECTION_INTRINSICS.iter().map(|i| i.name).collect();
        assert_eq!(listed, reflection, "the table is not in the lexer's order");
    }

    /// Every entry is renderable: at least one form, every form with a result, and a summary. Cheap,
    /// and it is what stops a stub entry added to silence the completeness test from reaching the
    /// editor as `construct(): `.
    ///
    /// The load-bearing half is the last one — **a form may only name the type argument it binds**.
    /// A bare form whose result says `T` would render a signature promising a type the call has no
    /// way to name, which is exactly what a per-intrinsic result produced for `attributes_of(name)`
    /// before the result moved onto the form.
    #[test]
    fn every_entry_renders() {
        // Every placeholder any form binds anywhere — `T`, `RoleEnum`. A bare form's result may name
        // none of them.
        let placeholders: Vec<&str> = REFLECTION_INTRINSICS
            .iter()
            .flat_map(|i| i.forms.iter())
            .filter_map(|f| f.turbofish)
            .collect();

        for intrinsic in REFLECTION_INTRINSICS {
            assert!(
                !intrinsic.forms.is_empty(),
                "{} has no form",
                intrinsic.name
            );
            assert!(
                !intrinsic.summary.is_empty(),
                "{} has no summary",
                intrinsic.name
            );
            let rendered = intrinsic.signature();
            assert!(rendered.starts_with(intrinsic.name), "{rendered}");
            assert!(
                rendered.ends_with(intrinsic.primary_form().result),
                "{rendered}"
            );
            for form in intrinsic.forms {
                assert!(
                    !form.result.is_empty(),
                    "a form of {} has no result",
                    intrinsic.name
                );
                match form.turbofish {
                    Some(bound) => {
                        assert!(!bound.is_empty());
                        for other in &placeholders {
                            assert!(
                                *other == bound || !names(form.result, other),
                                "{}'s `{}` form names `{other}`, which it does not bind",
                                intrinsic.name,
                                form.render_call(intrinsic.name)
                            );
                        }
                    }
                    None => {
                        for placeholder in &placeholders {
                            assert!(
                                !names(form.result, placeholder),
                                "{}'s bare form results in `{}`, which names the type argument \
                                 `{placeholder}` that a bare call binds nothing to",
                                intrinsic.name,
                                form.result
                            );
                        }
                    }
                }
            }
        }
    }

    /// Whether a rendered type spelling contains `word` as a whole identifier — so `List<T>` names
    /// `T` but `List<Type>` does not.
    fn names(spelling: &str, word: &str) -> bool {
        spelling
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|token| token == word)
    }

    #[test]
    fn rendering_shows_the_turbofish_and_the_result() {
        let construct = reflection_intrinsic("construct").expect("construct is an intrinsic");
        assert_eq!(
            construct.signature(),
            "construct::<T>(fields: List<dyn>): Result<dyn, string>"
        );
        let type_of = reflection_intrinsic("type_of").expect("type_of is an intrinsic");
        assert_eq!(type_of.signature(), "type_of(value: dyn): Type");
        // The canonical form of the one intrinsic whose operand is optional is the bare one.
        let roles_of = reflection_intrinsic("roles_of").expect("roles_of is an intrinsic");
        assert_eq!(roles_of.signature(), "roles_of(): List<RoleBinding>");
    }

    /// **The two surfaces of a name-keyed query do not answer the same type**, and the table can now
    /// say so. `attributes_of::<T>()` materializes real `T`s; `attributes_of(name)` takes a runtime
    /// string, so there is no `T` to substitute and the checker erases the element to `dyn`.
    #[test]
    fn the_two_manifest_surfaces_have_different_results() {
        let attributes_of =
            reflection_intrinsic("attributes_of").expect("attributes_of is an intrinsic");
        assert_eq!(
            attributes_of.form_for(true, 0).result,
            "List<Attributed<T>>"
        );
        assert_eq!(
            attributes_of.form_for(false, 1).result,
            "List<Attributed<dyn>>"
        );
    }

    /// The two that have no bare form are the two a completion must not insert bare. `attributes_of`
    /// left this set when it gained its name-keyed arm — that is the point of deriving the insert
    /// text from the forms rather than listing the words.
    #[test]
    fn the_turbofish_only_intrinsics_are_the_expected_two() {
        let required: Vec<&str> = REFLECTION_INTRINSICS
            .iter()
            .filter(|i| i.requires_turbofish())
            .map(|i| i.name)
            .collect();
        assert_eq!(required, ["type_name", "from_bytes"]);
    }

    /// Form selection follows how many arguments have been *written*, which is the whole reason
    /// `invoke`'s two arities can both be described.
    #[test]
    fn form_selection_follows_the_arity_written_so_far() {
        let invoke = reflection_intrinsic("invoke").expect("invoke is an intrinsic");
        // `invoke(` — nothing written yet; the narrowest form that could hold it.
        assert_eq!(invoke.form_for(false, 0).params.len(), 2);
        assert_eq!(invoke.form_for(false, 1).params.len(), 2);
        assert_eq!(invoke.form_for(false, 2).params.len(), 2);
        assert_eq!(invoke.form_for(false, 3).params.len(), 3);
        // Past every arity: the widest form, not nothing.
        assert_eq!(invoke.form_for(false, 9).params.len(), 3);
        // A turbofish `invoke` matches no form; the primary one still describes the intrinsic.
        assert_eq!(invoke.form_for(true, 0), invoke.primary_form());

        let construct = reflection_intrinsic("construct").expect("construct is an intrinsic");
        assert_eq!(construct.form_for(true, 0).turbofish, Some("T"));
        assert_eq!(construct.form_for(false, 0).params.len(), 2);
    }

    /// **An empty argument list is not "about to type argument 0".**
    ///
    /// `roles_of` is the first intrinsic with two same-turbofish-ness forms of arity 0 and 1, and it
    /// is the case a cursor-index picker cannot express: `roles_of()` and `roles_of(x` put the cursor
    /// in the same place. Selecting on the cursor index rendered the empty call as
    /// `roles_of(name: string)` — a signature for a surface the user had not written.
    ///
    /// Pinned as a property over the whole table rather than for `roles_of` alone, so the next
    /// intrinsic to grow a bare form beside a one-operand form is covered the day it lands: for
    /// every intrinsic, an empty call must select a form of the smallest arity available.
    #[test]
    fn an_empty_argument_list_selects_the_narrowest_form() {
        let roles_of = reflection_intrinsic("roles_of").expect("roles_of is an intrinsic");
        assert_eq!(roles_of.form_for(false, 0).params.len(), 0, "roles_of()");
        assert_eq!(roles_of.form_for(false, 1).params.len(), 1, "roles_of(x");
        assert_eq!(
            roles_of.form_for(true, 0).turbofish,
            Some("RoleEnum"),
            "roles_of::<E>()"
        );

        for intrinsic in REFLECTION_INTRINSICS {
            for turbofish in [false, true] {
                let candidates: Vec<usize> = intrinsic
                    .forms
                    .iter()
                    .filter(|f| f.turbofish.is_some() == turbofish)
                    .map(|f| f.params.len())
                    .collect();
                let Some(narrowest) = candidates.iter().min() else {
                    continue;
                };
                assert_eq!(
                    intrinsic.form_for(turbofish, 0).params.len(),
                    *narrowest,
                    "an empty `{}` call selects a wider form than it has to",
                    intrinsic.name
                );
            }
        }
    }
}
