# `para/api` handoff — what std.http gives you, and what this arc learned

Written by the agent that built the `std.http` client layer (arc H6–H10), for whoever builds
`para/api`. The short version: **std.http never invokes user code; `para/api` does.** That one line
decides where everything lives, and it was learned the hard way — see "Why composition belongs in
Noeta" below.

## The seam std.http gives you

Everything here is shipped and covered by the conformance differential.

```noeta
use std.http.client

api = client.new("https://api.example.com")
    .header("accept", "application/json")
    .bearer(token)
    .timeout(30_000)
    .retry(3)

req  = api.prepare("get", "/users/1")        // build, don't perform
req2 = req.with_header("x-trace", id)        // copy-modify
resp = api.send(req2)?                       // perform — the terminal
user = resp.json::<User>()?                  // decode into your own type
```

| Piece | What it is |
|---|---|
| `Client` | Immutable config: base URL, headers, auth, deadline, retry. Every builder returns a new client, so sharing is safe. |
| `Client.prepare(method, path, body?, headers?)` | Builds a `Request` **without performing it** — path resolved against the base URL, client headers applied. The value your outermost middleware should see. |
| `Client.send(req)` | Performs a `Request` through the client's configuration. **This is the terminal your chain bottoms out in.** |
| `Request` | `method()`, `path()`, `url()`, `query(n)`, `header(n)`, `body()`, `body_bytes()`, plus copy-modify `with_header(n, v)` / `with_url(u)`. |
| `Response` | `status()`, `ok()`, `body()`, `body_bytes()`, `header(n)`, `url()` (final, after redirects), `links()`, `error_for_status()`, `json::<T>()`. |
| `HttpError` | `message()`, `kind()`, `url()`, `retryable()`. Implements `Error` + `Display`, so `?` converts it. |

Two things worth knowing about the error model, because they shape your API:

- **A transport failure is the `Err`; an HTTP status is not.** A 404 arrives as `Ok(Response)`. So
  `?` on a request means exactly "the network broke". `error_for_status()` is the opt-in door.
- **`HttpError.kind()` is classified, not stringly-typed** — `timeout`/`dns`/`connect`/`tls`/
  `protocol`/`invalid_url`/`other`, with `retryable()` derived from it. Build your policies on that
  predicate, never on message text.

Retry already lives in std (it calls no user code) and applies **inside** `send`, i.e. innermost —
beneath any middleware you wrap around it. If you need retry *outside* a middleware (to re-run the
whole chain), build that in `para/api` and leave `retry(0)` on the client.

## Why composition belongs in Noeta, not in the native client

This was attempted natively first, and it was wrong. The native `Client` held its middleware as
`Vec<Retained>` — arena ids for user closures. That smuggles raw references to GC-managed values
into a `Clone + PartialEq` value type:

- cloning a client doesn't re-retain, dropping one doesn't release (retention until run teardown);
- `PartialEq` ends up comparing arena *indices*;
- and the whole thing leaked until the leak oracle caught it.

Composing the chain in Noeta makes all of that vanish: closures are ordinary GC values held by an
ordinary struct. **Do not push middleware back down into native code.**

The shape that works:

```noeta
// A middleware is just a function.
type Middleware = fn(Request, fn(Request) -> Result<Response, HttpError>) -> Result<Response, HttpError>

// Compose innermost-first so the FIRST registered middleware is the outermost layer.
fn chain(mws: List<Middleware>, terminal: fn(Request) -> Result<Response, HttpError>) {
    handler = terminal
    for mw in mws.reversed() {
        inner = handler
        handler = fn(req) { return mw(req, inner) }
    }
    return handler
}
```

## Three findings that cost real time — please don't rediscover them

**1. Middleware must be an ONION with a callable `next`, not a before/after hook pair.**
A `before(req)` + `after(resp)` design looks simpler and cannot express the case that matters:
a middleware that answers *without* performing the request. Cache hits and mocking are exactly
that, and they are the two middlewares people actually want. The onion gets it for free — a layer
that never calls `next` short-circuits everything inside it.

**2. Pagination must be strategy-based. Do NOT privilege `Link`.**
RFC 8288 `Link` headers are a real standard (GitHub, GitLab, Jira, Shopify, WordPress), and
std.http gives you `resp.links()` as a parsed `rel -> target` map. But it is one of at least four
conventions in the wild:

| Convention | Who |
|---|---|
| RFC 8288 `Link` header | GitHub, GitLab, Shopify |
| `page` / `offset`+`limit` query params | countless REST APIs |
| opaque cursor in the body (`next_cursor`, `has_more`) | Slack, Notion |
| fully-qualified `next` URL in the body | Stripe |

So the extension point is a strategy — `next(req, resp) -> ?Request` — with `Link`, `Offset` and
`Page` shipped as built-ins and cursor/body-URL expressible by a user impl. **No default strategy**:
guessing wrong is worse than making the caller name one. Return a generator so pages stream lazily
and compose with `map`/`filter`.

Note `resp.links()` targets may be **relative**; resolve them against `resp.url()` (which is the
final URL after redirects, precisely so this works). `Client.prepare`/`send` treat an absolute
target as absolute, so handing a `next` link straight back to a based client is safe.

**3. `resp.json::<T>()` is recoverable by construction.**
It returns `Result<T, JsonError>`, not `T`. A response body is remote input — a server changing
shape must be a value you handle, not an abort. The `JsonError` is path-precise
(`items[2].price: expected float, found JSON string`), which is worth surfacing in your OpenAPI
error reporting. `json.parse::<T>(resp.body())` remains the aborting spelling if you want it.

## Language capability you may find useful

Extern types now participate in the **`Callable` protocol** (`crates/noeta-conformance/tests/callable_extern_seam.rs`):
a registered `call` method makes a native type's values invocable as `value(args)`, type-checked
through the declared signature. You do not need it for middleware (Noeta closures are callable
already), but it exists if an OpenAPI-generated *operation handle* would read better as
`api.getUser(id)` backed by a native value than as a generated function.

Also new: **call-site-typed extern methods** (`ExtType::typed_methods` / `typed_dispatch`) — the
turbofish surface `resp.json::<User>()`. If `para/api` grows native types that build values of a
caller-named type, that is the mechanism. See `docs/Native-Extensions.md`.

## Scope note

`para/api` should own: middleware (+ the standard set: logging, retry-outside-chain, cache, mock,
record), pagination strategies, and OpenAPI codegen. std.http keeps transport, configuration, the
error classification, and the `Link` parsing primitive — and should stay that way.
