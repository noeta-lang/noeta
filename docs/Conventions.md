# Conventions

Noeta enforces less about names than it could. A type may be spelled `user`, a constructor may be called `make`, a module may sit in `MyUtils.noe` — all of it compiles, and none of it draws a warning. What keeps an ecosystem legible at that point is agreement, so this page is the agreement: the handful of naming rules the compiler *does* enforce, and then the conventions that everything in the standard library and the first-party packages follows.

The split matters, so it comes first.

## What the compiler enforces

| Rule | Why | Diagnostic |
|---|---|---|
| Every module-path segment — each directory name and the file stem — lexes as exactly one identifier | every segment is spelled out in somebody's `use` | [E0074](Modules#naming-a-file) |
| Two files may not derive the same module path | one module path is one module | [E0073](Modules#one-path-one-module) |
| A module's path is derived from where the file sits and cannot be declared | a declaration could only restate the derivation or contradict it | [E0072](Modules#a-modules-path-cannot-be-declared) |
| A reserved word is not a name — not a binding, parameter, field, type, or function | each one already means something everywhere it appears | [E0046](Syntax-Basics#reserved-words) |
| A package name is `company/package`, each half an identifier | the `package` half is the root segment a consumer's modules derive under | manifest error |
| Manifest `keywords` are 1–20 characters of lowercase `a–z`, `0–9`, `-` | one canonical form per tag, so a registry can group by it | [manifest error](Manifest) |

Everything below that table is convention. Nothing lints it, `noeta fmt` never renames anything, and a program that ignores all of it type-checks and runs exactly the same.

That the compiler declines to fix a name for you is deliberate, and it is the same argument E0074's help text makes about module paths: silently mapping `my-utils` to `my_utils` would give one module two spellings, and the point of deriving a name is that there is only ever one.

## Case

| What | Case | In the wild |
|---|---|---|
| Types — `struct`, `class`, `enum`, `trait` | `PascalCase` | `User`, `HttpError`, `Uuid`, `Display` |
| Enum cases | `PascalCase` | `Status.Pending`, `OrderError.NegativePrice` |
| Attributes (`#[…]`) | `PascalCase` — an attribute *is* a struct | `#[Route]`, `#[Skip]`, `#[Doc]` |
| Generic parameters | a single capital: `T`, then `K`/`V`, `E` for an error | `Map<K, V>`, `Result<T, E>` |
| Functions, methods, fields, parameters, bindings | `snake_case` | `next_id`, `read_line_async`, `max_age` |
| Directives and tiers (`@…`) | lowercase, one word | `@test`, `@doc`, `@packed`, `@json` |
| Module files and directories | lowercase, one word | `models.noe`, `query.noe`, `middleware.noe` |
| Package names | lowercase `company/package` | `para/db`, `local/hello` |

**The two sigils are grammar, not convention.** `@…` means the compiler generates or registers something and `#[…]` means inert data is attached to a declaration — [that split is how the language is built](Attributes-and-Reflection), and none of it is yours to choose. What convention decides is the *case inside* them: a directive names something the toolchain does, so it reads like a keyword and is lowercase, while an attribute is a struct you declared and is capitalized like every other type.

**Acronyms are words, not shouts.** `Uuid`, `Sse`, `Ndjson`, `HttpError` — not `UUID`, `SSE`, `HTTPError`. The reason shows up the moment two of them meet: `HttpsUrlParser` has three readable boundaries and `HTTPSURLParser` has none. The same rule applies inside `snake_case`, where the acronym is simply another lowercase word: `to_hex`, `from_unix_ms`.

**Built-in types are lowercase, and that is the one place lowercase carries meaning.** `int`, `float`, `f32`, `f64`, `bool`, `string`, `bytes`, `void`, `dyn`, `never`, `number` are handed to you by the language; a capitalized type name means somebody declared it. The generic built-in constructors — `List`, `Map`, `Set`, `Option`, `Result` — are capitalized because they build types the same way a declaration does. `list`, `map` and `set` also lex as bare spellings that leave their element types unstated for inference to fill; prefer the canonical argument-taking form when you are writing a signature, where the whole point is to say what is in there. Where the language accepts two spellings of one thing, the first is the one to write: `void` over `unit`, `dyn` over `Any`.

**There is no `SCREAMING_CASE`.** A top-level binding is immutable unless you write `mut`, so "a constant" is not a separate category of thing that needs a separate shape of name — `max_retries = 3` is a constant, and it looks like every other binding.

**The case of a type is load-bearing for tooling, not just for taste.** The TextMate grammar and the docs/snippet highlighter both color a leading capital as a type without running the compiler, because that is the only signal available to a static grammar; the editor surfaces resolve a name to a type before a value when a program has both. A lowercase type name compiles and runs — it just never looks like a type anywhere it is read.

A small program with all of it in place:

```noeta
enum Status {
    Draft
    Published
}

struct Post {
    title: string
    status: Status
}

fn is_public(post: Post): bool {
    return post.status == Status.Published
}

posts = [
    Post { title: "hello", status: Status.Published },
    Post { title: "wip", status: Status.Draft },
]
echo posts.filter(fn(p) => is_public(p)).len()
```

## Constructors

**A constructor is a convention, not a keyword.** It is an associated function — one with no `self` — that returns an instance of its own type, and the ecosystem calls the primary one `new`:

```noeta
struct Todo {
    title: string
}

class Repo<T> {
    table: string

    fn new(table: string): Repo<T> {
        return Repo { table: table }
    }

    fn describe(): string {
        return "a repo over ${self.table} of ${type_name::<T>()}"
    }
}

repo: Repo<Todo> = Repo.new("todos")
echo repo.describe()
```

The compiler does care about one thing here, and it is the **shape, not the name**: a function qualifies as a *provable constructor* when it has at least one `return` and every one of them hands back a freshly built literal of the enclosing type. That is what makes the last two lines above work. Inside `new`, `T` is still a parameter with no instantiation to record — the instantiation is known one frame up, at `repo: Repo<Todo> = Repo.new("todos")`, so the **call site** stamps the type tag that `type_name::<T>()` later reads.

The proof has to be syntactic, because a function that hands back a cached or borrowed instance would let two differently-instantiated call sites re-tag one object and silently rewrite each other's answer. So a body that returns a local, delegates to another call, or wraps itself in a generator or `async` is not that shape; it is a perfectly good factory, it just leaves the instantiation to ordinary inference, which will ask for it at the call (`Repo::<Todo>.new(…)`, or an annotated binding) rather than guess. See [Attributes & Reflection](Attributes-and-Reflection) for what the tag buys you.

Beyond `new`, the naming follows what the function does with its input: `from_x` builds from another representation (`from_unix_ms`), and `with_x` returns a copy with one thing changed, which is what makes builder chains read (`cookie.with_path("/").with_secure(true)`). A constructor that can fail is a door, which is the next section.

## The method vocabulary

Standard-library names are predictable on purpose — the affix tells you the shape of the answer before you look the signature up.

| Affix | What it promises | Examples |
|---|---|---|
| `is_` / `has_` | a predicate, returning `bool` | `is_dir`, `is_match`, `is_before` |
| `to_x` | converts to another representation, leaving the receiver alone | `to_string`, `to_hex`, `to_instant` |
| `from_x` | the associated twin of `to_x`: builds one from another representation | `from_unix_ms`, `from_bytes` |
| `with_x` | returns a copy with `x` set | `with_header`, `with_max_age` |
| `try_x` | the **recoverable door**: returns a `Result` where the bare `x` aborts | `try_parse`, `try_spawn`, `try_compile` |
| `x_async` | the awaitable twin of a blocking `x`, same semantics and same cursor | `read_line_async`, `wait_async` |
| `x_all` | the bulk form of `x` | `add_all`, `find_all`, `replace_all` |

The `try_` pair is the one with real rules behind it rather than a habit: it exists wherever the same failure is a bug in one program and an ordinary condition in another, and choosing between the two is [documented in full](Error-Handling#aborting-and-recoverable-doors). Do not invent a `try_` twin for something that cannot fail, and do not name a fallible function `try_` unless the aborting door exists too.

Two affixes are conspicuously **absent**. There are no `get_`/`set_` accessors: a field read is `u.name` and a computed one is `u.total()`, so the prefix only adds a word to every call. And nothing is named `as_`, because `as` is already the language's own checked narrowing (`x.as<Post>()`) — a method borrowing the word would read like a cast that isn't one.

## Files and layout

`src/` is a convention, not a rule: a leading `src/` is never a module-path segment, so `src/human.noe` and `human.noe` are both `<package>.human` and moving your sources under `src/` never renames your API. Use it, and put the entry file at `src/main.noe`.

A module file is a lowercase single word — `models.noe`, `query.noe`, `middleware.noe`. The compiler preserves case verbatim (`Helpers/URI.noe` really is the module `Helpers.URI`), which is exactly why the convention is worth keeping: an import path that mixes cases makes every `use` a spelling test. See [Modules](Modules#naming-a-file).

Tests live beside the code they test, in a `@test` block, and test functions are named for the behavior they pin rather than for the function they call — `greets_by_name`, not `test_greet_2`. Benchmarks work the same way with `@bench`. An example application is its own package: `examples/<app>/noeta.toml`, which is one of the three directory kinds the module walk deliberately stops at.

A `@doc` block goes immediately above the declaration it documents, and its first line is a one-sentence summary — that sentence is what hover shows and what `noeta doc` lists, so write it as a statement about what the thing does.

## If you disagree with any of it

These are conventions, which means the cost of breaking one is paid in reading, not in compiling. Break them deliberately where the domain demands it — a protocol type that is genuinely called `NDJSON` everywhere in its spec is a defensible `Ndjson`-versus-`NDJSON` argument to have. The one thing worth avoiding is breaking them by accident, in a package other people import, where every wrong-looking name becomes something a reader has to memorize.
