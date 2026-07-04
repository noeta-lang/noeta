# lang-db

The query graph: the compile pipeline as a [salsa](https://github.com/salsa-rs/salsa) (0.27) database.

- **Takes in:** source text (the `SourceProgram` input — `id`/`name`/`text`).
- **Emits:** three memoized, pass-through tracked queries forming the dependency graph `tokens(db) → ast(db) → bytecode(db)`, each a thin wrapper over `lang_lexer::lex` / `lang_parser::parse` / `lang_compiler::compile`. The `LangDatabase` plus a `source_program(db, &Source)` constructor.

M1.1 threads the existing straight-line pipeline through salsa **before** the type checker needs it, so later slices edit a graph rather than rewrite a pipeline — the checker query (`checked_ast`, M1.7) slots between `ast` and `bytecode` with no re-threading. This slice is deliberately *behavior-preserving*: the differential oracle proves the wrap changes nothing (the VM still reproduces the tree-walker byte-for-byte).

salsa memoizes a tracked function's return value and needs it to be `Update` + `PartialEq`. The artifacts (`Lexed`/`Parsed`/`Module`) are foreign and implement neither, so each is wrapped in a local newtype (`Tokens`/`Ast`/`Bytecode`) given conservative "always-changed" impls: `PartialEq` returns `false` (salsa never backdates) and a hand-written `unsafe impl Update` overwrites the slot in place. Both are sound — salsa never serves a stale value; only backdating is forgone, which pass-through plumbing does not need. This 3-line always-replace is the crate's only `unsafe`, so (like `lang-value`/`lang-gc`/`lang-vm`) it opts out of the workspace `unsafe_code = "forbid"` and is `miri`-gated.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
