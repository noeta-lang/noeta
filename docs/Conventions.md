# Conventions

Noeta enforces little about names: a type may be spelled `user`, a constructor may be called `make`, a module may sit in `MyUtils.noe`, and all of it compiles without a warning. What keeps an ecosystem legible at that point is agreement. This page is the agreement — the naming rules the compiler *does* enforce, then the conventions the standard library and the first-party packages follow.

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

The compiler declines to *fix* a name for you on purpose: silently mapping `my-utils` to `my_utils` would give one module two spellings, and the point of deriving a name is that there is only ever one.

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

**The two sigils are grammar, not convention.** `@…` means the compiler generates or registers something; `#[…]` means inert data is attached to a declaration — [that split is how the language is built](Attributes-and-Reflection). Convention decides only the *case inside* them: a directive names something the toolchain does, so it reads like a keyword and is lowercase; an attribute is a struct you declared, so it is capitalized like every other type.

**Acronyms are words, not shouts.** `Uuid`, `Sse`, `Ndjson`, `HttpError` — not `UUID`, `SSE`, `HTTPError`. `HttpsUrlParser` has three readable boundaries and `HTTPSURLParser` has none. Inside `snake_case` an acronym is simply another lowercase word: `to_hex`, `from_unix_ms`.

**Built-in types are lowercase, and that is the one place lowercase carries meaning.** `int`, `float`, `f32`, `f64`, `bool`, `string`, `bytes`, `void`, `dyn`, `never` and `number` come from the language; a capitalized type name means somebody declared it. The generic built-in constructors — `List`, `Map`, `Set`, `Option`, `Result` — are capitalized because they build types the same way a declaration does. In a signature prefer the argument-taking form (`List<int>`) over the bare `list`, which leaves the element type to inference. Where the language accepts two spellings, write the first: `void` over `unit`, `dyn` over `Any`.

**There is no `SCREAMING_CASE`.** A top-level binding is immutable unless you write `mut`, so "a constant" is not a separate category needing a separate shape of name — `max_retries = 3` is a constant, and it looks like every other binding.

A leading capital is also **load-bearing for tooling**: the TextMate grammar and the docs highlighter color it as a type without running the compiler, because that is the only signal a static grammar has. A lowercase type name compiles and runs — it just never looks like a type anywhere it is read.

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

**A constructor is a convention, not a keyword.** It is a static function — one with no `self` — that returns an instance of its own type, and the ecosystem calls the primary one `new`:

```noeta
struct Todo {
    title: string
}

class Repo<T> {
    table: string

    pub fn new(table: string): Repo<T> {
        return Repo { table: table }
    }

    pub fn describe(): string {
        return "a repo over ${self.table} of ${type_name::<T>()}"
    }
}

repo: Repo<Todo> = Repo.new("todos")
echo repo.describe()
```

The compiler cares about the **shape, not the name**: a function is a *provable constructor* when every `return` hands back a freshly built literal of the enclosing type, and that is what lets the call site stamp the type tag `type_name::<T>()` reads. Any other body is a perfectly good factory; it just wants the instantiation named at the call (`Repo::<Todo>.new(…)`, or an annotated binding). See [Attributes & Reflection](Attributes-and-Reflection).

Beyond `new`, the name follows what the function does with its input: `from_x` builds from another representation (`from_unix_ms`), and `with_x` returns a copy with one thing changed, which is what makes builder chains read (`cookie.with_path("/").with_secure(true)`).

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

`try_` is the one with real rules behind it rather than a habit: it exists wherever the same failure is a bug in one program and an ordinary condition in another, and choosing between the two is [documented in full](Error-Handling#aborting-and-recoverable-doors). Do not invent a `try_` twin for something that cannot fail, and do not name a fallible function `try_` unless the aborting door exists too.

Two affixes are conspicuously **absent**. There are no `get_`/`set_` accessors: a field read is `u.name` and a computed one is `u.total()`, so the prefix only adds a word to every call. And nothing is named `as_`, because `as` is already the language's own checked narrowing (`x.as<Post>()`) — a method borrowing the word would read like a cast that isn't one.

## Files and layout

`src/` is a convention, not a rule: a leading `src/` is never a module-path segment, so `src/human.noe` and `human.noe` are both `<package>.human`, and moving your sources under `src/` never renames your API. Use it, and put the entry file at `src/main.noe`.

A module file is a lowercase single word — `models.noe`, `query.noe`, `middleware.noe`. The compiler preserves case verbatim (`Helpers/URI.noe` really is the module `Helpers.URI`), which is exactly why the convention is worth keeping: an import path that mixes cases makes every `use` a spelling test. See [Modules](Modules#naming-a-file).

Tests live beside the code they test, in a `@test` block, and test functions are named for the behavior they pin rather than for the function they call — `greets_by_name`, not `test_greet_2`. Benchmarks work the same way with `@bench`. An example application is its own package: `examples/<app>/noeta.toml`.

A `@doc` block goes immediately above the declaration it documents, and its first line is a one-sentence summary — that sentence is what hover shows and what `noeta doc` lists, so write it as a statement about what the thing does.

## If you disagree with any of it

The cost of breaking a convention is paid in reading, not in compiling. Break one deliberately where the domain demands it — a protocol type genuinely spelled `NDJSON` everywhere in its own spec is an argument worth having. The thing to avoid is breaking them *by accident*, in a package other people import, where every wrong-looking name becomes something a reader has to memorize.
