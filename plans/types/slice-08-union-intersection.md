# Slice S8 — Declared union / intersection types ("closed `dyn`")

Status: **planned** (design-only; no code yet)

> **Track:** inferred-static type system (see `plans/types/README.md`). **Gated after:** S6 (`dyn` narrowing). **Not** part of S3c. Recorded now so the door stays explicitly open; sequenced after the core inferred-static work and the `dyn` narrowing op it rides on.

## Why this is a separate, later stage — and why our engine choice does not foreclose it

The inferred-static engine is **bidirectional checking + local inference, never Hindley–Milner / MLsub** (`type-system-direction` memory). The thing that makes inference-under-subtyping exotic is the demand for **principal** types: the least solution of two conflicting lower bounds (`int <: ?` and `string <: ?`) is the union `int | string`, so a principality-seeking inferencer is *forced* to manufacture union/intersection types. We sidestep that entirely because we have `dyn` as a join-of-last-resort — when constraints conflict, S3c.3's solver joins to `dyn`, never to a union.

The crucial distinction is **declared vs. inferred**:

- **Inferred** unions/intersections are the TS/MLsub trap (unreadable inferred types, non-local errors). We will **never** produce them from inference.
- **Declared** unions/intersections are a separate, tractable surface feature: the user *writes* `int | string`, and the checker only ever *checks against* it. Declared-only types slot into bidirectional checking as just another **expectation** — no constraint solver involvement, no principality, no disturbance to the inference engine.

So going Full on the inference engine (S3c) costs us *inferred* unions (which we do not want) and costs us **nothing** on *declared* unions/intersections later. This file records that later option.

## The framing: a union is a *closed* `dyn`

`dyn` is the **open** top — any type, no exhaustiveness guarantee, narrowed one type at a time via S6's `x.as<T>()` → `?T`. A union `int | string` is a `dyn` whose membership is restricted to a **static, finite set** — which is exactly the property that buys *exhaustive* discrimination: a `match` over `{int, string}` can be checked complete, the way an enum match is. That is the value a union adds over plain `dyn`: the closed-world guarantee, riding on the same narrowing machinery (hence the S6 gate).

## Design sketch (to be detailed when the slice opens)

- **Lattice:** add `Type::Union(Vec<Type>)` (normalized: flattened, deduplicated, order-insensitive). Optionally `Type::Intersection(Vec<Type>)` — but see below, intersection is largely already covered.
- **Subtyping** (extends `Type::subtype`):
  - `A <: B | C` iff `A <: B` **or** `A <: C` (a member is a subtype of the union).
  - `B | C <: A` iff `B <: A` **and** `C <: A` (a union is a subtype of `A` only if every arm is).
  - Dual for intersection: `A & B <: C` iff `A <: C` or `B <: C`; `C <: A & B` iff `C <: A` and `C <: B`.
  - `T | … <: dyn` always (a union still widens into the open top).
- **Surface:** parse `A | B` (and possibly `A & B`) in `TypeRef` positions; `Type::from_ref` builds the variant. **Accepted only where written** — inference still joins to `dyn`, never synthesizes a union.
- **Narrowing / exhaustiveness:** reuse S6's `x.as<T>()`; extend the `match`-exhaustiveness check (`E0011` path) to treat a union's arm set as the closed domain, so a `match` over a `int | string` value is checked for completeness.

## Intersection — mostly already covered by S4

We get the useful form of intersection for free as **S4 trait bounds**: `<T: Comparable + Display>` is "implements Comparable AND Display" — intersection at the constraint level, Rust-style. First-class *structural* intersection types (`A & B` as a nameable type) buy little beyond that and are **optional**; this slice may ship unions only and leave structural intersection unbuilt unless a concrete need appears.

## Relationship to tagged alternatives

`Result` / `Option` / enums remain the primary, *tagged* "this or that" — nameable, dispatch-decidable, exhaustively checkable today. Declared unions are an *untagged* precision upgrade for when the tagged form is awkward; they are additive, not a replacement. Default guidance stays Rust-like: return a single concrete type, `dyn`, or a tagged enum; reach for a union only when a bounded-`dyn` is genuinely what you mean.

## Out of scope until this opens
No `Type::Union` code, no parser surface, no subtyping rules land before S6 is done. This document exists so the decision ("declared yes, inferred never") is on record and the engine work in S3c is known not to foreclose it.
