# LiveView — server-side reactive HTML

`@html` is **server-side reactive HTML templating**, LiveWire/Phoenix-LiveView-style, built entirely from language features — no client framework. It composes three pieces you already have:

- **[Expression tiers](Documentation-and-Tiers#expression-tiers--embedded-languages-as-values)** — `@html { … ${expr} … }` is a typed template *value*: verbatim HTML with `${…}` holes that are real, type-checked Noeta expressions.
- **[Reactivity](Reactivity)** — each hole becomes a `computed`, so a `signal` change recomputes *exactly* the affected holes, glitch-free.
- **The reactive-view diff-push transport** (`std.reactive.view` + `std.http.server` websockets) — which serializes only the holes that changed and pushes a minimal patch to the browser.

It ships as the **`para.html` package** (`packages/para-html/`): the `Html` type, the `@html` handler, and a `handle(req, …)` that wires up the page, the client shim, and the websocket session. It is a **first-party but non-default** package under the `para` ("alongside") namespace — maintained by the project, distributed through the package registry rather than baked into `std` (its sibling is the [`para.p2p`](Local-First-and-P2P) local-first package). A consumer keys the dependency `para`, imports `render` (which brings the `@html` tier into scope) and `handle`, and writes templates over signals:

```toml
[dependencies]
para = { path = "…/packages/para-html" }   # or a registry/git version once published
```

The runnable apps are `examples/para-html/liveview-counter/` and `examples/para-html/liveview-todos/`.

## The counter

```noeta ignore
use para.html.{render, Html, handle}
use std.reactive.signal
use std.http.server
use std.http.{Request, Response}

count = signal(0)

fn page(): Html {
    return @html {
        <h1>count: <b>${count.get()}</b>, doubled: <b>${count.get() * 2}</b></h1>
        <button data-live-click="inc">+1</button>
    }
}

fn on_event(name: string): void {
    if name == "inc" { count.update(fn(n) => n + 1) }
}

fn fetch(req: Request): Response {
    return handle(req, "Counter", page, on_event)
}
```

`noeta serve` it and open the page: clicking `+1` sends an event, the handler updates `count`, and the server pushes a minimal patch — `count` **and** its double update in place from one signal change (glitch-free). Both holes were compiled to `computed`s that read `count`, so both recompute; the transport pushes only what changed.

## The template language

A hole `${expr}` is an ordinary Noeta expression, checked in scope. What its **value** is decides how it renders:

- **A scalar** (`string`/`int`/`bool`/…) renders as **escaped text** — XSS-safe by default. `${user_input}` can never inject markup.
- **An `Html`** (a nested `@html { … }`) or a **`List<Html>`** (a loop, e.g. `${rows.map(row)}`) is embedded as **raw markup**. This is the JSX rule: `{child}` composes, `{text}` is escaped.

So a **loop is `.map` producing a `List<Html>`** — the JSX/React model, not a `v-for` directive. The loop body can be written **inline**, since a `${…}` hole may itself contain a nested `@html { … }`:

```noeta ignore
<ul>${items.map(fn(t) => @html { <li>${t.title}</li> })}</ul>
```

or factored into a named function when the row is non-trivial:

```noeta ignore
fn row(t: Todo): Html {
    // A hole in *attribute* position (`class="${…}"`) is inlined and escaped; a hole in
    // text position gets a reactive span.
    return @html { <li class="${if t.done then "done" else "todo"}">${t.title}</li> }
}

fn page(): Html {
    return @html {
        <h1>Todos — ${remaining()} of ${todos.get().len()} left</h1>
        <ul>${todos.get().map(row)}</ul>                       // a List<Html> loop
        <p>${if remaining() == 0 then "All done!" else "Keep going."}</p>
    }
}
```

There is **no `v-for` / template-directive syntax** — `@html` is lightweight interpolation, not a template compiler, so loops and conditionals are ordinary Noeta expressions (`.map`, `.filter`, `if…then…else`) over `Html` values. Nested `@html` bodies are verbatim text, **not** strings, so a `${…}` hole inside one may contain double quotes (`${if t.done then "done" else "todo"}`) with none of string interpolation's nested-quote limitation.

### Keyed lists — per-row reactivity and structural patching

A plain `${todos.get().map(row)}` loop is **one** reactive region: any change re-renders the whole list into a single `innerHTML`. For a large list where one item changes, that is a lot of wire traffic. `keyed` makes **each row its own reactive region**, so a change to one row pushes only that row's markup — the diff drops every row that is unchanged:

```noeta ignore
<ul>${keyed(todos, fn(t) => "${t.id}", row)}</ul>
```

`keyed(source, key_of, render_row)` takes the **signal itself** (not a snapshot), a stable string key per item, and the row renderer. Each row becomes a persistent `computed` that re-derives *its* item from the signal by key: on any change every row recomputes, but a row whose rendered markup is unchanged produces a value-equal string, so the transport sends nothing for it. Toggling one todo in a 1000-row list pushes exactly one row.

**Structural changes patch in place too.** When the *set* or *order* of keys changes — a row appended, prepended, removed, or reordered — the session reconciles the key sequence and pushes the **minimal** structural ops rather than re-rendering the parent:

- a **new** key renders alone and is inserted at its position (`insert-before`), carrying its markup inline;
- a **removed** key's row is torn down alone (`remove`) and its per-row reactive scope is reclaimed on the spot (`view.unexpose` disposes the row `computed`);
- a **reordered** key `move`s its existing DOM element — the row's content is *not* re-rendered, so its DOM identity, form state, and focus survive the reorder.

The diff is the standard keyed-children algorithm (Vue 3's `patchKeyedChildren`, the same shape as Solid's and Inferno's reconcile): surviving rows are anchored on their **longest increasing subsequence**, so a reorder moves the *fewest* rows — swapping two adjacent items is a single `move`, not two. Each row rides the reactive **owner tree**: it is an owned child scope keyed by its key, kept while its key stays in the set and disposed the moment the key leaves. Toggling, adding, removing, and reordering a 1000-row list each touch exactly the rows that changed.

Use `keyed` for any list a client edits — rows mutating in place *or* rows coming and going; a plain `.map` is fine for small or rarely-changing lists that render whole.

### Reactivity: read the signal *inside* the hole

A hole is reactive to exactly the signals its expression reads **when the hole evaluates**. Read the signal *inside* the hole:

```noeta ignore
<h1>${todos.get().len()} items</h1>        // reactive — the hole reads `todos`
```

not by pre-computing a local in the enclosing function:

```noeta ignore
n = todos.get()                            // read happens here, outside any hole
return @html { <h1>${n.len()} items</h1> } // NOT reactive — the hole captured a value
```

`examples/para-html/liveview-todos/` is a full example — a **keyed** loop of nested rows, a computed count, a conditional status line, escaped text, and a "complete all" event. Each row is its own reactive region (keyed by `t.id`), so toggling a single todo pushes exactly one row; the count and status update from the same signal read, and the unchanged rows and total are left alone. `examples/para-html/liveview-structural-app/` drives the structural side — buttons that append, prepend, remove, and reorder a keyed list, each pushing a lone insert / remove / move.

## Events

The bundled client turns a `data-live-click="name"` into an event; the app's `on_event(name)` handles it (typically a `signal.update`). That's the whole client→server surface for v1 — enough for buttons and actions. State lives in signals on the server; the browser is a thin view.

## Native and pure-Noeta handlers

`@html`'s handler is pure Noeta (it composes `std.reactive`). The same `@html` mechanism also supports a **native** handler — see [expression tiers](Documentation-and-Tiers#native-rust-package-expression-tiers), where std's `@json` is a native example — but a *reactive* template composes signals most naturally in Noeta.

A hole in **attribute position** (`class="${…}"`, `title="${…}"`) is detected from the preceding text and **inlined** (escaped, including the quote) rather than wrapped in a `<span>`. Its value re-renders with the enclosing region (a row), not on its own.

## Current limitations (v1)

- **Single-worker**: signals are per-isolate, so a LiveView app runs single-worker (`--parallel` documents this).

## See also

- [Reactivity](Reactivity) — `signal`/`computed`/`effect`, the engine underneath.
- [Documentation & Dev Tiers](Documentation-and-Tiers#expression-tiers--embedded-languages-as-values) — expression tiers, the `@html` mechanism.
- [The `noeta` CLI](The-CLI#noeta-serve-and---watch) — `noeta serve` and `--watch` (hot reload keeps signal state across edits).
