# noeta-graphbench

How well a composition of `noeta mcp` tool calls answers a graph question about a Noeta codebase.

```console
$ cargo run -p noeta-graphbench
graphbench: corpus
  orders_service      12 files  linked=true  errors=0  warnings=0
  ...
arm  category          n      P      R     F1  Acc@1  Acc@5    MRR   tokens  calls      ms  evidence
A0   callees          26  1.000  1.000  1.000  0.846  0.846  0.846     1300    2.9    4081  trace
A5   callees          26  0.243  0.885  0.350  0.192  0.423  0.332     8724   21.4       0  file_read

graphbench: every row holds its baseline.
```

The run exits non-zero when a row's F1 falls below its floor or its token size rises above its ceiling, so it gates at the merge tier. A filtered run says so and compares nothing.

## What it measures

Each **arm** is a fixed strategy: for one question category, a composition of tool calls an agent would plausibly make. Arms run against the real `NoetaMcp` service over an in-process duplex, so every answer travels the wire an agent sees.

| Arm | What it has | What it isolates |
|---|---|---|
| A0 | `symbols`, `definition`, `references`, `trace`, `module_graph`, `reflect`, file reads | the floor a real agent works from |
| A1 | A0 plus `code_search` | seed selection |
| A2 | A1 plus `context_map` | budgeted ranking |
| A3 | A2 plus `path`, `impact`, `architecture`, `callers` | multi-hop and role structure |
| A4 | a fixed 4k-token repo map, no navigation | whether precomputation alone suffices |
| A5 | a lexical scan of the files, no Noeta tools | whether any of this beats grep |

An arm whose tools the service does not advertise reports **SKIP** naming the missing tool. A SKIP prints in its own column, holds no baseline row, and is never a pass.

An arm is given what a developer types: a leaf name and the file it sits in, a module path, or a sentence. It never sees the question's gold, and it never sees the qualified name the graph knows a declaration by, because obtaining that name is part of what is measured.

## Question categories

| Category | Form | Where gold comes from | Metric |
|---|---|---|---|
| `definition` | "Where is `X` declared?" | the declaration universe | Acc@k, MRR, F1 |
| `callers` | "Which declarations call `X`?" | reverse call-graph edges | set P/R/F1 |
| `callees` | "What does `X` reach within 3 hops?" | the forward closure | set P/R/F1 |
| `importers` | "Which files import module `M`?" | the linker's `use` edges | set P/R/F1 |
| `role_reach` | "Which role-bearing declarations does this entry point reach?" | the trace engine's boundary set | set P/R/F1 |
| `path` | "How does `A` reach `B`?" | the shortest call path | set P/R/F1 over the path's nodes |
| `impact` | "If `X` changes, which `@test` functions must run again?" | the reverse closure, restricted to tests | set P/R/F1 |
| `seed_mapping` | a sentence naming no identifier | hand-labeled | Acc@k, MRR, F1 |

Every row also records the output size in tokens (characters over four), the number of logical tool calls, the wall time, and which tool supplied the evidence. Wall time is recorded and gated nowhere: this machine carries several concurrent builds, so a millisecond column measures the afternoon.

A ranked category's F1 is dominated by the candidate depth an arm returns, since ten candidates against one gold answer cap precision at 0.1. Read `Acc@1` and MRR there, and read F1 on the set categories.

Responses are cached per (tool, arguments) within a run, because every MCP call builds a fresh `LangDatabase` and re-links the project. The **logical** call count is still what the arm issued, so the calls column stays the number an agent would pay; the milliseconds column is what the cache left.

## Sampling rules

A structural question is emitted only when it needs two files, or two hops, or turns on a leaf name that names more than one declaration, or ends at a labeled `external`/`dynamic` leaf. A question a single `grep` answers measures nothing.

Every category also carries **negatives**, where the empty set is right: a function nothing calls, a module nothing imports, an entry point that reaches no boundary, a pair with no path between them. Answering nothing scores 1.0 there, and answering confidently scores 0.

## The circularity, stated plainly

Gold comes from the compiler's own indices: `noeta_ide::callgraph::build`, the reflection role index, the linker's module paths. Both sides therefore read one graph.

That bounds what the benchmark can find. It measures **retrieval, ranking, and mapping a question onto a node** — whether a tool composition can reach an answer the graph already holds. It cannot measure whether the graph is right, and a question here can never fail for a reason the conformance corpus should have caught. Graph correctness stays the conformance corpus's job.

## The corpus

`tests/graphbench/corpus/` holds four projects, each a package with a `noeta.toml` and an entry file, none of them depending on an external package. The harness asserts every one of them links and checks with zero errors before it scores anything: a project that stopped linking would degrade every graph tool to the entry file's own AST, and the arms would score against a program the compiler never built.

| Project | Shape |
|---|---|
| `orders_service` | an HTTP order service with a repository layer and a notifier |
| `task_cli` | a task CLI with subcommands, a config store and a dispatch registry |
| `feed_pipeline` | an ingest pipeline of parsers, checkers and publishers |
| `shipped_examples` | a pinned copy of the repository's own `examples/` tree |

Between them they carry all five `Semantic` roles, traits with default methods, both in-body and standalone `impl Trait for T`, generics over a bound, closures held in fields, named functions passed as values, `Type.new()` construction, stdlib calls on typed receivers, `@test` blocks declaring their own structs and helpers, nested `fn`s, subdirectory modules, colliding leaf names, functions nothing calls, and roles nothing reaches.

**To add a project**: write the directory under `tests/graphbench/corpus/`, add a row to `corpus/index.json` naming it and its entry file, write `tests/graphbench/questions/<project>.json`, and re-record the baseline.

**To add a seed question**: append to `tests/graphbench/questions/<project>.json`. Each seed is a natural-language query and the declarations that answer it, best first, labeled `file#name`:

```json
{ "query": "where does an order get persisted?", "answers": ["store.noe#save_order", "store.noe#append"] }
```

A label naming no declaration, or an ambiguous bare name, fails the run rather than scoring zero. `--dump-gold <project>` prints the universe a label can name.

## The synthetic tier

`--synthetic 1000 --synthetic 10000` writes projects of that many declarations across N/100 modules, beside a manifest of every call, reference and import edge the generator emitted. The call structure is acyclic by construction, so the manifest is the whole truth about the graph.

These rows are a scaling and latency observation. They are gated nowhere, and accuracy is measured on the hand-written corpus only.

## The baseline

`tests/graphbench/baseline.txt` carries one row per (arm, category) with an F1 floor and a token ceiling. `--record` rewrites it. The file's own header carries the tolerances and why they are what they are: F1 may fall by half of its last printed digit, tokens may not rise at all, because nothing in this measurement varies.

Expect to re-record whenever the corpus, the questions or a tool's wire shape changes, and read the diff's sign before committing it.

## Ablating the benchmark

```console
$ cargo run -p noeta-graphbench -- --ablate all
```

Each pass stubs one tool to its empty answer and re-runs the arms, reporting which (arm, category) rows fell. A category no ablation moves is a category whose questions are not reaching the tools they name, and `--ablate all` exits non-zero when it finds one.

The summary separates two verdicts. A category reached by stubbing a Noeta graph tool is one the graph surface really answers. A category reached only by stubbing the file read is one where the lexical arm carries the score and the graph tools have nothing an ablation can take away, which is a finding about the tools rather than about the benchmark.

## The agent layer

`scripts/graphbench-agent.sh` runs the same questions through a real model with `noeta mcp` as its only tool server, one question per invocation, with a per-arm tool allowlist. It scores structural categories by exact set comparison against the same gold, and reports tool calls, tokens and cost per answer.

It is nightly-only and never gated. An answer costs money, and the same question asked twice can come back differently.

## Reading the code

| File | What lives there |
|---|---|
| `src/corpus.rs` | the roster, the salsa workspace per project, the link gate |
| `src/gold.rs` | the declaration universe and every structural answer |
| `src/question.rs` | the categories, the sampling rules, the seed-question loader |
| `src/adapter.rs` | **the one module that reads MCP tool output** |
| `src/service.rs` | the duplex MCP client, the response cache, the ablation switch |
| `src/arms.rs` | the strategies |
| `src/metrics.rs` | resolution and scoring |
| `src/baseline.rs` | the floors and ceilings |
| `src/generate.rs` | the synthetic tier |

`src/adapter.rs` is the boundary that matters. Every reader there prefers a node's `id` object when a tool emits one, and falls back to today's per-tool fields when it does not, so re-targeting the benchmark to a new wire shape is an edit in one file.
