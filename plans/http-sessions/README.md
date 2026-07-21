# Arc — cookies and stateless signed sessions

Status: **S1 done**, S2–S3 designed, S4 blocked

Sessions, end to end: the `Set-Cookie` surface `std.http` was missing (S1), a signed-cookie codec
that needs no store (S2–S3), and the DB-backed variant `para/aether` offers on top (S4).

## Why storeless first

`noeta serve --parallel N` spawns N worker threads, each with its **own** `RealHost` and its own
retained arena (`crates/noeta-cli/src/cmd/serve.rs:446`); only the bytecode `Module` is shared, and
the kernel load-balances accepts across workers arbitrarily. So an in-memory session map — the
obvious first implementation, a `Cell<Map<…>>` captured by the handler — is correct at
`--parallel 1` and **silently fragments** above it: a session written on worker 2 is invisible to
workers 1, 3, and 4, and a user's requests bounce between them. The bug appears only under the flag
you reach for in production, and presents as random logouts.

A signed cookie has no such failure mode: the state rides on the request, so every worker can read
it and no worker has to have written it. It is also the only design that needs **no framework hook
at all** — read from `Request`, write to `Response` — so it works with bare `server.serve` and
composes with any router. That is why it is the std-level answer and the DB-backed store is the
opt-in upgrade rather than the default.

## Slices

| # | Slice | Status |
|---|---|---|
| S1 | The cookie gap: multi-value headers, typed `Cookie`, `Request.cookies()` | **done** |
| S2 | `std.session` — the signed-token codec | todo |
| S3 | `Session` over a request/response pair; conformance + docs | todo |
| S4 | `para/aether` DB-backed sessions | **blocked** — see below |

---

## S1 — the cookie gap (done)

Three defects, closed:

1. **No multi-value emit.** `Response.with_header` `retain`s away same-named headers before pushing,
   and `server.response`'s `headers` argument is a `Map`, so a key is unique by construction. There
   was no way to emit two `Set-Cookie` headers — and `Set-Cookie` has no comma-joined form (the
   `Expires` attribute contains a comma), so two cookies *must* be two headers.
   Closed by `Response.with_added_header`, an **append** twin.

   `with_header` was deliberately left alone. `para/api`'s `Header` middleware documents its
   dependence on last-writer-wins ("a per-request header set by an inner layer wins, because
   `with_header` replaces case-insensitively"); turning that into accumulation would have every
   layered header quietly duplicate. Two operations, because the choice between them is never
   incidental.

2. **No multi-value read.** `header(name)` is a `.find` — first match only. Closed by
   `Response.headers_all(name) -> List<string>`.

3. **No cookie codec.** Closed by a typed `Cookie` (`crates/noeta-stdlib/src/cookie.rs`) built by
   `server.cookie(name, value)`, attached by `Response.with_cookie`, and read back by
   `Request.cookies()` / `Request.cookie(name)`.

Two invariants are structural rather than documented, both because the failure they prevent is
silent:

- **Validation at construction, not serialization.** A cookie value is derived from user input more
  reliably than any other header; `\r\n` in an unchecked value is response splitting and a stray `;`
  forges attributes. Validating in `Cookie::new` and the `with_*` builders makes an unserializable
  cookie unrepresentable, which is what lets `to_header()` be total.
- **`SameSite=None` implies `Secure`.** A browser discards the pair outright, so the alternative to
  upgrading is a cookie that is never stored while the response looks correct on the wire. The
  upgrade is one-directional — `with_secure(false)` on such a cookie is an explicit contradiction and
  errors.

`SameSite` is a validated **string**, not an enum: the extern ABI has no enum door (there is no
`ExtEnum`), and `HttpError.kind()` is the same shape. `SameSite::parse` is what keeps the set closed.

---

## S2 — `std.session`, the codec

A new module rather than more surface on `http.server`: it is a pure codec, testable without a
server, and equally the right tool for any signed-token need (an unsubscribe link, a CSRF token).
It needs no new native payload — `crypto` is already linked and `base64` is already a dependency of
the workspace — so **no ring**.

### Token format

```
base64url(payload_json) "." base64url(hmac_sha256(key, base64url(payload_json)))
```

`payload_json` is `{"d": {…}, "exp": <unix_seconds>}`. Nesting the data under `d` rather than
reserving key names (`_exp`) means there is no reserved-name rule for a caller to trip over.

### Decisions to encode

- **Verify before parse.** The HMAC is checked, in constant time, *before* the JSON is parsed —
  never hand attacker-controlled bytes to a parser that has not been authenticated. This ordering is
  the whole security argument and belongs in a test, not only a comment.
- **`exp` is mandatory and checked at decode.** With no store there is nothing to revoke against, so
  a stolen cookie is valid until it expires; an unbounded token is valid forever. Expiry is a
  parameter of `encode`, not an option.
- **Rotation is a list, not a value.** `keyring(secrets)` signs with the first and verifies against
  all. Without this, rotating a secret logs out every user at once, so nobody ever rotates.
- **A hard size error at 4096 bytes.** The real ceiling of storeless sessions. Silently truncating
  state is the worst available failure; erroring is what tells an author they have outgrown this and
  should reach for S4.
- **Values are `Map<string, string>`.** Sessions hold identifiers and flags. The restriction is what
  keeps the 4 KB ceiling reachable, and makes S4 a genuine upgrade rather than a parallel API.

### Surface

| Function | Signature |
|---|---|
| `session.keyring` | `keyring(secrets: List<string>) -> Keyring` |
| `session.encode` | `encode(data: Map<string,string>, keys: Keyring, max_age: int) -> string` |
| `session.decode` | `decode(token: string, keys: Keyring) -> Map<string,string>?` |

`decode` returns `none` for *every* rejection — bad signature, expired, malformed — rather than a
`Result` with a reason. An attacker learns nothing from the distinction, and a caller has exactly
one correct response to all three: treat the request as unauthenticated.

## S3 — the request/response pair

`Session` over the codec, so a handler never touches tokens:

- `session.open(req, keys) -> Session` — decode from the `Cookie` header, or an empty session.
- `Session.get/set/remove/clear`, and a `dirty` flag.
- `Session.attach(resp, keys, max_age) -> Response` — re-emits `Set-Cookie` **only when dirty**, so
  an unchanged session costs no header and does not extend its own expiry by accident.

Defaults: `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure` **on** — the opposite of the raw
`Cookie` default, because here the stakes justify breaking plain-http local dev, with an explicit
opt-out.

## S4 — `para/aether`, DB-backed (blocked)

**Blocked on aether, not on this arc.** aether cannot host *either* kind of session today:
`aether.noe:381` destructures the `Request` into three strings before dispatch, so a handler cannot
see it; handlers return `string`, so they cannot set a header; and `trait Middleware` is
`before`-only, so there is no after-hook to attach a cookie on the way out. Request-context
threading + a response hook is its own arc and must land first.

### Making `para/db` an opt-in dependency

The open design question. **Rings are the wrong mechanism** — `ExtModule.ring` gates *native payload
linking* inside an extension crate by reachability (the ~5 MB reqwest tree); it is a linker/DCE
concept, and `noeta-pm` contains no reference to rings at all. `Dependency` likewise has no
`optional` field, and the `[features]` table in `noeta-pm` is *cargo*'s, for native crates.

The mechanism that already fits is `Dependency::Scope` — `para = [{path}, {path}]` — whose doc
comment names this exact case: "what lets an app depend on more than one package of the same scope
(`para/aether` *and* `para/db`) without two colliding TOML keys". So:

- aether declares a `trait SessionStore` and ships the **stateless** implementation over S2/S3,
  depending on nothing.
- The DB-backed implementation ships **inside `para/db`**, which already owns the connection, the
  repository, and migrations. An app opts in by adding `para/db` to the `para` scope list it very
  likely already has.
- Discoverability comes from the **diagnostic**, not from manifest metadata: configuring a DB store
  without `para/db` present should error naming the exact line to add — the play `E0019` already
  makes for imports ("add this exact `use`"). A `[suggests]` table would be metadata nobody reads at
  the moment they need it.

A manifest-level `optional`/`suggests` remains a legitimate future PM feature; it is simply not
required for this, and adding it here would be building the general mechanism before the second use
case exists.
