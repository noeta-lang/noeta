# Conventions

Noeta enforces little about names. A type may be spelled `user`, a constructor may be called `make`, a module may sit in `MyUtils.noe`, and all of it compiles without a warning. This page is the agreement the standard library and the first-party packages follow, preceded by the short list of naming rules the compiler does enforce.

## What the compiler enforces

| Rule | Why | Diagnostic |
|---|---|---|
| Every module-path segment — each directory name and the file stem — lexes as exactly one identifier | every segment is spelled out in somebody's `use` | [E0074](Modules#naming-a-file) |
| Two files may not derive the same module path | one module path is one module | [E0073](Modules#one-path-one-module) |
| A module's path is derived from where the file sits and cannot be declared | a declaration could only restate the derivation or contradict it | [E0072](Modules#a-modules-path-cannot-be-declared) |
| A reserved word is not a name — not a binding, parameter, field, type, or function | each one already means something everywhere it appears | [E0046](Syntax-Basics#reserved-words) |
| A package name is `company/package`, each half a registry name: letters, digits, `_`, and `-` between them | an identity is a coordinate a forge and a URL path accept, not a token anybody lexes | manifest error |
| A package whose `package` half is not an identifier declares a `[package] root` | the root is the segment a consumer's modules derive under, so it has to lex | manifest error |
| Manifest `keywords` are 1–20 characters of lowercase `a–z`, `0–9` and `-`, starting alphanumeric, at most five of them | one canonical form per tag, so a registry can group by it | [manifest error](Manifest) |

Everything below that table is convention. Nothing lints it, `noeta fmt` never renames anything, and a program that ignores all of it type-checks and runs exactly the same.

## Case

| What | Case | In the wild |
|---|---|---|
| Types — `struct`, `class`, `enum`, `trait` | `PascalCase` | `User`, `HttpError`, `Uuid`, `Display` |
| Enum cases | `PascalCase` | `Status.Pending`, `OrderError.NegativePrice` |
| Attributes (`#[…]`) | `PascalCase`, because an attribute *is* a struct | `#[Route]`, `#[Skip]`, `#[Doc]` |
| Generic parameters | a single capital: `T`, then `K`/`V`, `E` for an error | `Map<K, V>`, `Result<T, E>` |
| Functions, methods, fields, parameters, bindings | `snake_case` | `next_id`, `read_line_async`, `max_age` |
| Directives and tiers (`@…`) | lowercase, one word | `@test`, `@doc`, `@packed`, `@json` |
| Module files and directories | lowercase, one word | `models.noe`, `query.noe`, `middleware.noe` |
| Package names | lowercase `company/package` | `para/db`, `local/hello` |

**The two sigils are grammar.** `@…` means the compiler generates or registers something; `#[…]` means inert data is attached to a declaration. [That split is how the language is built](Attributes-and-Reflection). Convention decides only the case inside them: a directive reads like a keyword because it names something the toolchain does, and an attribute is capitalized because it is a struct you declared.

**Acronyms are words.** Write `Uuid`, `Sse`, `Ndjson`, `HttpError`. `HttpsUrlParser` has three readable boundaries and `HTTPSURLParser` has none. Inside `snake_case` an acronym is another lowercase word: `to_hex`, `from_unix_ms`.

**Built-in types are lowercase, and that is the one place lowercase carries meaning.** `int`, `float`, `f32`, `f64`, `bool`, `string`, `bytes`, `void`, `dyn`, `never` and `number` come from the language; a capitalized type name means somebody declared it.

The generic built-in constructors `List`, `Map`, `Set`, `Option` and `Result` are capitalized because they build types the same way a declaration does. In a signature prefer the argument-taking form `List<int>` over the bare `list`, which leaves the element type to inference. Where the language accepts two spellings, write the first: `void` over `unit`, `dyn` over `Any`.

**There is no `SCREAMING_CASE`.** A top-level binding is immutable unless you write `mut`, so `max_retries = 3` is a constant and looks like every other binding.

A leading capital is also **load-bearing for tooling**. The TextMate grammar and the docs highlighter color a name as a type without running the compiler, and a leading capital is the only signal a static grammar has. A lowercase type name compiles and runs, and never looks like a type anywhere it is read.

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

A constructor is a convention rather than a keyword. It is a static function, one with no `self`, that returns an instance of its own type, and the ecosystem calls the primary one `new`:

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

The compiler cares about the shape rather than the name. A function is a *provable constructor* when every `return` hands back a freshly built literal of the enclosing type, and that is what lets the call site stamp the type tag `type_name::<T>()` reads. Any other body is a factory, and it wants the instantiation named at the call: `Repo::<Todo>.new(…)`, or an annotated binding. See [Attributes & Reflection](Attributes-and-Reflection).

Beyond `new`, the name follows what the function does with its input. `from_x` builds from another representation (`from_unix_ms`), and `with_x` returns a copy with one thing changed, which is what makes builder chains read: `cookie.with_path("/").with_secure(true)`.

## The method vocabulary

The affix tells you the shape of the answer before you look the signature up.

| Affix | What it promises | Examples |
|---|---|---|
| `is_` / `has_` | a predicate, returning `bool` | `is_dir`, `is_match`, `is_before` |
| `to_x` | converts to another representation, leaving the receiver alone | `to_string`, `to_hex`, `to_instant` |
| `from_x` | the associated twin of `to_x`: builds one from another representation | `from_unix_ms` |
| `with_x` | returns a copy with `x` set | `with_header`, `with_max_age` |
| `try_x` | the **recoverable door**: returns a `Result` where the bare `x` aborts | `try_parse`, `try_spawn`, `try_compile` |
| `x_async` | the awaitable twin of a blocking `x`, same semantics and same cursor | `read_line_async`, `wait_async` |
| `x_all` | the bulk form of `x` | `add_all`, `find_all`, `replace_all` |

`try_` carries real rules rather than a habit. It exists wherever the same failure is a bug in one program and an ordinary condition in another, and choosing between the two is [documented in full](Error-Handling#aborting-and-recoverable-doors). Do not invent a `try_` twin for something that cannot fail, and do not name a fallible function `try_` unless the aborting door exists too.

Two affixes are absent. A field read is `u.name` and a computed one is `u.total()`, so there are no `get_`/`set_` field accessors. And `as` is already the language's own checked narrowing (`x.as<Post>()`), so nothing is named `as_`.

## Files and layout

`src/` is a convention. A leading `src/` is never a module-path segment, so `src/human.noe` and `human.noe` are both `<package>.human`, and moving your sources under `src/` never renames your API. Use it, and put the entry file at `src/main.noe`.

A module file is a lowercase single word: `models.noe`, `query.noe`, `middleware.noe`. The compiler preserves case verbatim, so `Helpers/URI.noe` really is the module `Helpers.URI`, and an import path that mixes cases makes every `use` a spelling test. See [Modules](Modules#naming-a-file).

Tests live beside the code they test, in a `@test` block, and a test function is named for the behavior it pins rather than for the function it calls: `greets_by_name`, not `test_greet_2`. Benchmarks work the same way with `@bench`. An example application is its own package, at `examples/<app>/noeta.toml`.

A `@doc` block goes immediately above the declaration it documents, and its first line is a one-sentence summary. That sentence is what hover shows and what `noeta doc` lists, so write it as a statement about what the thing does.

## If you disagree with any of it

The cost of breaking a convention is paid in reading. Break one deliberately where the domain demands it: a protocol type spelled `NDJSON` everywhere in its own spec is an argument worth having. Breaking one by accident, in a package other people import, turns every wrong-looking name into something a reader has to memorize.
