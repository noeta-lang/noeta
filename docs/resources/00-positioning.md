# Positioning & Identity

*Working title: the language is referred to here as **the language**. Name TBD — and now load-bearing (see below).*

This document fixes the language's identity and the sentence everything else keys off. It reflects the **clean-break decision**: this is a new language whose reason to exist is a capability combination, not familiarity with PHP. PHP resemblance is incidental.

---

## The positioning sentence

> **A language for shipping reactive applications as single binaries — web, desktop, or service — with a type system that makes illegal states unrepresentable.**

This is the sentence to repeat everywhere: README first line, first post, the pitch. It leads with what the language *does* (ship reactive apps as single binaries, any surface) and what keeps them correct (the type system), and says nothing about heritage — because heritage is not the reason to choose it.

### Shorter forms
- **One line:** *Reactive apps, any surface, one binary, fully typed.*
- **Tagline:** *Ship it as one binary. Keep it correct.*

---

## The identity paragraph

> Most languages make you choose. Go deploys as a single binary but has no story for reactive UIs and a thin type system. Elixir has reactivity (LiveView) but is dynamically typed and needs the BEAM deployed. TypeScript has the types and the reactivity but only on the frontend, and ships a runtime, not a binary. Rust has the types and the binary but is not where you reach to build an app quickly. This language sits where none of them do: a **persistent, reactive runtime** with **server-side signals** as a language primitive, an **ML-grade type system** (algebraic data types, `Result`-typed errors, exhaustive matching, real generics), and a compiler that emits a **single static binary** for **any surface** — a web server, a desktop app, or a CLI tool — from one codebase. The surface reads cleanly and will look broadly familiar to anyone coming from PHP, JavaScript, or similar; that familiarity is a convenience, not the point. The point is that the application you ship is reactive end to end, deploys as one artifact, and the type system has already ruled out a whole class of the bugs you would otherwise find in production.

---

## What the language is (and is not)

**Is:**
- A general-purpose, application-oriented language with a **persistent runtime** (not request-per-process).
- **Reactive at the language level** — `signal`/`computed`/`effect` drive UI (web and desktop) and, as an R&D direction, persistence.
- **Inferred-static typing** with an explicit `dyn` escape — signatures are required at named boundaries, bodies are inferred, and `dyn` is the one opt-in dynamic on-ramp; simple code stays simple, rigor composes upward.
- **Single-binary, any-surface** — CLI, web (bundled server), desktop (Tauri shell), plus shared-logic WASM for an existing JS/TS frontend.
- Implemented in **Rust**, built **agentically**, with an executable conformance spec.

**Is not:**
- A PHP runtime. It does not run PHP, Composer, or Laravel, and never will.
- A "better PHP." PHP informed the design (often as a cautionary example); it is not the target audience or the pitch.
- A frontend framework, a backend framework, or a webview wrapper — it is the *language and runtime* those could be built on.
- A research language. The novel parts (server-side reactivity, reactive persistence) are bets layered on a deliberately conventional, shippable core.
- A systems language. Embedded, bare-metal, no-std, and hard-real-time / zero-allocation workloads are out of scope (Rust/C/Zig territory, reachable *from* this language as native extensions). Mobile is *not* a 1.0 target but is deliberately not foreclosed — the design stays mobile-reachable for later (architecture §11.5).

---

## Who it is for

Not "PHP developers looking to switch." The audience is defined by the job, not the background:

- People who want to **ship a reactive app and deploy it as one artifact** without the operational stack (no nginx/FPM/Supervisor/separate runtime).
- People who want **one language across surfaces** — the same code model for a web app, its desktop build, and the CLI tooling around it.
- People who want **type-driven correctness** (make-illegal-states-unrepresentable) without adopting Rust's manual-memory learning curve.

PHP, JavaScript, Python, and Ruby developers will find the surface approachable — a useful on-ramp, not the reason to come.

---

## The reason-to-exist, stated plainly

The capability combination is the moat, and no incumbent occupies it:

| | Single binary | Reactive (built-in) | Strong static types | App-quick | Any surface |
|---|---|---|---|---|---|
| Go | ✓ | ✗ | partial | ✓ | partial |
| Elixir/Phoenix | ✗ (BEAM) | ✓ | ✗ | ✓ | ✗ |
| TypeScript/Node | ✗ (runtime) | frontend only | ✓ | ✓ | partial |
| Rust | ✓ | ✗ | ✓ | ✗ | ✓ |
| **This language** | ✓ | ✓ | ✓ | ✓ | ✓ |

The pitch is not that any single column is unique — it is that **the full row is**. The job to win first is the one where that full row is obviously required and the incumbents structurally are not: most likely a **live web app shipped as one binary**, the cheapest demo off the Rust stack and the most striking to see.

---

## Why the name now matters more

While PHP-familiarity was the (former) thesis, a PHP-adjacent name would have helped. With the clean break, a PHP-adjacent name would **mislead** about the audience and the capability. The name should:
- Point at the **capability identity** (reactive, single-binary, any-surface, safe) or be cleanly neutral and ownable.
- Carry **no "this is a PHP thing"** connotation.
- Fit the established naming aesthetic (meaningful roots, deliberate styling) — applied to what the language *is*, not where it came from.

The name is now a near-term unblock: it gates everything social (community, announcement, `init` of the repo) and should be decided deliberately in its own pass.

---

## How to talk about PHP (when it comes up)

It will come up because of the surface resemblance. The honest, on-thesis framing:

> "PHP influenced the look and several design decisions — sometimes by showing what to avoid. But this is a new language with its own runtime and type system; it does not run PHP code. If the syntax feels familiar, good — that lowers the cost of trying it. The reason to use it is the reactivity, the single-binary deployment, and the type system, not the resemblance."

Avoid: "PHP but better," "the PHP successor," "modern PHP." All three re-anchor to the heritage you deliberately broke from and set the wrong expectation (that it runs PHP code).
