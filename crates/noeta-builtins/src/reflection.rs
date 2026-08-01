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
//! turbofish `f::<T>()`, a runtime-string `f(name)`, a value `f(x)`, or several), each form's
//! parameter names and types, and the result type the checker gives the expression.
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

/// One **call form** of an intrinsic — the shape a call is actually written in.
///
/// Several intrinsics have more than one, and they are genuinely different shapes rather than
/// optional arguments: `construct::<T>(fields)` and `construct(name, fields)` differ in arity *and*
/// in whether a type is named statically, and the parser tells them apart on the token after the
/// keyword. Signature help picks the form the user is writing; completion shows the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectForm {
    /// The turbofish type parameter's display name (`T`, `RoleEnum`) when this form is written
    /// `f::<…>`, `None` when it is written bare. It is a *name*, not a flag, because
    /// [`ReflectionIntrinsic::result`] may mention it.
    pub turbofish: Option<&'static str>,
    /// The positional parameters inside the parentheses, in order. Empty for `type_name::<T>()`.
    pub params: &'static [ReflectParam],
}

/// One reflection intrinsic: its spelling, its call forms, the type the checker gives the
/// expression, and a one-line summary for the editor to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionIntrinsic {
    /// The reserved word, exactly as it is written. Every one of these is a
    /// [`ReservedRole::Reflection`](noeta_lexer::ReservedRole::Reflection) word, and every such word
    /// is here — see the module docs.
    pub name: &'static str,
    /// The call forms, most-specific first: where an intrinsic has a turbofish form and a
    /// runtime-string form, the turbofish comes first, because it is the compile-checked one (an
    /// unresolvable `T` is an `E0013`, where an unknown runtime name is a lenient empty answer).
    /// `roles_of` is the exception the ordering rule earns: its bare form is the whole index and the
    /// turbofish only narrows it, so the bare form leads.
    pub forms: &'static [ReflectForm],
    /// The result type in surface spelling, with a form's [`ReflectForm::turbofish`] name standing
    /// for the type argument (`List<Attributed<T>>`, `List<T>`). Every form of one intrinsic has the
    /// same result — where a result mentions `T`, every form of that intrinsic binds a `T`.
    pub result: &'static str,
    /// One line, in the imperative, for the completion item and the signature label.
    pub summary: &'static str,
}

/// `List<dyn>` — the runtime argument/field list `invoke` and `construct` take.
const LIST_DYN: &str = "List<dyn>";

/// **Every** reflection intrinsic, in the order the lexer's token table declares them.
///
/// Kept in the lexer's order deliberately: the completeness test walks
/// [`noeta_lexer::ReservedWord::all`], which is that order, so a reader diffing the two lists reads
/// them side by side. Result types are the checker's, verbatim — see the module docs for the test
/// that holds them to it.
pub const REFLECTION_INTRINSICS: &[ReflectionIntrinsic] = &[
    ReflectionIntrinsic {
        name: "attributes_of",
        forms: &[ReflectForm {
            turbofish: Some("T"),
            params: &[],
        }],
        result: "List<Attributed<T>>",
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
        }],
        result: "Type",
        summary: "the runtime `Type` descriptor of a value",
    },
    ReflectionIntrinsic {
        name: "type_name",
        forms: &[ReflectForm {
            turbofish: Some("T"),
            params: &[],
        }],
        result: "string",
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
        }],
        result: "List<FieldEntry>",
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
        }],
        result: "List<string>",
        summary: "the qualified trait names a value's type has a registered `impl` for",
    },
    ReflectionIntrinsic {
        name: "from_bytes",
        forms: &[ReflectForm {
            turbofish: Some("T"),
            params: &[ReflectParam {
                name: "blob",
                ty: "bytes",
            }],
        }],
        result: "List<T>",
        summary: "decode a flat packed buffer into a list of `@packed` values",
    },
    ReflectionIntrinsic {
        name: "roles_of",
        forms: &[
            ReflectForm {
                turbofish: None,
                params: &[],
            },
            ReflectForm {
                turbofish: Some("RoleEnum"),
                params: &[],
            },
        ],
        result: "List<RoleBinding>",
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
        }],
        result: "List<ParamInfo>",
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
        }],
        result: "?Type",
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
            },
        ],
        result: "Result<dyn, dyn>",
        summary: "call a function by name (or, with a receiver, a method) — `Err` on miss or arity",
    },
    ReflectionIntrinsic {
        name: "field_specs_of",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[],
            },
            ReflectForm {
                turbofish: None,
                params: &[ReflectParam {
                    name: "name",
                    ty: "string",
                }],
            },
        ],
        result: "List<FieldSpec>",
        summary: "a struct or class TYPE's declared field schema",
    },
    ReflectionIntrinsic {
        name: "variants_of",
        forms: &[
            ReflectForm {
                turbofish: Some("T"),
                params: &[],
            },
            ReflectForm {
                turbofish: None,
                params: &[ReflectParam {
                    name: "name",
                    ty: "string",
                }],
            },
        ],
        result: "List<VariantSpec>",
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
            },
            ReflectForm {
                turbofish: None,
                params: &[
                    ReflectParam {
                        name: "name",
                        ty: "string",
                    },
                    ReflectParam {
                        name: "fields",
                        ty: LIST_DYN,
                    },
                ],
            },
        ],
        result: "Result<dyn, string>",
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
    /// This form's call surface for the intrinsic `name`, with parameters rendered:
    /// `construct::<T>(fields: List<dyn>)`. The result is appended by
    /// [`ReflectionIntrinsic::render_form`], which knows it.
    pub fn render_call(&self, name: &str) -> String {
        let turbofish = match self.turbofish {
            Some(t) => format!("::<{t}>"),
            None => String::new(),
        };
        let params: Vec<String> = self.params.iter().map(ReflectParam::render).collect();
        format!("{name}{turbofish}({})", params.join(", "))
    }
}

impl ReflectionIntrinsic {
    /// The form the *canonical* spelling is: the first, by [`Self::forms`]'s ordering rule.
    pub fn primary_form(&self) -> &'static ReflectForm {
        self.forms.first().expect("an intrinsic has a call form")
    }

    /// One form rendered whole — `construct::<T>(fields: List<dyn>): Result<dyn, string>`. The
    /// completion item's `detail` and the signature help's label are both this.
    pub fn render_form(&self, form: &ReflectForm) -> String {
        format!("{}: {}", form.render_call(self.name), self.result)
    }

    /// The canonical rendering: [`Self::render_form`] of [`Self::primary_form`].
    pub fn signature(&self) -> String {
        self.render_form(self.primary_form())
    }

    /// Whether **every** form of this intrinsic is written with a turbofish — so a completion that
    /// inserts the bare word can only ever produce a syntax error. True for `type_name`,
    /// `attributes_of` and `from_bytes`; false for the ten that have a bare form.
    pub fn requires_turbofish(&self) -> bool {
        self.forms.iter().all(|f| f.turbofish.is_some())
    }

    /// The form a call being written matches: the arity that still has room for the argument at
    /// index `active`, among the forms whose turbofish presence matches what was typed.
    ///
    /// `invoke(a, b, |` is the three-operand form and `invoke(a, |` the two-operand one, and this is
    /// what tells them apart. Falls back to the last candidate when the cursor is past every form's
    /// arity (a call with too many arguments still shows the widest signature rather than none), and
    /// to [`Self::primary_form`] when the turbofish does not match any form at all — a
    /// `type_of::<T>(` that will not parse should still explain what `type_of` is.
    pub fn form_for(&self, turbofish: bool, active: usize) -> &'static ReflectForm {
        let mut matching = self
            .forms
            .iter()
            .filter(|f| f.turbofish.is_some() == turbofish)
            .peekable();
        if matching.peek().is_none() {
            return self.primary_form();
        }
        let mut last = self.primary_form();
        for form in matching {
            last = form;
            if form.params.len() > active {
                return form;
            }
        }
        last
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

    /// Every entry is renderable: at least one form, a result, and a summary. Cheap, and it is what
    /// stops a stub entry added to silence the completeness test from reaching the editor as
    /// `construct(): `.
    #[test]
    fn every_entry_renders() {
        for intrinsic in REFLECTION_INTRINSICS {
            assert!(
                !intrinsic.forms.is_empty(),
                "{} has no form",
                intrinsic.name
            );
            assert!(
                !intrinsic.result.is_empty(),
                "{} has no result",
                intrinsic.name
            );
            assert!(
                !intrinsic.summary.is_empty(),
                "{} has no summary",
                intrinsic.name
            );
            let rendered = intrinsic.signature();
            assert!(rendered.starts_with(intrinsic.name), "{rendered}");
            assert!(rendered.ends_with(intrinsic.result), "{rendered}");
            // A result that mentions a type argument needs every form to bind one, or the rendered
            // signature promises a `T` the call has no way to name.
            for form in intrinsic.forms {
                if let Some(t) = form.turbofish {
                    assert!(!t.is_empty());
                } else {
                    assert!(
                        !mentions_type_arg(intrinsic.result),
                        "{}'s result `{}` names a type argument, but its bare form binds none",
                        intrinsic.name,
                        intrinsic.result
                    );
                }
            }
        }
    }

    /// Whether a result template names a turbofish type argument — a bare `T` token in the spelling.
    fn mentions_type_arg(result: &str) -> bool {
        result
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == "T")
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
        let roles_of = reflection_intrinsic("roles_of").expect("roles_of is an intrinsic");
        assert_eq!(roles_of.signature(), "roles_of(): List<RoleBinding>");
    }

    /// The three that have no bare form are the three a completion must not insert bare.
    #[test]
    fn the_turbofish_only_intrinsics_are_the_expected_three() {
        let required: Vec<&str> = REFLECTION_INTRINSICS
            .iter()
            .filter(|i| i.requires_turbofish())
            .map(|i| i.name)
            .collect();
        assert_eq!(required, ["attributes_of", "type_name", "from_bytes"]);
    }

    /// Form selection follows the argument the cursor is in, which is the whole reason `invoke`'s
    /// two arities can both be described.
    #[test]
    fn form_selection_follows_the_active_argument() {
        let invoke = reflection_intrinsic("invoke").expect("invoke is an intrinsic");
        assert_eq!(invoke.form_for(false, 0).params.len(), 2);
        assert_eq!(invoke.form_for(false, 1).params.len(), 2);
        assert_eq!(invoke.form_for(false, 2).params.len(), 3);
        // Past every arity: the widest form, not nothing.
        assert_eq!(invoke.form_for(false, 9).params.len(), 3);
        // A turbofish `invoke` matches no form; the primary one still describes the intrinsic.
        assert_eq!(invoke.form_for(true, 0), invoke.primary_form());

        let construct = reflection_intrinsic("construct").expect("construct is an intrinsic");
        assert_eq!(construct.form_for(true, 0).turbofish, Some("T"));
        assert_eq!(construct.form_for(false, 0).params.len(), 2);
    }
}
