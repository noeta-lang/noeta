# LiveView — server-side reactive HTML

`@html` is **server-side reactive HTML templating**, LiveWire/Phoenix-LiveView-style, built entirely from language features — no client framework. It composes three pieces you already have:

- **[Expression tiers](Documentation-and-Tiers#expression-tiers--embedded-languages-as-values)** — `@html { … ${expr} … }` is a typed template *value*: verbatim HTML with `${…}` holes that are real, type-checked Noeta expressions.
- **[Reactivity](Reactivity)** — each hole becomes a `computed`, so a `signal` change recomputes *exactly* the affected holes, glitch-free.
- **The reactive-view diff-push transport** (`std.reactive.view` + `std.http.server` websockets) — which serializes only the holes that changed and pushes a minimal patch to the browser.

It ships as the **`liveview` package** (`examples/liveview/`): the `Html` type, the `@html` handler, and a `handle(req, …)` that wires up the page, the client shim, and the websocket session. A consumer depends on it, imports `render` (which brings the `@html` tier into scope) and `handle`, and writes templates over signals. The runnable apps are `examples/liveview-counter/` and `examples/liveview-todos/`.

## The counter

```noeta ignore
use liveview.{render, Html, handle}
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

or factored into a named function when the row is non-trivial (e.g. a conditional over two templates — see "attribute holes" below):

```noeta ignore
fn row(t: Todo): Html {
    if t.done { return @html { <li class="done">[x] ${t.title}</li> } }
    return @html { <li class="todo">[ ] ${t.title}</li> }
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

`examples/liveview-todos/` is a full example — a loop of nested rows, a computed count, a conditional status line, escaped text, and a "complete all" event. One `signal` update pushes a minimal diff of exactly the holes that changed (the list, the count, the status) and leaves the unchanged total alone.

## Events

The bundled client turns a `data-live-click="name"` into an event; the app's `on_event(name)` handles it (typically a `signal.update`). That's the whole client→server surface for v1 — enough for buttons and actions. State lives in signals on the server; the browser is a thin view.

## Native and pure-Noeta handlers

`@html`'s handler is pure Noeta (it composes `std.reactive`). The same `@html` mechanism also supports a **native** handler — see [expression tiers](Documentation-and-Tiers#native-rust-package-expression-tiers), where std's `@json` is a native example — but a *reactive* template composes signals most naturally in Noeta.

## Current limitations (v1)

- **Attribute-position holes** are wrapped in a `<span>` rather than inlined, so `class="${…}"` breaks. Use conditional template *branches* for dynamic attributes (as `row` above does). Inlining attribute holes is a planned refinement.
- **Nested holes** re-render with their parent region, not independently: a change inside a loop re-renders the whole list (via `innerHTML`), not one row. Per-row keyed reactivity is a planned refinement.
- **Single-worker**: signals are per-isolate, so a LiveView app runs single-worker (`--parallel` documents this).

## See also

- [Reactivity](Reactivity) — `signal`/`computed`/`effect`, the engine underneath.
- [Documentation & Dev Tiers](Documentation-and-Tiers#expression-tiers--embedded-languages-as-values) — expression tiers, the `@html` mechanism.
- [The `noeta` CLI](The-CLI#noeta-serve-and---watch) — `noeta serve` and `--watch` (hot reload keeps signal state across edits).
