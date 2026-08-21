//! The `std` extension registration — the concrete half of the native-extension registry (the ABI
//! type & trait vocabulary lives in [`noeta_ext_abi::registry`], re-exported here).
//!
//! Core's `std` is the dogfood: several in-tree [`Extension`] units ([`CoreExtension`],
//! [`HttpExtension`], [`CryptoExtension`], [`IdExtension`], [`VecExtension`]), all
//! sharing the `"std"` root, register the Ring 2 modules (`math`/`random`/`fs`/`json`/`crypto`/
//! `http`/…) and the core extern types (`Uuid`/`FileHandle`/`Hasher`/`Response`) through the very API
//! a third-party extension would use. Each module declares its [`ExtFn`] signatures
//! plus one shared `dispatch`; both backends route every call through the lookup functions here
//! (`find_module`/`dispatch`/`find_type`/`dispatch_method`), so the differential oracle
//! (`TreeWalkBackend` ≡ `VmBackend`) holds by construction. The neutral value marshalling
//! ([`NativeValue`]/[`NativeOut`]) and the [`Host`] seam are the ABI crate's; this module only
//! *uses* them.

pub use noeta_ext_abi::registry::*;

use crate::{
    Arg, Dispatch, ErrorKind, Host, Output, StdError, arity_error, math, no_function_error,
    type_error,
};

// Core's `std` is registered as **several in-tree [`Extension`] units** (package-manager P1.4),
// all sharing the `"std"` namespace root — the dogfood proving the multi-extension registry a
// third-party package plugs into. Each unit is a wholesale include/exclude boundary (the seam
// Phase 2/3 populate; the shape a heavy ring would gate behind a Cargo feature): [`CoreExtension`]
// is the always-on Ring-1/2 surface, and each capability with a separable identity — `http`,
// `crypto`, `id`, the `vec`/`quat` geometry pair (extraction-prep, native-extensions), `p2p` — is
// its own unit. `find_module`/`find_type`/`commands` iterate every unit filtered by root, so the
// registered surface is **identical** to the former single `StdExtension` — this is a faithful
// partition, differential-green by construction.

/// A one-line `impl Extension` for a `std`-rooted core unit: a label name, the shared `"std"` root,
/// and its module/type slices. Commands are default-empty (only `http` overrides).
macro_rules! std_unit {
    ($ty:ident, $label:literal, modules = $modules:expr, types = $types:expr $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;
        impl Extension for $ty {
            fn name(&self) -> &'static str {
                $label
            }
            fn root(&self) -> &'static str {
                "std"
            }
            fn modules(&self) -> &'static [ExtModule] {
                $modules
            }
            fn types(&self) -> &'static [ExtType] {
                $types
            }
        }
    };
}

/// The core unit — the always-on Ring-1/2 surface. Written out (not `std_unit!`) because it also
/// declares the built-in dev-tiers and their attributes (tier-extensions port, `crate::tiers`).
#[derive(Debug, Clone, Copy)]
pub struct CoreExtension;
/// std's native derive recipes (derive layer 4) — the dogfood proving the `ExtDerive` seam the
/// way core modules prove the module ABI. `@derive(Inspect)` gives a type
/// `fn inspect(): dyn` forwarding into `json.stringify(self)` — a structural dump via the native
/// JSON renderer, no macro and no per-type codegen.
static STD_DERIVES: &[noeta_ext_abi::registry::ExtDerive] = &[noeta_ext_abi::registry::ExtDerive {
    name: "Inspect",
    methods: &[noeta_ext_abi::registry::ExtDeriveMethod {
        name: "inspect",
        arity: 0,
        handler: noeta_ext_abi::JSON_STRINGIFY_HANDLER,
    }],
    validate: None,
}];

impl Extension for CoreExtension {
    fn name(&self) -> &'static str {
        "std.core"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        CORE_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        CORE_TYPES
    }
    fn tiers(&self) -> &'static [noeta_ext_abi::registry::ExtTier] {
        crate::tiers::TIERS
    }
    fn attributes(&self) -> &'static [noeta_ext_abi::registry::ExtAttribute] {
        crate::tiers::ATTRIBUTES
    }
    fn derives(&self) -> &'static [noeta_ext_abi::registry::ExtDerive] {
        STD_DERIVES
    }
    fn body_formatters(&self) -> &'static [noeta_ext_abi::registry::BodyFormatter] {
        crate::tiers::BODY_FORMATTERS
    }
    fn capabilities(&self) -> &'static [noeta_ext_abi::registry::ExtCapability] {
        // The reactive engine provides the `ReactiveSource` capability (capability-broker seam) so a
        // foreign source node — `para.synced` — reaches the shared graph by trait, out of `std`.
        crate::reactive::REACTIVE_CAPABILITIES
    }
}
std_unit!(
    CryptoExtension,
    "std.crypto",
    modules = CRYPTO_MODULES,
    types = CRYPTO_TYPES
);
std_unit!(
    IdExtension,
    "std.id",
    modules = ID_MODULES,
    types = ID_TYPES
);
// The `vec`/`quat` packed-3D-math pair, split into its own unit to **prep extraction** into an
// out-of-tree geometry package (native-extensions; Phase 3). No extern types — pure value math.
// Written out (not `std_unit!`) because it declares the two kernel `ExtTrait`s (ExtBundle→ExtTrait
// fold-in, slice 4): the `impl vec.Kernels for T {}` / `impl vec.SatKernels for T {}` binding surface.
#[derive(Debug, Clone, Copy)]
pub struct VecExtension;
/// The two migrated kernel traits (`vec.Kernels`, `vec.SatKernels`). Namespaced `std.vec` so the
/// module-qualified `impl` spelling and the runtime dispatch route resolve one identity.
static VEC_TRAITS: &[noeta_ext_abi::registry::ExtTrait] = &[
    crate::vec_kernels::VEC_KERNELS,
    crate::vec_kernels::VEC_SAT_KERNELS,
];
impl Extension for VecExtension {
    fn name(&self) -> &'static str {
        "std.vec"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        VEC_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        &[]
    }
    fn traits(&self) -> &'static [noeta_ext_abi::registry::ExtTrait] {
        VEC_TRAITS
    }
}
// The p2p/local-first stack (`crdt`/`p2p`/`synced`) left `std` for the first-party non-default
// `para` namespace — it now lives in the standalone para-p2p package repo
// (github.com/noeta-lang/para-p2p), installed only when a program depends on it. See the
// para-namespace arc.

/// The `http` unit — the only one contributing a CLI subcommand (`noeta serve`), so it can't use the
/// `std_unit!` shorthand.
#[derive(Debug, Clone, Copy)]
pub struct HttpExtension;

impl Extension for HttpExtension {
    fn name(&self) -> &'static str {
        "std.http"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        HTTP_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        HTTP_TYPES
    }
    fn commands(&self) -> &'static [noeta_ext_abi::ExtCommand] {
        // `noeta serve` (higher-order-abi H6) — contributed here, not a core CLI verb.
        &[crate::serve::SERVE_COMMAND]
    }
    fn enums(&self) -> &'static [ExtEnum] {
        HTTP_ENUMS
    }
    fn structs(&self) -> &'static [ExtStruct] {
        HTTP_STRUCTS
    }
}

/// The `http` unit's native enum: `Framing` (http-streaming arc) — how `client.stream` cuts a
/// response body. A real language enum rather than a string, so a `match` over it is exhaustive
/// (E0011) and a typo is a compile error instead of a runtime surprise.
///
/// Fieldless and unbacked: the variants are a choice, not a value with a wire representation, so
/// there is nothing for `.value()` to return.
const HTTP_ENUMS: &[ExtEnum] = &[ExtEnum {
    name: noeta_ext_abi::stream::FRAMING_TYPE_NAME,
    namespace: "std.http",
    variants: &[
        ExtVariant {
            name: "Sse",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Ndjson",
            fields: &[],
            value: VariantValue::None,
        },
        ExtVariant {
            name: "Lines",
            fields: &[],
            value: VariantValue::None,
        },
    ],
    doc: "How `client.stream` cuts a response body into `Frame`s. The three cuts cover the deployed \
          world, and they are deliberately one enum rather than three entry points: switching an \
          LLM client between an OpenAI-compatible endpoint and a native Ollama one changes this \
          argument and nothing else.\n\n```noeta\nfor frame in client.stream(req, \
          Framing.Ndjson) {\n    let msg = json.parse::<Message>(frame.data)\n}\n```\n\nIt is a real \
          enum rather than a string, so a `match` over it is exhaustive (E0011) and a typo is a \
          compile error instead of a runtime surprise. The variants are a choice, not a value with \
          a wire representation, so there is no `.value()` on them.",
    docs: FRAMING_DOCS,
    ..ExtEnum::DEFAULTS
}];

/// Per-variant prose for [`HTTP_ENUMS`]' `Framing`. Each variant states which [`Frame`] fields it
/// populates, since that is the part a caller gets wrong.
///
/// [`Frame`]: noeta_ext_abi::stream::Frame
const FRAMING_DOCS: &[(&str, &str)] = &[
    (
        "Sse",
        "`text/event-stream` — W3C/WHATWG server-sent events. The default, because it is what the \
         overwhelming majority of streaming HTTP APIs speak.\n\nPopulates every `Frame` field: \
         `data` holds the joined `data:` lines, `event` the frame's name (empty when it names none, \
         deliberately **not** defaulted to `\"message\"` the way a browser's `EventSource` does), \
         `id` the stream's last-seen event id (which persists across frames per the spec), and \
         `retry` only on a frame whose own block carried a `retry:` field.",
    ),
    (
        "Ndjson",
        "Newline-delimited JSON: one JSON document per line, blank lines skipped. `Frame.data` is \
         the line **verbatim** — not parsed — so you choose your own decoding (`json.parse`, or a \
         typed `json.parse::<T>`) and a malformed line is yours to report rather than one the \
         reader swallows. `event` and `id` are empty.",
    ),
    (
        "Lines",
        "One raw line per frame in `Frame.data`, blank lines **kept** — that is the whole \
         difference from `Ndjson`: a blank line is content in a log tail, and is not a JSON \
         document. `event` and `id` are empty.",
    ),
];

/// The `http` unit's native value struct: `Frame` (http-streaming arc) — one frame cut out of a
/// streaming body.
///
/// A **struct**, not a class and not an extern handle, and that is the load-bearing decision: a
/// struct-kind type is `Send` when all its fields are (they are all `string`/`?int` here), so a
/// frame crosses a channel or an isolate. A consuming pipeline — the motivating case is an LLM
/// client re-emitting provider tokens to a browser — is channel-based, and a `class`/`dyn` is
/// `!Send` and could not participate.
///
/// Every field is `pub` and none is `mut`: a frame is a decoded observation, so reading it is the
/// whole point and mutating it in place is meaningless (build a new one).
const HTTP_STRUCTS: &[ExtStruct] = &[ExtStruct {
    name: noeta_ext_abi::stream::FRAME_TYPE_NAME,
    namespace: "std.http",
    fields: &[
        ExtField {
            name: "event",
            ty: Str,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "data",
            ty: Str,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "id",
            ty: Str,
            is_public: true,
            is_mut: false,
        },
        ExtField {
            name: "retry",
            ty: SigType::Option(&SigType::Int),
            is_public: true,
            is_mut: false,
        },
    ],
    doc: "One frame cut out of a streaming response body by `client.stream`. Which fields carry \
          anything depends on the `Framing` the stream was opened with — see that enum's \
          variants.\n\nIt is a **struct**, not a class and not an extern handle, and that is \
          load-bearing: a struct is `Send` when all its fields are (these are all `string`/`?int`), \
          so a frame crosses a channel or an isolate. The motivating case is an LLM client \
          re-emitting provider tokens to a browser, which is channel-based — a `class`/`dyn` is \
          `!Send` and could not participate.\n\nEvery field is readable and none is mutable: a \
          frame is a decoded observation, so reading it is the point and mutating it in place is \
          meaningless (build a new one).",
    docs: FRAME_DOCS,
    ..ExtStruct::STRUCT_DEFAULTS
}];

/// Per-field prose for [`HTTP_STRUCTS`]' `Frame`.
const FRAME_DOCS: &[(&str, &str)] = &[
    (
        "event",
        "The SSE `event:` field — the frame's name. Empty when the framing has no notion of one \
         (`Ndjson`, `Lines`), or when an SSE frame names none.",
    ),
    (
        "data",
        "The frame's payload: the joined SSE `data:` lines, one NDJSON document, or one raw line. \
         Always verbatim — never parsed for you.",
    ),
    (
        "id",
        "The SSE stream's last-seen event id, which persists across frames per the spec. Empty \
         under `Ndjson`/`Lines`.",
    ),
    (
        "retry",
        "The SSE reconnection delay in milliseconds, set only on a frame whose own block carried a \
         `retry:` field. `none` otherwise.",
    ),
];

/// The `id` unit's extern type: `Uuid` (X2 — pure, byte-ordered, key-capable).
///
/// It declares `Comparable`, because it is: a UUID compares by its 16 bytes at every door the
/// runtime has — `compare`, a set's canonical order, a map key's placement — and a version-7 UUID's
/// whole purpose is that byte order being creation order. Declaring it is what lets `<`, `sorted`,
/// `min`/`max` and a `T: Comparable` bound reach the ordering the type already has.
///
/// It declares `Display` for the same reason: the type renders itself as the canonical hyphenated
/// form and answers `to_string()` with exactly that text, so `${id}` and `id.to_string()` agree.
/// Declaring it is what lets `T: Display` and `id is dyn Display` see the rendering `echo` already
/// uses.
const ID_TYPES: &[ExtType] = &[ExtType {
    name: crate::id::TYPE_NAME,
    namespace: "std.id",
    methods: UUID_METHODS,
    dispatch: uuid_method_dispatch,
    key_capable: true,
    traits: &["Comparable", "Display"],
    docs: UUID_METHOD_DOCS,
    ..ExtType::DEFAULTS
}];

/// The `crypto` unit's extern type: the incremental `Hasher` (C3).
const CRYPTO_TYPES: &[ExtType] = &[ExtType {
    name: crate::crypto::HASHER_TYPE_NAME,
    namespace: "std.crypto",
    methods: HASHER_METHODS,
    dispatch: hasher_method_dispatch,
    key_capable: false, // `update` mutates — a hasher can never key a map
    docs: HASHER_METHOD_DOCS,
    ..ExtType::DEFAULTS
}];

/// The `http` unit's extern types: the outbound `Response` and inbound `Request` (http arc /
/// http-server). Both stay top-level type names (no module move in P0.3b's client/server split).
const HTTP_TYPES: &[ExtType] = &[
    ExtType {
        name: crate::net::RESPONSE_TYPE_NAME,
        namespace: "std.http",
        methods: RESPONSE_METHODS,
        dispatch: response_method_dispatch,
        typed_methods: RESPONSE_TYPED_METHODS,
        typed_dispatch: Some(response_typed_method_dispatch),
        key_capable: false, // a response is not a map key
        docs: RESPONSE_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::net::REQUEST_TYPE_NAME,
        namespace: "std.http",
        methods: REQUEST_METHODS,
        dispatch: request_method_dispatch,
        key_capable: false, // an inbound request is not a map key
        docs: REQUEST_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Keyring` (session arc S2) — the signing secrets. Opaque on purpose: it has no methods, so
    // there is no way to read a secret back out of one in-language.
    ExtType {
        name: crate::session::KEYRING_TYPE_NAME,
        namespace: "std.session",
        key_capable: false, // a keyring is not a map key
        docs: KEYRING_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Session` (session arc S3) — the decoded data plus whether it changed.
    ExtType {
        name: crate::session::SESSION_TYPE_NAME,
        namespace: "std.session",
        methods: SESSION_METHODS,
        dispatch: session_method_dispatch,
        key_capable: false, // a session is not a map key
        // `data()` hands back a `Map`, which must arrive marshalled rather than as a handle.
        deep_marshal: true,
        docs: SESSION_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Cookie` (cookie arc C1) — a validated `Set-Cookie`. Pure, content-equal, immutable data
    // like `Response`; every builder returns a new one.
    ExtType {
        name: crate::cookie::COOKIE_TYPE_NAME,
        namespace: "std.http",
        methods: COOKIE_METHODS,
        dispatch: cookie_method_dispatch,
        key_capable: false, // a cookie is not a map key
        docs: COOKIE_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Client` (http arc H7) — the configured client: base URL, headers, auth, deadline. Pure,
    // content-equal config; immutable, so every builder method returns a new one.
    ExtType {
        name: crate::http_client::CLIENT_TYPE_NAME,
        namespace: "std.http",
        methods: CLIENT_METHODS,
        dispatch: client_method_dispatch,
        key_capable: false, // a client is not a map key
        // The verbs take the same optional `headers: Map<string, string>` the free functions do,
        // so the map must arrive marshalled rather than as an opaque handle.
        deep_marshal: true,
        docs: CLIENT_DOCS,
        ..ExtType::DEFAULTS
    },
    // `HttpError` (http arc H6) — the transport failure the client's `Result` door carries. Pure,
    // content-equal data like `Response`; declares `Error` + `Display` (the `JsonError` model) so
    // `<E: Error>` bounds accept it and `?` converts through `From`.
    ExtType {
        name: crate::net::HTTP_ERROR_TYPE_NAME,
        namespace: "std.http",
        methods: HTTP_ERROR_METHODS,
        dispatch: http_error_method_dispatch,
        key_capable: false, // a transport failure is not a map key
        traits: &["Error", "Display"],
        docs: HTTP_ERROR_DOCS,
        ..ExtType::DEFAULTS
    },
    // The websocket session handle (server-hmr L0) — its methods reach the `Network` hijack seam
    // (send/recv/close ride the executor), so they live in the ctx table.
    ExtType {
        name: crate::serve::SOCKET_TYPE_NAME,
        namespace: "std.http",
        ctx_methods: crate::serve::SOCKET_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::serve::socket_ctx_method_dispatch(method, ctx, recv, args)
        }),
        key_capable: false, // identifies a host resource
        docs: SOCKET_DOCS,
        ..ExtType::DEFAULTS
    },
    // The incremental body reader (http-streaming arc) — the `Socket` shape on the OUTBOUND side:
    // a host-resource handle whose `recv` rides the executor, so its body methods live in the ctx
    // table. Its **head** methods (`status`/`ok`/`header`/`error_for_status`) are plain reads off
    // the handle and live in the ordinary table, so answering "did this request fail?" costs no
    // executor round and no `recv()`.
    ExtType {
        name: noeta_ext_abi::stream::FRAME_STREAM_TYPE_NAME,
        namespace: "std.http",
        methods: crate::http_stream::FRAME_STREAM_METHODS,
        dispatch: crate::http_stream::frame_stream_method_dispatch,
        ctx_methods: crate::http_stream::FRAME_STREAM_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::http_stream::frame_stream_ctx_method_dispatch(method, ctx, recv, args)
        }),
        key_capable: false, // identifies a host resource
        docs: FRAME_STREAM_DOCS,
        ..ExtType::DEFAULTS
    },
    // The event-stream sink (http-streaming arc) — `Socket`'s write-only inbound twin. Its `send`
    // takes a `Frame` value struct, which must arrive marshalled rather than as an opaque handle.
    ExtType {
        name: noeta_ext_abi::stream::SSE_SINK_TYPE_NAME,
        namespace: "std.http",
        ctx_methods: crate::http_stream::SSE_SINK_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::http_stream::sse_sink_ctx_method_dispatch(method, ctx, recv, args)
        }),
        key_capable: false, // identifies a host resource
        deep_marshal: true,
        docs: SSE_SINK_DOCS,
        ..ExtType::DEFAULTS
    },
];

/// `FrameStream`'s method prose (`noeta doc --api` renders `docs/std-http.md` from this).
const FRAME_STREAM_DOCS: &[(&str, &str)] = &[
    (
        "status",
        "The response status the opening handshake received — readable immediately, **before** the first \
         `recv()`. Check it: a rate-limited provider answers a streaming request with a `429` whose body is \
         a JSON error document, and since that is not an event stream, `Framing.Sse` decodes it to zero \
         frames. Without the status, that failure is indistinguishable from a model that had nothing to \
         say.",
    ),
    (
        "ok",
        "Whether `status()` is a 2xx. `if !stream.ok() { … }` is the guard to write before draining a \
         stream you did not open with `error_for_status()`.",
    ),
    (
        "header",
        "A response header from the opening handshake, matched case-insensitively; `none` when absent. \
         This is where a streamed failure keeps its actionable part: `stream.header(\"retry-after\")` on a \
         `429` tells a backoff loop how long to wait, and the provider's `x-ratelimit-*` headers report the \
         remaining budget.",
    ),
    (
        "error_for_status",
        "Turn a non-2xx status into the `Err` arm, so `client.stream(req, Framing.Sse)?.error_for_status()?` \
         short-circuits a rate limit the same way a transport failure does. Opt-in and explicit, exactly \
         like `Response.error_for_status`: a status is an answer, not a broken network, so plain `?` on a \
         `stream(...)` keeps its one meaning.",
    ),
    (
        "recv",
        "The next frame of the body, or `none` once the body ends. Await it: `frame = stream.recv().await`. \
         A stream yields `none` forever after the body ends, so a `while` loop over it terminates.",
    ),
    (
        "close",
        "Release the stream and its connection without reading the rest of the body — what a caller does \
         after seeing a terminal frame (`[DONE]`) rather than draining to the end. Idempotent, and \
         unnecessary once `recv` has returned `none`.",
    ),
];

/// `SseSink`'s method prose.
const SSE_SINK_DOCS: &[(&str, &str)] = &[
    (
        "send",
        "Push one `Frame` to the client. A multi-line `data` is encoded as several `data:` lines, which is \
         the only legal way to carry a newline through an event stream.",
    ),
    (
        "comment",
        "Write an SSE comment (`: text`) — the keepalive heartbeat. It puts bytes on the wire without \
         dispatching an event, so an idle stream is not reaped by an intermediary.",
    ),
    (
        "close",
        "End the event stream and release the connection. The stream also closes when the handler returns, \
         so this is for ending early.",
    ),
];

/// The always-on core extern types: `FileHandle` (X3 — mutable + effectful, `fs`), `Cell<T>` (H4),
/// and the reactive handles (H5).
const CORE_TYPES: &[ExtType] = &[
    // `Span` (native OTEL T1) — a mutable, effectful, host-coupled handle (like `FileHandle`): its
    // methods reach the `Tracing` capability by id. NOT key-capable (identifies a host resource).
    // `deep_marshal` so `add_event_with`'s `Map<string, …>` argument arrives as a full
    // `NativeValue::Map` (the shallow projection collapses containers to opaque) — the same reason
    // the metrics handles set it for their `*_with(_, attrs)` forms.
    ExtType {
        name: crate::tracing::SPAN_TYPE_NAME,
        namespace: "std.tracing",
        methods: crate::tracing::SPAN_METHODS,
        dispatch: crate::tracing::span_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: SPAN_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // The metrics instrument handles (native OTEL Phase M) — mutable, effectful, host-coupled like
    // `Span`: their methods reach the `Metrics` capability by id. Namespaced under `std.metrics`, so
    // the idiomatic OTel names are `use`-imported (not globally reserved) and coexist with a user's
    // own `Counter`. Not key-capable; `deep_marshal` so the `*_with(_, attrs)` map argument arrives
    // as a full `NativeValue`.
    ExtType {
        name: crate::metrics::COUNTER_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::COUNTER_METHODS,
        dispatch: crate::metrics::counter_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: COUNTER_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::HISTOGRAM_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::histogram_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: HISTOGRAM_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::GAUGE_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::gauge_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: GAUGE_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: "FileHandle",
        namespace: "std.fs",
        methods: FILE_HANDLE_METHODS,
        dispatch: file_handle_dispatch,
        key_capable: false,
        docs: FILE_HANDLE_DOCS,
        ..ExtType::DEFAULTS
    },
    // `JsonError` (error-machinery arc) — the JSON decode failure: pure, content-equal data (the
    // `ExecResult` model), std's first `Error` implementor. Declares `Error` + `Display` through
    // the registration (the extern-type analogue of a user `impl`), so `<E: Error>` bounds accept
    // it and it renders as its composed message.
    ExtType {
        name: crate::json::JSON_ERROR_TYPE_NAME,
        namespace: "std.json",
        methods: JSON_ERROR_METHODS,
        dispatch: json_error_method_dispatch,
        key_capable: false, // a decode failure is not a map key
        traits: &["Error", "Display"],
        docs: JSON_ERROR_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Base64Error` — the `JsonError` shape for a flat input: pure, content-equal data carrying the
    // failure kind and the offset it was detected at. Declares `Error` + `Display` so `<E: Error>`
    // bounds accept it, `?` converts through `From`, and `${e}` renders the composed message.
    ExtType {
        name: crate::base64::BASE64_ERROR_TYPE_NAME,
        namespace: "std.base64",
        methods: BASE64_ERROR_METHODS,
        dispatch: base64_error_method_dispatch,
        key_capable: false, // a decode failure is not a map key
        traits: &["Error", "Display"],
        docs: BASE64_ERROR_DOCS,
        ..ExtType::DEFAULTS
    },
    // `ExecResult` (stdlib-gaps) — pure, content-equal subprocess outcome (the `Response` model).
    ExtType {
        name: crate::os::EXEC_RESULT_TYPE_NAME,
        namespace: "std.os",
        methods: EXEC_RESULT_METHODS,
        dispatch: exec_result_dispatch,
        key_capable: false,
        docs: EXEC_RESULT_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Process` (process-handle arc) — a spawned child's control handle: a mutable, host-coupled
    // reference value (like `FileHandle`), its methods reaching the `Os` seam by id.
    ExtType {
        name: crate::os::PROCESS_TYPE_NAME,
        namespace: "std.os",
        methods: PROCESS_METHODS,
        dispatch: process_method_dispatch,
        key_capable: false,
        docs: PROCESS_DOCS,
        ..ExtType::DEFAULTS
    },
    // `OsError` (subprocess-doors arc) — the recoverable failure the `try_spawn`/`try_write` doors
    // carry. Pure, content-equal data like `HttpError`; declares `Error` + `Display` so `<E: Error>`
    // bounds accept it and `?` converts it through `From`.
    ExtType {
        name: crate::os::OS_ERROR_TYPE_NAME,
        namespace: "std.os",
        methods: OS_ERROR_METHODS,
        dispatch: os_error_method_dispatch,
        key_capable: false, // a subprocess failure is not a map key
        traits: &["Error", "Display"],
        docs: OS_ERROR_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Cell<T>` (higher-order-abi H4) — the generic, Class-3 corner of the matrix: all methods
    // higher-order (ctx table), the held value in the retained arena; `get` is a declared
    // always-open arena read (H5), so the backend inlines it.
    ExtType {
        name: crate::cell::CELL_TYPE_NAME,
        namespace: "std.cell",
        ctx_methods: crate::cell::CELL_CTX_METHODS,
        // A shim closure picks the `dyn` instantiation of the generic dispatch (the fn-pointer
        // table needs the higher-ranked trait-object lifetime a turbofish cannot name).
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::cell::cell_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::cell::CELL_ARENA_GETTER),
        docs: CELL_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // The reactive handles (higher-order-abi H5): generic extern types over the per-run graph
    // state; `get` on both readable kinds is a declared arena read behind the extension's gate.
    ExtType {
        name: crate::reactive::SIGNAL_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::SIGNAL_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::signal_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::reactive::SIGNAL_ARENA_GETTER),
        docs: SIGNAL_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::reactive::COMPUTED_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::COMPUTED_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::computed_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::reactive::COMPUTED_ARENA_GETTER),
        docs: COMPUTED_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::reactive::EFFECT_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::EFFECT_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::effect_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: EFFECT_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // `View` (server-hmr L1) — the diff-push flush subscriber: named bindings onto
    // Signal/Computed/SyncedSignal handles, `snapshot()`/`diff()` render the wire frames.
    ExtType {
        name: crate::reactive::VIEW_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::VIEW_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::view_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: VIEW_METHOD_DOCS,
        // `expose`'s second argument is a reactive node whose value this view serializes on every
        // later flush tick, not here — so the call site's static type is the last place the value's
        // signedness exists, and the binding carries the hint from here on.
        push_hint_args: crate::reactive::VIEW_PUSH_HINT_ARGS,
        ..ExtType::DEFAULTS
    },
];

/// The `FileHandle` instance methods (extern-types X3) — the signatures the checker's
/// `file_handle_method`/`file_handle_params` tables used to hardcode.
const FILE_HANDLE_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "read_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["count"],
        name: "read",
        params: &[Int],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["contents"],
        name: "write",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &[],
        name: "close",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
];

/// Method dispatch for `FileHandle` (extern-types X3): the cursor logic lives on the shared
/// [`crate::FileHandle`] as before — this replaces the two per-backend `call_file_handle_method`
/// twins with ONE body. The receiver mutates in place (reference semantics through the shared
/// cell) and `close` flushes through the host — the whole effectful corner of the matrix.
fn file_handle_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(handle) = recv.as_any_mut().downcast_mut::<crate::FileHandle>() else {
        return Err(type_error(method, "FileHandle"));
    };
    let some_str = |s: Option<String>| match s {
        Some(text) => NativeOut::Some(Box::new(NativeOut::Str(text))),
        None => NativeOut::None,
    };
    match method {
        "read_line" => {
            want_arity(method, args, 0)?;
            Ok(some_str(handle.read_line(host)?))
        }
        "read" => {
            want_arity(method, args, 1)?;
            let NativeValue::Scalar(Scalar::Int(count)) = args[0] else {
                return Err(type_error(method, "int"));
            };
            Ok(some_str(handle.read(count, host)?))
        }
        "write" => {
            want_arity(method, args, 1)?;
            let NativeValue::Str(chunk) = &args[0] else {
                return Err(type_error(method, "string"));
            };
            handle.write(chunk)?;
            Ok(NativeOut::Unit)
        }
        "close" => {
            want_arity(method, args, 0)?;
            // Take the flush instruction first (ends the handle borrow's logical role), then
            // hit the host — the same order both backend twins used.
            match handle.close() {
                None => {}
                Some(crate::Flush::Write { path, content }) => host.fs_write(&path, &content)?,
                Some(crate::Flush::Append { path, content }) => host.fs_append(&path, &content)?,
            }
            Ok(NativeOut::Unit)
        }
        _ => Err(crate::no_method_error("FileHandle", method)),
    }
}

/// The `std` extension units this crate contributes — what the facade below installs as the
/// registry's lazy default, and what an assembling binary (`noeta_cli::run_cli`, a composed
/// Phase-3 shim) passes to [`noeta_ext_abi::registry::install`] alongside its extra units. The
/// order is cosmetic — every lookup iterates the whole list filtered by namespace root.
pub fn std_units() -> Vec<&'static (dyn Extension + Sync)> {
    #[allow(unused_mut)]
    let mut units: Vec<&'static (dyn Extension + Sync)> = vec![
        &CoreExtension,
        &HttpExtension,
        &CryptoExtension,
        &IdExtension,
        &VecExtension,
    ];
    // The `std.datetime` calendar/timezone unit (Ring 3) — present only when its default-on ring is
    // compiled in, so a footprint-tailored build that sheds jiff also sheds the module + types.
    #[cfg(feature = "ring-datetime")]
    units.push(&crate::datetime::DateTimeExtension);
    // The `std.regex` engine unit (Ring 3) — same shape: present only when its default-on ring is
    // compiled in, so shedding the engine also sheds the module and both its types.
    #[cfg(feature = "ring-regex")]
    units.push(&crate::regex::RegexExtension);
    units
}

// --- the registry facade (package-manager Phase 3, N3.0) ----------------------------------------
//
// The registry *mechanism* — the assembled unit list and the whole generic lookup layer — lives in
// `noeta_ext_abi::registry` (it was grown here around the dogfood, but nothing in it was
// std-specific, and Phase 3's composed shim must not register its units through the dogfood
// crate). These wrappers keep every existing `noeta_stdlib::registry::*` call site working
// unchanged, and make an unseeded registry unobservable: each ensures the std units are installed
// (a no-op after the first call, or after an assembling binary's explicit earlier `install`).
//
// Std residue deliberately NOT moved: the unit definitions above and the `static_dispatch_ctx*`
// monomorphized fast routes below (they name `cell`/`reactive` concretely — the per-crate
// compiled-in fast path). (`is_module_function`'s transitional `vec`/`fs` special cases died with
// the N3.4 `with_packed` migration, as planned.)

/// Ensure the std units are installed before a lookup (lazy default; an explicit
/// [`noeta_ext_abi::registry::install`] by the assembling binary wins).
fn ensure() {
    noeta_ext_abi::registry::install_default(std_units);
}

/// Register [`std_units`] as the registry's **fallback provider** at load time, so that merely
/// *linking* this crate gives a process a working default registry — no call site has to remember
/// to seed. That is what makes seeding structural: the front-end crates reach the registry through
/// `noeta_ext_abi::registry::single_registry_process()`, which used to panic unless something had
/// already seeded. An assembling binary does seed; a **test** binary does not, so a crate's tests
/// passed only when a sibling test happened to run first through this crate's lazily-seeding facade
/// — a race CI lost across four crates at once.
///
/// This registers a function pointer and installs **nothing** (see
/// [`noeta_ext_abi::registry::set_default_provider`]): installation stays lazy on first lookup, so a
/// binary composing its own set with [`install_with_extras`] still wins the `OnceLock` and is
/// unaffected. Eager installation here would instead seed std-only into those binaries and make
/// their `install` panic.
///
/// Native only — `#[ctor]` has no wasm support, and every wasm driver (the wasm runner, the
/// playground engine) assembles its registry explicitly, so nothing there relies on the fallback.
#[cfg(not(target_family = "wasm"))]
#[ctor::ctor]
fn register_default_provider() {
    noeta_ext_abi::registry::set_default_provider(std_units);
}

/// The process-global default [`Registry`] as a first-class handle — the seeded-and-unwrapped form
/// the instance-registry threading (server-hmr F2) hands to a checker/backend that was **not**
/// given an explicit per-session registry. Ensures the std units are installed (like every facade
/// lookup), so the returned reference is always live. A host wanting a *different* extension set per
/// session builds its own [`Registry`] and threads that instead of calling this.
pub fn default_seeded() -> &'static noeta_ext_abi::registry::Registry {
    ensure();
    noeta_ext_abi::registry::default_registry()
        .expect("the default registry is seeded by `ensure()` immediately above")
}

/// The process-global default [`Registry`], named for what calling it MEANS: **this call site
/// assumes a single-registry process** (cross-cutting audit finding 5). The CLI, LSP, MCP, and IDE
/// run one registry per process (the recorded instance-registry decision), so their leaf lookups —
/// loader tier-seeding, IDE completion/namespace answers, the compiler's default entry presets —
/// take the global by design. A session assembled with extra extensions (noeta-embed, a composed
/// MCP toolchain) must NOT reach code that calls this; it threads its own registry via the
/// options/`_with_registry` seams. Grepping this name finds every site to upgrade if the IDE ever
/// goes session-aware. Behavior-identical to [`default_seeded`].
pub fn single_registry_process() -> &'static noeta_ext_abi::registry::Registry {
    default_seeded()
}

/// Assemble the registry for a toolchain binary: the std units plus a composed shim's `extra`
/// extension units (package-manager Phase 3). Called by `noeta_cli::run_cli` at entry, before
/// anything can look a name up. With no extras this is exactly the lazy default; with extras it
/// installs eagerly so a later facade lookup cannot race in an std-only default first.
pub fn install_with_extras(extra: &[&'static (dyn Extension + Sync)]) {
    if extra.is_empty() {
        ensure();
    } else {
        let mut units = std_units();
        units.extend_from_slice(extra);
        noeta_ext_abi::registry::install(units);
    }
}

/// Assemble a **standalone** registry — the std units plus `extra` — **without** touching the
/// process-global default (instance-registry IR5). This is the per-session assembly seam: an
/// embedding host that wants a session with its own extension set builds one here and threads it
/// through the checker / compiler / VM, so two sessions with different extension sets can coexist in
/// one process. (The uniqueness sweep in [`noeta_ext_abi::registry::Registry::new`] still applies —
/// a duplicate module identity across `extra` and std panics, as at install time.)
pub fn assemble_with_extras(
    extra: &[&'static (dyn Extension + Sync)],
) -> noeta_ext_abi::registry::Registry {
    let mut units = std_units();
    units.extend_from_slice(extra);
    noeta_ext_abi::registry::Registry::new(units)
}

/// Assemble (std ∪ `extra`) as an **interned** `&'static` registry — the per-session entry
/// (instance-registry IR5) for hosts that create sessions repeatedly. The whole pipeline hands out
/// `&'static Registry`, so a per-session assembly must leak; interning by the unit set bounds that
/// leak by *distinct configurations* instead of by session count (a game engine reloading levels
/// re-uses one assembly forever). Fallible: a mis-assembled unit set (duplicate identities, a type
/// namespaced outside its unit's root) is an `Err` for the host to surface, never a panic out of a
/// library entry point.
///
/// # Why the key is the units themselves
///
/// The table is keyed by **unit identity** — the `&'static dyn Extension` references the caller
/// passed, compared whole (see [`same_units`]). Two earlier keys were both wrong, in opposite
/// directions, and each produced the same silent failure: a session handed *another* session's
/// registry, so its own extension resolved as an unknown name.
///
/// * **Data pointers** collide, because an extension type is typically a unit struct and distinct
///   **zero-sized** statics share one address — any two ZST extensions hashed to the same key.
/// * **Names** collide too, one level up: two *different* extension objects may legitimately share
///   a `name()` (a host that links a plugin's v1 and v2 surface, or builds a unit set per feature
///   flag). `Registry::new`'s uniqueness sweep only rejects a duplicate name *within* one set, so it
///   never sees this — the second session simply got the first's assembly.
///
/// Comparing the references whole distinguishes both: a ZST's vtable carries its type even when its
/// address does not. A false *negative* (two vtable copies for one type across codegen units) costs
/// one extra assembly — a bounded leak, never a wrong registry — which is the direction to err in.
pub fn interned_with_extras(
    extra: &[&'static (dyn Extension + Sync)],
) -> Result<&'static noeta_ext_abi::registry::Registry, String> {
    use std::sync::{Mutex, OnceLock};
    type Units = Vec<&'static (dyn Extension + Sync)>;
    static INTERNED: OnceLock<Mutex<Vec<(Units, &'static noeta_ext_abi::registry::Registry)>>> =
        OnceLock::new();
    // Order-normalized by name so `[&A, &B]` and `[&B, &A]` are one configuration. Names are unique
    // within any set that assembles (`Registry::try_new` rejects a duplicate), so this is a total
    // order on every input that gets interned; a set that does *not* assemble returns `Err` below
    // and is never stored.
    let mut key: Units = extra.to_vec();
    key.sort_unstable_by_key(|unit| unit.name());
    let mut interned = INTERNED
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("registry intern table poisoned");
    if let Some((_, registry)) = interned.iter().find(|(units, _)| same_units(units, &key)) {
        return Ok(registry);
    }
    let mut units = std_units();
    units.extend_from_slice(extra);
    let registry: &'static _ =
        Box::leak(Box::new(noeta_ext_abi::registry::Registry::try_new(units)?));
    interned.push((key, registry));
    Ok(registry)
}

/// Whether two name-sorted unit lists are the **same units** — pairwise reference identity, vtable
/// included (see [`interned_with_extras`] for why the vtable is the load-bearing half).
fn same_units(
    a: &[&'static (dyn Extension + Sync)],
    b: &[&'static (dyn Extension + Sync)],
) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| std::ptr::eq(*x, *y))
}

/// All registered extensions.
pub fn extensions() -> &'static [&'static (dyn Extension + Sync)] {
    ensure();
    noeta_ext_abi::registry::extensions()
}

/// See [`noeta_ext_abi::registry::find_module`].
pub fn find_module(name: &str) -> Option<&'static ExtModule> {
    ensure();
    noeta_ext_abi::registry::find_module(name)
}

/// See [`noeta_ext_abi::registry::ext_tiers`].
pub fn ext_tiers() -> impl Iterator<Item = &'static noeta_ext_abi::registry::ExtTier> {
    ensure();
    noeta_ext_abi::registry::ext_tiers()
}

/// See [`noeta_ext_abi::registry::find_ext_tier`].
pub fn find_ext_tier(name: &str) -> Option<&'static noeta_ext_abi::registry::ExtTier> {
    ensure();
    noeta_ext_abi::registry::find_ext_tier(name)
}

/// Every installed extension's **verbatim-body** tier names — the text tiers (`doc` → markdown)
/// and expression tiers whose `@<name> { … }` bodies the lexer must capture un-parsed. The
/// front-end pipeline seeds `noeta_lexer::TextTiers` with these so a native tier's bodies capture
/// even though no `.noe` file declares them (a program `@tier(…, text/expr)` is discovered by the
/// lexer's own token scan instead).
pub fn ext_verbatim_tier_names() -> Vec<&'static str> {
    default_seeded().ext_verbatim_tier_names()
}

/// Every installed extension's **tier-body formatters** as `(language, formatter)` pairs — the
/// languages an extension supplied a `noeta fmt` reflow for (extension-driven tier-body formatting,
/// keyed by body language). The `noeta fmt` front-end maps a tier's declared `text:` language to one
/// of these; a language absent here stays verbatim. See [`noeta_ext_abi::registry::BodyFormatter`].
pub fn ext_body_formatters() -> Vec<noeta_ext_abi::registry::BodyFormatter> {
    ensure();
    noeta_ext_abi::registry::ext_body_formatters()
        .copied()
        .collect()
}

/// See [`noeta_ext_abi::registry::ext_attributes`].
pub fn ext_attributes() -> impl Iterator<Item = &'static noeta_ext_abi::registry::ExtAttribute> {
    ensure();
    noeta_ext_abi::registry::ext_attributes()
}

/// See [`noeta_ext_abi::registry::find_ext_attribute`].
pub fn find_ext_attribute(name: &str) -> Option<&'static noeta_ext_abi::registry::ExtAttribute> {
    ensure();
    noeta_ext_abi::registry::find_ext_attribute(name)
}

/// See [`noeta_ext_abi::registry::module_name`] (pure string projection — no registry state).
pub fn module_name(module: &str) -> &str {
    noeta_ext_abi::registry::module_name(module)
}

/// See [`noeta_ext_abi::registry::ring_of`].
pub fn ring_of(module: &str) -> Option<&'static str> {
    ensure();
    noeta_ext_abi::registry::ring_of(module)
}

/// See [`noeta_ext_abi::registry::is_extension_root`].
pub fn is_extension_root(root: &str) -> bool {
    ensure();
    noeta_ext_abi::registry::is_extension_root(root)
}

/// See [`noeta_ext_abi::registry::find_module_qualified`].
pub fn find_module_qualified(path: &[String]) -> Option<&'static ExtModule> {
    ensure();
    noeta_ext_abi::registry::find_module_qualified(path)
}

/// See [`noeta_ext_abi::registry::find_function`].
pub fn find_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_function(module, func)
}

/// See [`noeta_ext_abi::registry::find_ctx_function`].
pub fn find_ctx_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_ctx_function(module, func)
}

/// See [`noeta_ext_abi::registry::find_function_sig`].
pub fn find_function_sig(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_function_sig(module, func)
}

/// See [`noeta_ext_abi::registry::dispatch_ctx`].
pub fn dispatch_ctx(
    module: &str,
    func: &str,
    ctx: &mut dyn crate::NativeCtx,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_ext_abi::registry::dispatch_ctx(module, func, ctx, args)
}

/// See [`noeta_ext_abi::registry::commands`].
pub fn commands() -> impl Iterator<Item = &'static noeta_ext_abi::ExtCommand> {
    ensure();
    noeta_ext_abi::registry::commands()
}

// (`find_bundle` / `dispatch_bundle_method` were removed in the ExtBundle→ExtTrait fold-in (slice 4):
// a kernel bundle is now a native `ExtTrait`, dispatched through `dispatch_trait_method` below.)

/// See [`noeta_ext_abi::registry::find_trait_in_module`] (ExtBundle→ExtTrait fold-in, slice 4).
pub fn find_trait_in_module(
    qualified_module: &str,
    name: &str,
) -> Option<&'static noeta_ext_abi::ExtTrait> {
    ensure();
    noeta_ext_abi::registry::find_trait_in_module(qualified_module, name)
}

/// See [`noeta_ext_abi::registry::dispatch_trait_method`] (ExtBundle→ExtTrait convergence, slice 2).
pub fn dispatch_trait_method(
    trait_q: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_ext_abi::registry::dispatch_trait_method(trait_q, method, ctx, recv, args)
}

/// See [`noeta_ext_abi::registry::find_type`].
pub fn find_type(name: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_ext_abi::registry::find_type(name)
}

/// See [`noeta_ext_abi::registry::find_type_qualified`] (extern-type namespacing).
pub fn find_type_qualified(qualified: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_ext_abi::registry::find_type_qualified(qualified)
}

/// See [`noeta_ext_abi::registry::resolve_type`] (extern-type namespacing).
pub fn resolve_type(name: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_ext_abi::registry::resolve_type(name)
}

/// See [`noeta_ext_abi::registry::find_type_method`].
pub fn find_type_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_type_method(type_name, method)
}

/// See [`noeta_ext_abi::registry::find_type_ctx_method`].
pub fn find_type_ctx_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_type_ctx_method(type_name, method)
}

/// See [`noeta_ext_abi::registry::find_type_method_sig`].
pub fn find_type_method_sig(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_ext_abi::registry::find_type_method_sig(type_name, method)
}

/// See [`noeta_ext_abi::registry::dispatch_ctx_method`].
pub fn dispatch_ctx_method(
    type_name: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_ext_abi::registry::dispatch_ctx_method(type_name, method, ctx, recv, args)
}

/// See [`noeta_ext_abi::registry::dispatch_method`].
pub fn dispatch_method(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    ensure();
    noeta_ext_abi::registry::dispatch_method(recv, method, host, args)
}

// (The **virtual-module** mechanism — prelude-redesign P2's `VIRTUAL_MODULES` table, backend
// `call_native_module` intercepts, and compiler `Builtin` bindings for selective imports — died
// with higher-order-abi H5: `task` migrated at H0/H2, `http.serve` at H3, and `reactive`, the
// last entry, at H5. Every std module is registry-backed now; the whole `Builtin` orchestration
// family dispatches through the `NativeCtx` seam.)

/// Whether `<module>.<func>` names a callable module function — the single predicate the checker
/// and both backends share to decide what a selective member import (`use std.<mod>.<fn>`) binds,
/// so all three agree by construction. Pure registry delegation since package-manager N3.4
/// migrated the last per-backend fallbacks (the `vec` bulk `*_all` kernels became registered ctx
/// functions; `fs.list` got its real trailing-optional signature).
pub fn is_module_function(module: &str, func: &str) -> bool {
    find_function_sig(module, func).is_some()
}

/// See [`noeta_ext_abi::registry::dispatch`].
pub fn dispatch(
    module: &str,
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    ensure();
    noeta_ext_abi::registry::dispatch(module, func, host, args)
}

// --- argument helpers (shared by the module dispatch functions) ---------------------------------
// The exact-duplicate guard family lives once in `noeta_ext_abi::args` (audit-2 F8); only the
// module-specific extractors (`want_data`/`want_tag`/`want_headers`/`want_argv`) stay here.
use noeta_ext_abi::args::{want_arity, want_arity_range, want_int, want_str};

fn str_list(items: impl IntoIterator<Item = String>) -> NativeOut {
    NativeOut::List(items.into_iter().map(NativeOut::Str).collect())
}

/// A `Map<string, string>` result (F5 `env.parse`/`env.load`). Entries stay in the iteration order
/// of `items` — callers pass a `BTreeMap` so the map is key-sorted and deterministic.
fn str_map(items: impl IntoIterator<Item = (String, String)>) -> NativeOut {
    NativeOut::Map(
        items
            .into_iter()
            .map(|(k, v)| (k, NativeOut::Str(v)))
            .collect(),
    )
}

/// The surface type name of an argument, for error messages (matches each backend's `type_name`).
fn native_type_name(value: &NativeValue) -> &str {
    match value {
        NativeValue::Scalar(Scalar::Int(_)) => "int",
        NativeValue::Scalar(Scalar::Float(_)) => "float",
        NativeValue::Scalar(Scalar::F32(_)) => "f32",
        NativeValue::Scalar(Scalar::Bool(_)) => "bool",
        NativeValue::Str(_) => "string",
        NativeValue::Bytes(_) => "bytes",
        NativeValue::Unit => "unit",
        NativeValue::List(_) => "list",
        NativeValue::Map(_) => "map",
        NativeValue::Object { type_name, .. } | NativeValue::Opaque(type_name) => type_name,
        NativeValue::Extern(e) => e.type_display_name(),
        // A native enum value (native-extensibility S1): its enum name is the surface type.
        NativeValue::Variant { enum_name, .. } => enum_name,
        // A native class instance (native-extensibility S2): its class name is the surface type.
        NativeValue::Instance { class, .. } => class,
    }
}

// --- `math`: pure scalar functions, no host -----------------------------------------------------

/// Project a [`NativeValue`] onto the scalar [`Arg`] seam `math` consumes.
fn to_arg(value: &NativeValue) -> Arg<'_> {
    match value {
        NativeValue::Scalar(Scalar::Int(n)) => Arg::Int(*n),
        NativeValue::Scalar(Scalar::Float(f)) => Arg::Float(*f),
        NativeValue::Scalar(Scalar::F32(f)) => Arg::Float(*f as f64),
        NativeValue::Scalar(Scalar::Bool(b)) => Arg::Bool(*b),
        NativeValue::Str(s) => Arg::Str(s),
        NativeValue::Bytes(_)
        | NativeValue::Unit
        | NativeValue::List(_)
        | NativeValue::Map(_)
        | NativeValue::Object { .. }
        | NativeValue::Opaque(_)
        | NativeValue::Extern(_)
        | NativeValue::Variant { .. }
        | NativeValue::Instance { .. } => Arg::Other,
    }
}

fn from_output(output: Output) -> NativeOut {
    match output {
        Output::Str(s) => NativeOut::Str(s),
        Output::Bool(b) => NativeOut::Scalar(Scalar::Bool(b)),
        Output::Int(n) => NativeOut::Scalar(Scalar::Int(n)),
        Output::Float(f) => NativeOut::Scalar(Scalar::Float(f)),
        Output::StrList(items) => str_list(items),
        Output::Bytes(data) => NativeOut::Bytes(data),
        Output::OptStr(opt) => match opt {
            Some(s) => NativeOut::Some(Box::new(NativeOut::Str(s))),
            None => NativeOut::None,
        },
        Output::OptInt(opt) => match opt {
            Some(n) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(n)))),
            None => NativeOut::None,
        },
        Output::OptFloat(opt) => match opt {
            Some(f) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Float(f)))),
            None => NativeOut::None,
        },
    }
}

fn math_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let projected: Vec<Arg> = args.iter().map(to_arg).collect();
    match math::call(func, &projected) {
        Dispatch::Done(output) => Ok(from_output(output)),
        Dispatch::Err(error) => Err(error),
        Dispatch::Unknown => Err(no_function_error("math", func)),
    }
}

// --- `random`: seeded PRNG, host-owned state ----------------------------------------------------

fn random_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "seed" => {
            want_arity(func, args, 1)?;
            host.rng_seed(want_int(func, args, 0)?);
            Ok(NativeOut::Unit)
        }
        "int" => {
            want_arity(func, args, 2)?;
            let lo = want_int(func, args, 0)?;
            let hi = want_int(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.rng_int(lo, hi)?)))
        }
        "float" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Float(host.rng_float())))
        }
        _ => Err(no_function_error("random", func)),
    }
}

// --- `time`: logical monotonic clock ------------------------------------------------------------

fn time_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "monotonic" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(
                Scalar::Int(host.clock_monotonic() as i64),
            ))
        }
        "sleep" => {
            want_arity(func, args, 1)?;
            host.clock_sleep(want_int(func, args, 0)?);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_function_error("time", func)),
    }
}

// --- `id`: sequential ids + UUIDs (id-entropy U2) ------------------------------------------------

fn id_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "next_id" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.id_next() as i64)))
        }
        "uuid" => {
            want_arity(func, args, 0)?;
            let u = crate::id::v4(host.entropy_u64(), host.entropy_u64());
            Ok(NativeOut::Extern(crate::ExternBox::new(u)))
        }
        "uuid_v7" => {
            want_arity(func, args, 0)?;
            let ms = host.clock_unix_ms();
            let ra = host.entropy_u64();
            let rb = host.entropy_u64();
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::v7(
                ms, ra, rb,
            ))))
        }
        // `parse(s) -> Uuid?`: any RFC form the crate accepts; `none` on malformed input (the
        // Option is the error channel — parse failure is an ordinary outcome, not a panic).
        "parse" => {
            want_arity(func, args, 1)?;
            let NativeValue::Str(s) = &args[0] else {
                return Err(type_error(func, "string"));
            };
            Ok(match uuid::Uuid::parse_str(s) {
                Ok(u) => NativeOut::Some(Box::new(NativeOut::Extern(crate::ExternBox::new(
                    crate::id::Uuid(u),
                )))),
                Err(_) => NativeOut::None,
            })
        }
        "uuid_v5" => {
            want_arity(func, args, 2)?;
            let Some(NativeValue::Extern(ns_box)) = args.first() else {
                return Err(type_error(func, "Uuid"));
            };
            let Some(ns) = ns_box.as_any().downcast_ref::<crate::id::Uuid>() else {
                return Err(type_error(func, "Uuid"));
            };
            let name = want_str(func, args, 1)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::v5(
                ns, name,
            ))))
        }
        "namespace_dns" | "namespace_url" | "namespace_oid" | "namespace_x500" => {
            want_arity(func, args, 0)?;
            let ns = match func {
                "namespace_dns" => uuid::Uuid::NAMESPACE_DNS,
                "namespace_url" => uuid::Uuid::NAMESPACE_URL,
                "namespace_oid" => uuid::Uuid::NAMESPACE_OID,
                _ => uuid::Uuid::NAMESPACE_X500,
            };
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::Uuid(
                ns,
            ))))
        }
        _ => Err(no_function_error("id", func)),
    }
}

// --- `crypto`: digests, HMAC (crypto arc C2) -----------------------------------------------------

/// A digest input: a string hashes as its UTF-8 bytes, a `bytes` buffer as-is.
const STR_OR_BYTES: SigType = SigType::Union(&[SigType::String, SigType::Bytes]);

/// Project a `string|bytes` argument onto the byte view the digest functions consume.
fn want_data<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a [u8], StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s.as_bytes()),
        Some(NativeValue::Bytes(b)) => Ok(b),
        _ => Err(type_error(func, "string|bytes")),
    }
}

/// An HMAC tag argument — `bytes` only (a tag is raw bytes; a "string tag" is a smell).
fn want_tag<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a [u8], StdError> {
    match args.get(index) {
        Some(NativeValue::Bytes(b)) => Ok(b),
        _ => Err(type_error(func, "bytes")),
    }
}

// --- `base64`: RFC 4648 encode/decode over `bytes` -----------------------------------------------
//
// Four functions rather than two plus alphabet/padding flags: the name states the wire format, so a
// call site documents which envelope it speaks and a mistake is visible in review instead of
// surfacing as a token the remote party silently rejects. `encode`/`decode` are the standard
// `+/`-alphabet, `=`-padded form (RFC 4648 §4, and what §10's vectors show); `encode_url`/
// `decode_url` are the `-_` URL-safe form with no padding (§5, as RFC 7515 requires for JWTs).
// Decoding is recoverable — base64 off a wire is untrusted input exactly like JSON — so both decode
// doors return `Result<bytes, Base64Error>` and never abort. See `crate::base64` for the alphabet-
// strict / padding-indifferent rule and the reasoning behind offering url-safe at all.

/// `Base64Error`'s signature spelling — the error arm of both decode doors.
const BASE64_ERROR_SIG: SigType = SigType::Named(crate::base64::BASE64_ERROR_TYPE_NAME);

/// What both decode doors return: `Result<bytes, Base64Error>`.
const BASE64_RESULT_SIG: SigType = SigType::Result(&SigType::Bytes, &BASE64_ERROR_SIG);

const BASE64_FNS: &[ExtFn] = &[
    // Encoding accepts `string|bytes` like the `crypto` digests: a string encodes as its UTF-8
    // bytes, which is what a caller inlining text into a data URI or an auth header wants, and
    // spares the `.to_bytes()` ceremony in the overwhelmingly common case.
    ExtFn {
        param_names: &["data"],
        name: "encode",
        params: &[STR_OR_BYTES],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["data"],
        name: "encode_url",
        params: &[STR_OR_BYTES],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["text"],
        name: "decode",
        params: &[Str],
        ret: Concrete(BASE64_RESULT_SIG),
    },
    ExtFn {
        param_names: &["text"],
        name: "decode_url",
        params: &[Str],
        ret: Concrete(BASE64_RESULT_SIG),
    },
];

fn base64_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    // Both decode doors are recoverable: the whole `Result` rides inside the `NativeOut` and the
    // `Err` channel (a runtime abort) is never used — the same contract `json.try_parse` honors.
    let decoded = |result: Result<Vec<u8>, crate::base64::Base64Error>| match result {
        Ok(bytes) => NativeOut::Ok(Box::new(NativeOut::Bytes(bytes))),
        Err(error) => NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error)))),
    };
    match func {
        "encode" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(crate::base64::encode(want_data(
                func, args, 0,
            )?)))
        }
        "encode_url" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(crate::base64::encode_url(want_data(
                func, args, 0,
            )?)))
        }
        "decode" => {
            want_arity(func, args, 1)?;
            Ok(decoded(crate::base64::decode(want_str(func, args, 0)?)))
        }
        "decode_url" => {
            want_arity(func, args, 1)?;
            Ok(decoded(crate::base64::decode_url(want_str(func, args, 0)?)))
        }
        _ => Err(no_function_error("base64", func)),
    }
}

const BASE64_DOCS: &[(&str, &str)] = &[
    (
        "encode",
        "Encode bytes (or a string, as its UTF-8 bytes) with the **standard** RFC 4648 alphabet, \
         `=`-padded — the canonical form, and what an LLM provider's inline image/file field and an \
         MCP resource's `blob` expect.",
    ),
    (
        "encode_url",
        "Encode with the **URL-safe** alphabet (`-`/`_`) and no padding — RFC 4648 §5 as RFC 7515 \
         requires it, so the result is safe in a URL path, a query parameter, or a filename. This \
         is the JWT-segment spelling.",
    ),
    (
        "decode",
        "Decode **standard**-alphabet base64 into `bytes`: `Ok(bytes)`, or `Err(Base64Error)` \
         naming the failure and its `offset()`. Never aborts — base64 from a remote party is \
         untrusted input exactly like JSON.\n\n\
         Padded or unpadded input both decode; the *alphabet* is strict, so `-`/`_` is rejected \
         here (reach for `decode_url`). Non-canonical trailing bits are rejected too, so a \
         successful decode always re-encodes to the same text.",
    ),
    (
        "decode_url",
        "Decode **URL-safe**-alphabet base64 into `bytes` — the `encode_url` inverse, and the door \
         for a JWT segment. Same recoverable `Result<bytes, Base64Error>` contract as `decode`, and \
         it rejects `+`/`/`: that strictness is why this is a real function rather than a character \
         substitution you apply afterwards, which would accept a mixed-alphabet token.",
    ),
];

/// The `Base64Error` instance methods: pure reads over the decode failure — the `JsonError`
/// accessor model. `message` is `impl Error`'s required method and `to_string` is `impl Display`'s
/// (both declared on the type's registration), and both return the same composed message the value
/// also displays as.
const BASE64_ERROR_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "message",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "kind",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "offset",
        params: &[],
        ret: Concrete(SigType::Option(&Int)),
    },
];

const BASE64_ERROR_DOCS: &[(&str, &str)] = &[
    (
        "message",
        "The composed human message (`invalid url-safe base64 character '+' at offset 3`). The \
         `Error` trait's required method.",
    ),
    (
        "to_string",
        "Same as `message()` — the `Display` rendering, so `${e}` interpolates the message.",
    ),
    (
        "kind",
        "What went wrong: `\"invalid_character\"` (a character outside this door's alphabet), \
         `\"invalid_length\"` (a truncated group), `\"invalid_last_symbol\"` (non-canonical \
         trailing bits), or `\"invalid_padding\"`.",
    ),
    (
        "offset",
        "The 0-based byte offset into the encoded text where the failure was detected, or `none` \
         when the failure is not positional (malformed padding).",
    ),
];

fn base64_error_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::base64::{BASE64_ERROR_TYPE_NAME, Base64Error};
    let Some(error) = recv.as_any().downcast_ref::<Base64Error>() else {
        return Err(type_error(method, BASE64_ERROR_TYPE_NAME));
    };
    want_arity(method, args, 0)?;
    match method {
        // `message` (Error) and `to_string` (Display) are the same composed message by design.
        "message" | "to_string" => Ok(NativeOut::Str(error.message())),
        "kind" => Ok(NativeOut::Str(error.kind.label().to_string())),
        "offset" => Ok(match error.offset {
            Some(at) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(i64::from(at))))),
            None => NativeOut::None,
        }),
        _ => Err(crate::no_method_error(BASE64_ERROR_TYPE_NAME, method)),
    }
}

const CRYPTO_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["data"],
        name: "sha256",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        param_names: &["data"],
        name: "sha512",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    // Interop-only digests (UUID v5, legacy checksums) — documented as not collision-resistant.
    ExtFn {
        param_names: &["data"],
        name: "sha1",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        param_names: &["data"],
        name: "md5",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        param_names: &["key", "message"],
        name: "hmac_sha256",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        param_names: &["key", "message"],
        name: "hmac_sha512",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    // Constant-time verification (C7): tag comparison must not short-circuit like `bytes ==`.
    ExtFn {
        param_names: &["key", "message", "tag"],
        name: "hmac_sha256_verify",
        params: &[STR_OR_BYTES, STR_OR_BYTES, SigType::Bytes],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["key", "message", "tag"],
        name: "hmac_sha512_verify",
        params: &[STR_OR_BYTES, STR_OR_BYTES, SigType::Bytes],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "constant_time_eq",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bool),
    },
    // Incremental hashing (C3): per-algorithm constructors, one `Hasher` type.
    ExtFn {
        param_names: &[],
        name: "sha256_hasher",
        params: &[],
        ret: Concrete(HASHER_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "sha512_hasher",
        params: &[],
        ret: Concrete(HASHER_SIG),
    },
    // Password hashing + crypto-grade randomness (C4) — the module's Host-entropy corner.
    ExtFn {
        param_names: &["password", "cost"],
        name: "bcrypt_hash",
        params: &[Str, Int],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["password", "hash"],
        name: "bcrypt_verify",
        params: &[Str, Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["count"],
        name: "random_bytes",
        params: &[Int],
        ret: Concrete(SigType::Bytes),
    },
];

/// The `Hasher` signature type, named once.
const HASHER_SIG: SigType = SigType::Named(crate::crypto::HASHER_TYPE_NAME);

/// The `Hasher` instance methods (crypto C3): `update` is the mutable + host-free seam corner —
/// it mutates the receiver through the shared cell and never touches the Host; `digest` is a
/// non-destructive read (interim digests keep flowing).
const HASHER_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["data"],
        name: "update",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &[],
        name: "digest",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
];

fn hasher_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(hasher) = recv.as_any_mut().downcast_mut::<crate::crypto::Hasher>() else {
        return Err(type_error(method, "Hasher"));
    };
    match method {
        "update" => {
            want_arity(method, args, 1)?;
            hasher.update(want_data(method, args, 0)?);
            Ok(NativeOut::Unit)
        }
        "digest" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(hasher.digest()))
        }
        _ => Err(crate::no_method_error(
            crate::crypto::HASHER_TYPE_NAME,
            method,
        )),
    }
}

fn crypto_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "sha256" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha256(want_data(
                func, args, 0,
            )?)))
        }
        "sha512" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha512(want_data(
                func, args, 0,
            )?)))
        }
        "sha1" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha1(want_data(
                func, args, 0,
            )?)))
        }
        "md5" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::md5(want_data(
                func, args, 0,
            )?)))
        }
        "hmac_sha256" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Bytes(crate::crypto::hmac_sha256(
                want_data(func, args, 0)?,
                want_data(func, args, 1)?,
            )))
        }
        "hmac_sha512" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Bytes(crate::crypto::hmac_sha512(
                want_data(func, args, 0)?,
                want_data(func, args, 1)?,
            )))
        }
        "hmac_sha256_verify" => {
            want_arity(func, args, 3)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::hmac_sha256_verify(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                    want_tag(func, args, 2)?,
                ),
            )))
        }
        "hmac_sha512_verify" => {
            want_arity(func, args, 3)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::hmac_sha512_verify(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                    want_tag(func, args, 2)?,
                ),
            )))
        }
        "constant_time_eq" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::constant_time_eq(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                ),
            )))
        }
        "sha256_hasher" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::crypto::Hasher::Sha256(Default::default()),
            )))
        }
        "sha512_hasher" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::crypto::Hasher::Sha512(Default::default()),
            )))
        }
        "bcrypt_hash" => {
            want_arity(func, args, 2)?;
            let password = want_str(func, args, 0)?;
            let cost = want_int(func, args, 1)?;
            // The salt is the effectful input: two Entropy words, drawn here at the seam so
            // `crypto::bcrypt_hash` itself stays pure (and unit-testable against pinned salts).
            let mut salt = [0u8; 16];
            salt[..8].copy_from_slice(&host.entropy_u64().to_be_bytes());
            salt[8..].copy_from_slice(&host.entropy_u64().to_be_bytes());
            Ok(NativeOut::Str(crate::crypto::bcrypt_hash(
                password, cost, salt,
            )?))
        }
        "bcrypt_verify" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::bcrypt_verify(want_str(func, args, 0)?, want_str(func, args, 1)?)?,
            )))
        }
        "random_bytes" => {
            want_arity(func, args, 1)?;
            let n = want_int(func, args, 0)?;
            if n < 0 {
                return Err(StdError {
                    kind: crate::ErrorKind::ArgType,
                    message: format!("`crypto.random_bytes` count must be non-negative, got {n}"),
                });
            }
            let n = n as usize;
            let mut out = Vec::with_capacity(n.next_multiple_of(8));
            while out.len() < n {
                out.extend_from_slice(&host.entropy_u64().to_be_bytes());
            }
            out.truncate(n);
            Ok(NativeOut::Bytes(out))
        }
        _ => Err(no_function_error("crypto", func)),
    }
}

// --- `http`: an HTTP client over the Network capability (http arc H2) ----------------------------

/// The `Response` signature type, named once.
use crate::serve::REQUEST_SIG;

const RESPONSE_SIG: SigType = SigType::Named(crate::net::RESPONSE_TYPE_NAME);
const COOKIE_SIG: SigType = SigType::Named(crate::cookie::COOKIE_TYPE_NAME);
const KEYRING_SIG: SigType = SigType::Named(crate::session::KEYRING_TYPE_NAME);
const SESSION_SIG: SigType = SigType::Named(crate::session::SESSION_TYPE_NAME);
/// `Map<string, string>` — a session's data. Named so `Option<&…>` can borrow it.
const SESSION_DATA_SIG: SigType = SigType::Map(&Str, &Str);
const HTTP_ERROR_SIG: SigType = SigType::Named(crate::net::HTTP_ERROR_TYPE_NAME);

/// What every client verb returns (http arc H6): `Result<Response, HttpError>`.
///
/// The split is deliberate and load-bearing. A **transport** failure — the request never produced
/// a response — is the `Err`, so `?` on a request means exactly "the network broke". An HTTP error
/// **status** is an ordinary `Ok(Response)`: a 404 is an answer, and folding it into `Err` is the
/// `http_errors` flag that confuses everyone in Guzzle. Callers opt into status-as-error with
/// `resp.error_for_status()?`.
const RESPONSE_RESULT_SIG: SigType = SigType::Result(&RESPONSE_SIG, &HTTP_ERROR_SIG);

/// The `Framing` choice a `stream` call cuts with (http-streaming arc).
const FRAMING_SIG: SigType = SigType::Named(noeta_ext_abi::stream::FRAMING_TYPE_NAME);
/// The open incremental reader.
const FRAME_STREAM_SIG: SigType = SigType::Named(noeta_ext_abi::stream::FRAME_STREAM_TYPE_NAME);
/// The event-stream sink a `server.sse` handler writes to.
pub(crate) const SSE_SINK_SIG: SigType = SigType::Named(noeta_ext_abi::stream::SSE_SINK_TYPE_NAME);

/// What `stream` returns: the same `Err` door the one-shot verbs use, so `?` on opening a stream
/// means exactly "the request never got off the ground" — an HTTP error *status* is still a
/// successfully opened stream whose body the caller reads (an error page streams like anything
/// else), matching `Ok(Response)` for a 404.
const FRAME_STREAM_RESULT_SIG: SigType = SigType::Result(&FRAME_STREAM_SIG, &HTTP_ERROR_SIG);

/// A request-headers argument type — `Map<string, string>`, named once.
const HEADERS: SigType = SigType::Map(&SigType::String, &SigType::String);
/// The optional trailing `headers` parameter every verb accepts (http arc H5).
const OPT_HEADERS: SigType = SigType::Optional(&HEADERS);
/// The optional `body` parameter of the `http.response` builder (http-server S2).
const OPT_BODY: SigType = SigType::Optional(&STR_OR_BYTES);

/// The `http` surface. Bodyless verbs take a url; `post`/`put`/`query` take a `string|bytes` body;
/// `request(method, url)` covers any other (bodyless) verb. **Every** verb accepts an optional
/// trailing `headers: Map<string, string>` (H5, via the registry's optional-param support). All
/// return a `Response`; the `*_async` twins return `Future<Response>` (H3) and drive a real
/// reqwest future on the real host. `query` is the RFC-draft HTTP QUERY method — safe, idempotent,
/// body-carrying. Each performs the request through the Host (deterministic sandbox, real under
/// `noeta run`). Timeouts are a deferred follow-on.
/// The outbound-client functions of `std.http.client` — each pulls the reqwest/TLS ring. Split out
/// of the former single `http` module (package-manager P0.3b) so a whole-module `use std.http.client`
/// is precisely the client-ring signal, and `use std.http.server` sheds reqwest entirely.
const HTTP_CLIENT_FNS: &[ExtFn] = &[
    // The configured door (http arc H7): `client.new(base?)` mints a `Client` carrying base URL,
    // headers, auth, and a deadline. The free verbs below stay the one-shot door.
    ExtFn {
        param_names: &["base"],
        name: "new",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "get",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "head",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "delete",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "post",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "put",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "query",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["method", "url", "headers"],
        name: "request",
        params: &[Str, Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "get_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "head_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["url", "headers"],
        name: "delete_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "post_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "put_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["url", "body", "headers"],
        name: "query_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    ExtFn {
        param_names: &["method", "url", "headers"],
        name: "request_async",
        params: &[Str, Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_RESULT_SIG)),
    },
    // `stream(req, framing)` (http-streaming arc) — read a response body INCREMENTALLY instead of
    // buffering it whole.
    //
    // It takes a prepared `Request` rather than a url, because a streaming call is always a
    // configured one in practice (a base URL, an auth header, a JSON body naming the model) and
    // `prepare`/`send` is already the seam where std hands off to user code.
    //
    // Synchronous, returning `Result<FrameStream, HttpError>`: the call sends the request and reads
    // the response HEAD, which is the last moment a failure is still a transport error the caller
    // can handle. Everything after that is body, and a body failure surfaces as the stream ending.
    ExtFn {
        param_names: &["req", "framing"],
        name: "stream",
        params: &[REQUEST_SIG, FRAMING_SIG],
        ret: Concrete(FRAME_STREAM_RESULT_SIG),
    },
];

/// `std.session` — the signed-cookie session surface (session arc S2/S3).
///
/// Two layers in one module. `keyring`/`encode`/`decode` are the pure codec: no HTTP, so they also
/// serve any other signed-token need (a one-click unsubscribe link, a CSRF token). `open`/`attach`
/// are the HTTP convenience over it, so a handler never touches a token or a cookie by hand.
const SESSION_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["secrets"],
        name: "keyring",
        params: &[SigType::List(&Str)],
        ret: Concrete(KEYRING_SIG),
    },
    ExtFn {
        param_names: &["data", "keys", "max_age"],
        name: "encode",
        params: &[SESSION_DATA_SIG, KEYRING_SIG, Int],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["token", "keys"],
        name: "decode",
        params: &[Str, KEYRING_SIG],
        ret: Concrete(SigType::Option(&SESSION_DATA_SIG)),
    },
    // Build a clean (unchanged) session from data — the inverse of `data()`. A stored backend uses
    // it to rebuild the `Session` currency from a row it read, so a handler consumes a stored
    // session exactly as it does a cookie one. Clean so a pure read never provokes a row write.
    ExtFn {
        param_names: &["data"],
        name: "of",
        params: &[SESSION_DATA_SIG],
        ret: Concrete(SESSION_SIG),
    },
    // Read the session off a request. Never fails: an absent, forged, or expired cookie all yield
    // an empty session, because a caller has one correct response to all three.
    ExtFn {
        param_names: &["request", "keys"],
        name: "open",
        params: &[REQUEST_SIG, KEYRING_SIG],
        ret: Concrete(SESSION_SIG),
    },
    // Write it back — but only when it changed, so an unchanged session costs no header and does
    // not silently extend its own expiry on every request.
    //
    // `secure` has no default deliberately. Defaulting it on breaks every plain-http localhost
    // server with a cookie the browser silently refuses to store; defaulting it off ships session
    // credentials over cleartext. Both failures are quiet, so the choice is the caller's to make
    // out loud: `true` in production, `false` only for local development.
    ExtFn {
        param_names: &["response", "session", "keys", "max_age", "secure"],
        name: "attach",
        params: &[RESPONSE_SIG, SESSION_SIG, KEYRING_SIG, Int, SigType::Bool],
        ret: Concrete(RESPONSE_SIG),
    },
];

fn session_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "keyring" => {
            want_arity(func, args, 1)?;
            let Some(NativeValue::List(items)) = args.first() else {
                return Err(type_error(func, "List<string>"));
            };
            let mut secrets = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    NativeValue::Str(s) => secrets.push(s.as_bytes().to_vec()),
                    _ => return Err(type_error(func, "List<string>")),
                }
            }
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::session::Keyring::new(secrets)?,
            )))
        }
        "encode" => {
            want_arity(func, args, 3)?;
            let data = want_session_data(func, args, 0)?;
            let keys = want_keyring(func, args, 1)?;
            let max_age = want_int(func, args, 2)?;
            Ok(NativeOut::Str(crate::session::encode(
                &data,
                keys,
                max_age,
                host.clock_unix_ms(),
            )?))
        }
        "decode" => {
            want_arity(func, args, 2)?;
            let token = want_str(func, args, 0)?;
            let keys = want_keyring(func, args, 1)?;
            Ok(
                match crate::session::decode(token, keys, host.clock_unix_ms()) {
                    Some(data) => NativeOut::Some(Box::new(session_data_out(&data))),
                    None => NativeOut::None,
                },
            )
        }
        "open" => {
            want_arity(func, args, 2)?;
            let request = want_request(func, args, 0)?;
            let keys = want_keyring(func, args, 1)?;
            let header = crate::net::request_header(&request.inner, "cookie").unwrap_or_default();
            let token = crate::cookie::parse_cookie_header(header)
                .into_iter()
                .find(|(name, _)| name == crate::session::COOKIE_NAME)
                .map(|(_, value)| value);
            let data = token
                .and_then(|t| crate::session::decode(&t, keys, host.clock_unix_ms()))
                .unwrap_or_default();
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::session::Session {
                    data,
                    dirty: false,
                    id: None,
                },
            )))
        }
        "of" => {
            want_arity(func, args, 1)?;
            let data = want_session_data(func, args, 0)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::session::Session::of(data),
            )))
        }
        "attach" => {
            want_arity(func, args, 5)?;
            let resp = want_response(func, args, 0)?;
            let session = want_session(func, args, 1)?;
            let keys = want_keyring(func, args, 2)?;
            let max_age = want_int(func, args, 3)?;
            let secure = want_bool(func, args, 4)?;
            if !session.dirty {
                return Ok(NativeOut::Extern(crate::ExternBox::new(resp.clone())));
            }
            // An emptied session is a logout: overwrite the cookie with its expired form rather
            // than emitting a valid token for empty data, so the browser drops it immediately.
            let cookie = if session.data.is_empty() {
                crate::cookie::Cookie::new(crate::session::COOKIE_NAME, "")?
                    .with_secure(secure)?
                    .expired()
            } else {
                let token =
                    crate::session::encode(&session.data, keys, max_age, host.clock_unix_ms())?;
                crate::cookie::Cookie::new(crate::session::COOKIE_NAME, &token)?
                    .with_max_age(max_age)
                    .with_secure(secure)?
            };
            let mut next = resp.clone();
            next.headers.retain(|(k, v)| {
                !k.eq_ignore_ascii_case("set-cookie")
                    || !crate::cookie::header_sets_cookie_named(v, crate::session::COOKIE_NAME)
            });
            next.headers
                .push(("set-cookie".to_string(), cookie.to_header()));
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        _ => Err(no_function_error("session", func)),
    }
}

/// Read a `Map<string, string>` argument as session data.
fn want_session_data(
    func: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<std::collections::BTreeMap<String, String>, StdError> {
    let Some(NativeValue::Map(entries)) = args.get(index) else {
        return Err(type_error(func, "Map<string, string>"));
    };
    entries
        .iter()
        .map(|(k, v)| match v {
            NativeValue::Str(s) => Ok((k.clone(), s.clone())),
            _ => Err(type_error(func, "Map<string, string>")),
        })
        .collect()
}

/// Marshal session data back out as a `Map<string, string>`.
fn session_data_out(data: &std::collections::BTreeMap<String, String>) -> NativeOut {
    NativeOut::Map(
        data.iter()
            .map(|(k, v)| (k.clone(), NativeOut::Str(v.clone())))
            .collect(),
    )
}

fn want_keyring<'a>(
    func: &str,
    args: &'a [NativeValue],
    index: usize,
) -> Result<&'a crate::session::Keyring, StdError> {
    let Some(NativeValue::Extern(value)) = args.get(index) else {
        return Err(type_error(func, crate::session::KEYRING_TYPE_NAME));
    };
    value
        .as_any()
        .downcast_ref::<crate::session::Keyring>()
        .ok_or_else(|| type_error(func, crate::session::KEYRING_TYPE_NAME))
}

fn want_session<'a>(
    func: &str,
    args: &'a [NativeValue],
    index: usize,
) -> Result<&'a crate::session::Session, StdError> {
    let Some(NativeValue::Extern(value)) = args.get(index) else {
        return Err(type_error(func, crate::session::SESSION_TYPE_NAME));
    };
    value
        .as_any()
        .downcast_ref::<crate::session::Session>()
        .ok_or_else(|| type_error(func, crate::session::SESSION_TYPE_NAME))
}

fn want_request<'a>(
    func: &str,
    args: &'a [NativeValue],
    index: usize,
) -> Result<&'a crate::net::Request, StdError> {
    let Some(NativeValue::Extern(value)) = args.get(index) else {
        return Err(type_error(func, crate::net::REQUEST_TYPE_NAME));
    };
    value
        .as_any()
        .downcast_ref::<crate::net::Request>()
        .ok_or_else(|| type_error(func, crate::net::REQUEST_TYPE_NAME))
}

fn want_response<'a>(
    func: &str,
    args: &'a [NativeValue],
    index: usize,
) -> Result<&'a crate::NetResponse, StdError> {
    let Some(NativeValue::Extern(value)) = args.get(index) else {
        return Err(type_error(func, crate::net::RESPONSE_TYPE_NAME));
    };
    value
        .as_any()
        .downcast_ref::<crate::NetResponse>()
        .ok_or_else(|| type_error(func, crate::net::RESPONSE_TYPE_NAME))
}

/// The server-side functions of `std.http.server`: the pure `response` builder (status + optional
/// body/headers). `serve` (the inbound accept loop, a higher-order orchestrator) is the module's
/// ctx function. None of these pull reqwest — a `use std.http.server` program links no client ring.
const HTTP_SERVER_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["status", "body", "headers"],
        name: "response",
        params: &[Int, OPT_BODY, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    // The `Set-Cookie` builder (cookie arc C1). Server-side: a client sends cookies back through
    // the `Cookie:` header, which `Request.cookies()` reads, so only the reply side builds one.
    ExtFn {
        param_names: &["name", "value"],
        name: "cookie",
        params: &[Str, Str],
        ret: Concrete(COOKIE_SIG),
    },
    // The form/percent codec as free functions, for a caller holding a body string rather than a
    // `Request` — a websocket session delivering a client event, a queue consumer, a test. Same
    // parser as `Request.form_all()`; exposing it here is what keeps every consumer from
    // hand-rolling percent-decoding.
    // Build an INBOUND `Request` without a server (named `incoming` because the client module's
    // `request` verb is an outbound call, and the two share one dispatch). The serve loop is
    // otherwise the only source of one, so a
    // handler taking a `Request` — or any framework routing on one — could not be exercised from a
    // test or a script. Carries no connection, so replying to it is a no-op rather than traffic to
    // a live socket.
    ExtFn {
        param_names: &["method", "url", "body", "headers"],
        name: "incoming",
        params: &[Str, Str, OPT_BODY, OPT_HEADERS],
        ret: Concrete(REQUEST_SIG),
    },
    ExtFn {
        param_names: &["body"],
        name: "parse_form",
        params: &[Str],
        ret: Concrete(SigType::Map(&Str, &Str)),
    },
];

/// Read the optional `headers: Map<string, string>` argument at `index`, or an empty list if the
/// call omitted it (http arc H5). The `http` module is `deep_marshal`, so the map arrives as a
/// [`NativeValue::Map`]; the checker has already typed the values as strings.
fn want_headers(
    func: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<Vec<(String, String)>, StdError> {
    match args.get(index) {
        None => Ok(Vec::new()),
        Some(NativeValue::Map(entries)) => entries
            .iter()
            .map(|(k, v)| match v {
                NativeValue::Str(value) => Ok((k.clone(), value.clone())),
                _ => Err(type_error(func, "map of string to string")),
            })
            .collect(),
        Some(_) => Err(type_error(func, "map of string to string")),
    }
}

/// Assemble the request the sync and async paths share.
fn http_request(
    method: &str,
    url: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
) -> crate::NetRequest {
    crate::NetRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers,
        body,
        // The free verbs carry no deadline — a timeout is configuration, and configuration lives
        // on a `Client` (`client.new(base).timeout(ms)`). Redirects are the same: `None` is the
        // default limit, and a caller who wants another one asks a `Client` for it.
        timeout_ms: None,
        redirect_limit: None,
    }
}

/// `std.http.url` — the percent-encoder/decoder (RFC 3986). Pure and host-free: no request is
/// performed, so this module links nothing and is safe in any ring.
const HTTP_URL_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["value"],
        name: "encode",
        params: &[Str],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["value"],
        name: "decode",
        params: &[Str],
        ret: Concrete(Str),
    },
];

/// `std.http.url`'s dispatch. Its own rather than an arm of [`http_dispatch`], because neither
/// function touches the [`Host`]: they are string transformations, and routing them through the
/// request builder would imply otherwise.
fn http_url_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    want_arity(func, args, 1)?;
    let value = want_str(func, args, 0)?;
    match func {
        "encode" => Ok(NativeOut::Str(crate::url::encode(value))),
        "decode" => Ok(NativeOut::Str(crate::url::decode(value))),
        _ => Err(no_function_error("http.url", func)),
    }
}

fn http_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    // The server-side response builder (http-server S2) — constructs a value, no request/fetch.
    if func == "response" {
        want_arity_range(func, args, 1, 3)?;
        let status = want_int(func, args, 0)?;
        if !(100..=599).contains(&status) {
            return Err(type_error(func, "an HTTP status code in 100..=599"));
        }
        let body = match args.get(1) {
            None => Vec::new(),
            Some(_) => want_data(func, args, 1)?.to_vec(),
        };
        let headers = want_headers(func, args, 2)?;
        return Ok(NativeOut::Extern(crate::ExternBox::new(
            crate::NetResponse {
                status: status as u16,
                headers,
                body,
                url: String::new(), // a server-built reply has no originating URL
            },
        )));
    }
    // The cookie constructor (cookie arc C1) — pure, and validating: an invalid name or value is
    // refused here so `Cookie.to_header` is total and no reply can be split by a crafted value.
    if func == "cookie" {
        want_arity(func, args, 2)?;
        let name = want_str(func, args, 0)?;
        let value = want_str(func, args, 1)?;
        return Ok(NativeOut::Extern(crate::ExternBox::new(
            crate::cookie::Cookie::new(name, value)?,
        )));
    }
    // The form/percent codec — pure string transforms over the same parser `Request.form_all()`
    // uses, for callers that hold a body rather than a request.
    if func == "incoming" {
        want_arity_range(func, args, 2, 4)?;
        let method = want_str(func, args, 0)?.to_ascii_uppercase();
        let url = want_str(func, args, 1)?.to_string();
        let body = match args.get(2) {
            None => Vec::new(),
            Some(_) => want_data(func, args, 2)?.to_vec(),
        };
        return Ok(NativeOut::Extern(crate::ExternBox::new(
            crate::net::Request {
                conn: None,
                inner: crate::NetRequest {
                    method,
                    url,
                    headers: want_headers(func, args, 3)?,
                    body,
                    // Configuration is layered on at `send`/`stream` time by whichever client
                    // spends this request, so a prepared request carries none of its own.
                    timeout_ms: None,
                    redirect_limit: None,
                },
            },
        )));
    }
    if func == "parse_form" {
        want_arity(func, args, 1)?;
        return Ok(NativeOut::Map(
            crate::net::form_pairs(want_str(func, args, 0)?)
                .into_iter()
                .map(|(name, value)| (name, NativeOut::Str(value)))
                .collect(),
        ));
    }
    // The configured-client constructor (http arc H7) — pure, no request performed.
    if func == "new" {
        want_arity_range(func, args, 0, 1)?;
        let base = match args.first() {
            None => "",
            Some(_) => want_str(func, args, 0)?,
        };
        return Ok(NativeOut::Extern(crate::ExternBox::new(
            crate::http_client::HttpClient::new(base),
        )));
    }
    // `stream(req, framing)` (http-streaming arc) — open the body incrementally. Unlike the verbs
    // below it takes a prepared request, so there is nothing to build.
    if func == "stream" {
        want_arity(func, args, 2)?;
        let request = want_request(func, args, 0)?.inner.clone();
        let framing = want_framing(func, args, 1)?;
        return Ok(stream_outcome(host.net_stream_open(request, framing)));
    }
    // Build the request from the call, per verb shape. Bodyless verbs put headers at index 1;
    // body-carrying verbs and `request` put them at index 2. The method is uppercased so
    // `request("get", …)` and any custom verb (QUERY) normalize.
    let verb = func.trim_end_matches("_async");
    let request = match verb {
        "get" | "head" | "delete" => {
            want_arity_range(func, args, 1, 2)?;
            http_request(
                &verb.to_ascii_uppercase(),
                want_str(func, args, 0)?,
                Vec::new(),
                want_headers(func, args, 1)?,
            )
        }
        "post" | "put" | "query" => {
            want_arity_range(func, args, 2, 3)?;
            let url = want_str(func, args, 0)?.to_string();
            let body = want_data(func, args, 1)?.to_vec();
            http_request(
                &verb.to_ascii_uppercase(),
                &url,
                body,
                want_headers(func, args, 2)?,
            )
        }
        "request" => {
            want_arity_range(func, args, 2, 3)?;
            let method = want_str(func, args, 0)?.to_ascii_uppercase();
            let url = want_str(func, args, 1)?.to_string();
            http_request(&method, &url, Vec::new(), want_headers(func, args, 2)?)
        }
        _ => return Err(no_function_error("http", func)),
    };
    // Sync verbs fetch through the Host now; `*_async` hand the host its async descriptor to
    // ticket on the executor (H3).
    if func.ends_with("_async") {
        Ok(NativeOut::Spawn(SpawnBox(host.net_spawn(request))))
    } else {
        // A transport failure is `Err(HttpError)`, never a `StdError` abort (http arc H6) — the
        // caller decides whether to `?` it, retry it, or inspect its `kind()`.
        //
        // Redirects are followed here rather than at the seam, exactly as a `Client` follows them:
        // `net_fetch` is one hop, and a free verb owns a whole request. A free verb carries the
        // default limit and no way to change it — configuration lives on a `Client`.
        Ok(crate::net::fetch_outcome(
            noeta_ext_abi::redirect::follow_redirects(request, |hop| host.net_fetch(hop)),
        ))
    }
}

/// Read a `Framing` argument.
///
/// `Framing` is a real native enum, so a caller writes `Framing.Sse` and the **checker** rejects a
/// typo or a missing `match` arm — that guarantee is static and holds regardless of how the value
/// is projected across the seam.
///
/// One shape reaches this function, and it is the enum: [`NativeValue::Variant`]. This used to
/// accept a bare [`NativeValue::Str`] as well, because the *deep* (JSON-shaped) projection — which
/// `http.client` takes, since its optional `headers` argument is a `Map` — flattened every
/// non-`Option` enum to its variant **name**, so the two projections of the same value disagreed and
/// a reader had to know both. The deep projection now carries the real variant (see
/// `Value::to_native_deep`), so the second shape no longer exists and the string arm is retired with
/// it: the workaround's whole cost was the disagreement it papered over.
fn want_framing(
    func: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<noeta_ext_abi::stream::Framing, StdError> {
    let Some(NativeValue::Variant { variant, .. }) = args.get(index) else {
        return Err(type_error(func, noeta_ext_abi::stream::FRAMING_TYPE_NAME));
    };
    variant
        .parse::<noeta_ext_abi::stream::Framing>()
        .map_err(|()| type_error(func, noeta_ext_abi::stream::FRAMING_TYPE_NAME))
}

/// Marshal a stream-open outcome as `Result<FrameStream, HttpError>` — the
/// [`crate::net::fetch_outcome`] twin, shared by the free function and the `Client` method so both
/// doors return the identical shape.
///
/// The head rides onto the handle here, which is what makes `stream.status()` answerable without a
/// `recv()` — see [`noeta_ext_abi::stream::FrameStream`].
fn stream_outcome(
    result: Result<noeta_ext_abi::stream::StreamHead, noeta_ext_abi::NetError>,
) -> NativeOut {
    match result {
        Ok(head) => NativeOut::Ok(Box::new(NativeOut::Extern(crate::ExternBox::new(
            noeta_ext_abi::stream::FrameStream::new(head),
        )))),
        Err(error) => NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error)))),
    }
}

const CLIENT_SIG: SigType = SigType::Named(crate::http_client::CLIENT_TYPE_NAME);

/// The `Client` instance methods (http arc H7): the immutable configuration chain, then the verbs
/// that spend it.
///
/// Every configuration method returns a **new** `Client` (`-> Client`), the copy-modify shape
/// `Response.with_header` already uses — so a derived client can never mutate the one it came
/// from, and sharing a configured client across a program is safe by construction.
///
/// The verbs mirror the free functions exactly (same names, same optional trailing headers, same
/// `Result<Response, HttpError>`), differing only in that their first argument is a *path*
/// resolved against the base URL rather than a whole URL. An absolute target still wins, so a
/// paginator can hand back an absolute `next` link through a based client.
const CLIENT_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["name", "value"],
        name: "header",
        params: &[Str, Str],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["token"],
        name: "bearer",
        params: &[Str],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["user", "password"],
        name: "basic",
        params: &[Str, Str],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["ms"],
        name: "timeout",
        params: &[Int],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["max", "base_ms", "on"],
        name: "retry",
        params: &[
            Int,
            SigType::Optional(&Int),
            SigType::Optional(&SigType::List(&Int)),
        ],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "retry_non_idempotent",
        params: &[],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &["limit"],
        name: "redirect",
        params: &[Int],
        ret: Concrete(CLIENT_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "base_url",
        params: &[],
        ret: Concrete(Str),
    },
    // Build a request WITHOUT performing it: the value a middleware chain above std starts from.
    // Resolves the path against the base URL and applies the client's headers, so what the outermost
    // middleware sees is the request as configured, not a half-formed one.
    ExtFn {
        param_names: &["method", "path", "body", "headers"],
        name: "prepare",
        params: &[Str, Str, SigType::Optional(&STR_OR_BYTES), OPT_HEADERS],
        ret: Concrete(REQUEST_SIG),
    },
    // The **terminal**: perform an already-built `Request` through this client's configuration —
    // base URL resolution, client headers, deadline, retry. This is the seam a middleware layer
    // above std bottoms out in: compose the onion in Noeta, and `send` is the innermost call. std
    // deliberately does not compose chains itself, because doing so would mean holding user
    // closures inside a native value.
    ExtFn {
        param_names: &["request"],
        name: "send",
        params: &[REQUEST_SIG],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    // The streaming terminal (http-streaming arc) — `send`'s incremental twin, and the door a
    // configured client needs: a streaming call in practice carries a base URL and an auth header,
    // so without this the whole `Client` configuration chain would be unreachable from `stream`.
    //
    // The client's **retry policy is deliberately not applied**. Retrying means re-sending the
    // request and discarding the first attempt's response — coherent for a buffered body, and not
    // for a stream the caller may already have begun reading. The base URL, headers, and deadline
    // all still apply.
    ExtFn {
        param_names: &["request", "framing"],
        name: "stream",
        params: &[REQUEST_SIG, FRAMING_SIG],
        ret: Concrete(FRAME_STREAM_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "headers"],
        name: "get",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "headers"],
        name: "head",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "headers"],
        name: "delete",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "body", "headers"],
        name: "post",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "body", "headers"],
        name: "put",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["path", "body", "headers"],
        name: "query",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &["method", "path", "headers"],
        name: "request",
        params: &[Str, Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
];

const CLIENT_DOCS: &[(&str, &str)] = &[
    (
        "header",
        "A copy of the client with header `name: value` applied to every request. A per-request \
         header of the same name replaces it for that call.",
    ),
    (
        "bearer",
        "A copy of the client sending `Authorization: Bearer <token>`.",
    ),
    (
        "basic",
        "A copy of the client sending HTTP Basic credentials (RFC 7617).",
    ),
    (
        "timeout",
        "A copy of the client with a per-request deadline in milliseconds. Exceeding it is an \
         `HttpError` whose `kind()` is `\"timeout\"` (and `retryable()` is true).",
    ),
    (
        "retry",
        "A copy of the client that retries failed requests: `retry(max, base_ms?, on?)`. \
         Retries a **transient** transport failure (`timeout`/`dns`/`connect`) and any status in \
         `on` (default `[429, 502, 503, 504]`), backing off `base_ms` doubled per attempt \
         (default 250ms, capped at 30s). A server's own `Retry-After` wins over the computed \
         backoff. Only **idempotent** verbs are retried — see `retry_non_idempotent`.",
    ),
    (
        "retry_non_idempotent",
        "Extend the retry policy to POST. Off by default because retrying a request that may \
         already have been applied can duplicate a side effect — a second charge, a second \
         order — and a timeout is exactly the case where the client cannot tell. Opt in when the \
         endpoint is safe to repeat or you send an idempotency key.",
    ),
    (
        "redirect",
        "A copy of the client that follows at most `limit` redirects (10 by default). `0` opts          out: a `301`/`302`/`303`/`307`/`308` comes back as an ordinary response for you to read          its `header(\"location\")`, which is also what you get once a limit is used up.\n\nA          followed hop rewrites the request the way every HTTP client does: a `303` becomes a          bodyless `GET` (a `HEAD` stays a `HEAD`), a `301`/`302` turns a `POST` into a bodyless          `GET`, and a `307`/`308` preserves both method and body. `Authorization`, `Cookie` and          request signatures are dropped when a hop crosses to a different scheme, host or port —          an open redirect on a trusted host must not hand your token to whoever the parameter          names.",
    ),
    (
        "base_url",
        "The client's base URL, or empty if it has none.",
    ),
    (
        "prepare",
        "Build a `Request` without performing it — path resolved against the base URL, client \
         headers applied. The value a middleware chain starts from; pair it with `send`.",
    ),
    (
        "send",
        "Perform an already-built `Request` through this client's configuration (base URL, \
         headers, deadline, retry). The terminal a composed middleware chain bottoms out in — \
         see the `para/api` package, which owns middleware, mocking, and pagination.",
    ),
    (
        "stream",
        "`send`'s incremental twin: perform the request through this client's configuration and read \
         the response body as a `FrameStream` cut by `framing` — see `client.stream`.\n\n\
         Applies the base URL, the client headers, and the deadline, but **not** the retry policy: \
         retrying re-sends the request and discards the first attempt's response, which is coherent \
         for a buffered body and not for a stream the caller may already be reading.\n\n\
         This is a **separate terminal from `send`**, not a variant of it, and a middleware chain \
         written over `send` does not cover it. That is deliberate — a layer that buffers, caches, \
         replays, or retries a response cannot operate on a single-shot body, so the two terminals \
         are kept distinct rather than letting a stream flow silently through layers that would \
         mishandle it.",
    ),
    (
        "get",
        "GET the path, resolved against the base URL. An absolute target (one with a scheme) is \
         used as-is. Returns `Result<Response, HttpError>` exactly like the free verb.",
    ),
    ("head", "HEAD the path — see `get`."),
    ("delete", "DELETE the path — see `get`."),
    ("post", "POST a body to the path — see `get`."),
    ("put", "PUT a body to the path — see `get`."),
    (
        "query",
        "QUERY the path (a safe, idempotent, body-carrying read) — see `get`.",
    ),
    (
        "request",
        "Any other bodyless verb against the path — see `get`.",
    ),
];

fn client_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::http_client::{HttpClient, basic_auth_value};
    let Some(client) = recv.as_any().downcast_ref::<HttpClient>() else {
        return Err(type_error(method, crate::http_client::CLIENT_TYPE_NAME));
    };
    // The configuration half: each returns a NEW client, never mutating the receiver.
    let configured = |next: HttpClient| Ok(NativeOut::Extern(crate::ExternBox::new(next)));
    match method {
        "header" => {
            want_arity(method, args, 2)?;
            let name = want_str(method, args, 0)?.to_string();
            let value = want_str(method, args, 1)?;
            return configured(client.with_header(&name, value));
        }
        "bearer" => {
            want_arity(method, args, 1)?;
            let token = want_str(method, args, 0)?;
            return configured(client.with_header("authorization", &format!("Bearer {token}")));
        }
        "basic" => {
            want_arity(method, args, 2)?;
            let user = want_str(method, args, 0)?.to_string();
            let password = want_str(method, args, 1)?;
            return configured(
                client.with_header("authorization", &basic_auth_value(&user, password)),
            );
        }
        "timeout" => {
            want_arity(method, args, 1)?;
            let ms = want_int(method, args, 0)?;
            if ms <= 0 {
                return Err(type_error(method, "a positive timeout in milliseconds"));
            }
            return configured(client.with_timeout(ms as u64));
        }
        "retry" => {
            want_arity_range(method, args, 1, 3)?;
            let max = want_int(method, args, 0)?;
            if max < 0 {
                return Err(type_error(method, "a non-negative retry count"));
            }
            let mut policy = crate::http_client::RetryPolicy::new(max as u32);
            if args.len() > 1 {
                let base = want_int(method, args, 1)?;
                if base <= 0 {
                    return Err(type_error(method, "a positive backoff in milliseconds"));
                }
                policy.base_ms = base as u64;
            }
            if args.len() > 2 {
                let Some(NativeValue::List(statuses)) = args.get(2) else {
                    return Err(type_error(method, "a list of status codes"));
                };
                policy.on_status = statuses
                    .iter()
                    .map(|v| match v {
                        NativeValue::Scalar(Scalar::Int(n)) if (100..=599).contains(n) => {
                            Ok(*n as u16)
                        }
                        _ => Err(type_error(
                            method,
                            "a list of HTTP status codes in 100..=599",
                        )),
                    })
                    .collect::<Result<_, _>>()?;
            }
            // Inherit the opt-in when re-configuring an already-unsafe client, so the order of
            // `.retry(..)` and `.retry_non_idempotent()` in a chain does not change behavior.
            policy.non_idempotent = client.retry.as_ref().is_some_and(|r| r.non_idempotent);
            return configured(client.with_retry(policy));
        }
        "retry_non_idempotent" => {
            want_arity(method, args, 0)?;
            return configured(client.with_non_idempotent_retry());
        }
        "redirect" => {
            want_arity(method, args, 1)?;
            let limit = want_int(method, args, 0)?;
            if limit < 0 {
                return Err(type_error(method, "a non-negative redirect limit"));
            }
            return configured(client.with_redirect_limit(limit as u32));
        }
        "base_url" => {
            want_arity(method, args, 0)?;
            return Ok(NativeOut::Str(client.base_url.clone()));
        }
        "prepare" => {
            want_arity_range(method, args, 2, 4)?;
            let verb = want_str(method, args, 0)?.to_string();
            let target = want_str(method, args, 1)?.to_string();
            let body = match args.get(2) {
                None => Vec::new(),
                Some(_) => want_data(method, args, 2)?.to_vec(),
            };
            let inner = client.build(&verb, &target, body, want_headers(method, args, 3)?);
            return Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::net::Request {
                    conn: None, // outbound: there is no connection to reply on
                    inner,
                },
            )));
        }
        "send" => {
            want_arity(method, args, 1)?;
            let Some(NativeValue::Extern(value)) = args.first() else {
                return Err(type_error(method, crate::net::REQUEST_TYPE_NAME));
            };
            let Some(request) = value.as_any().downcast_ref::<crate::net::Request>() else {
                return Err(type_error(method, crate::net::REQUEST_TYPE_NAME));
            };
            // The request arrives fully formed, so only the client's own configuration is layered
            // on: its headers (the request's own win), its base URL for a relative target, its
            // deadline, its retry policy.
            let outgoing = client.build(
                &request.inner.method,
                &request.inner.url,
                request.inner.body.clone(),
                request.inner.headers.clone(),
            );
            return Ok(crate::net::fetch_outcome(client.perform(outgoing, host)));
        }
        // The streaming terminal (http-streaming arc) — `send`'s twin, layering the same client
        // configuration onto a prepared request, minus the retry policy (see the signature's note:
        // re-sending discards a response the caller may already be reading).
        "stream" => {
            want_arity(method, args, 2)?;
            let request = want_request(method, args, 0)?.inner.clone();
            let framing = want_framing(method, args, 1)?;
            let outgoing =
                client.build(&request.method, &request.url, request.body, request.headers);
            return Ok(stream_outcome(host.net_stream_open(outgoing, framing)));
        }
        _ => {}
    }
    // The verb half: expand the client into a plain request, then take the shared fetch door, so a
    // configured call and a free call are indistinguishable from the Host's side.
    let request = match method {
        "get" | "head" | "delete" => {
            want_arity_range(method, args, 1, 2)?;
            client.build(
                method,
                want_str(method, args, 0)?,
                Vec::new(),
                want_headers(method, args, 1)?,
            )
        }
        "post" | "put" | "query" => {
            want_arity_range(method, args, 2, 3)?;
            let target = want_str(method, args, 0)?.to_string();
            let body = want_data(method, args, 1)?.to_vec();
            client.build(method, &target, body, want_headers(method, args, 2)?)
        }
        "request" => {
            want_arity_range(method, args, 2, 3)?;
            let verb = want_str(method, args, 0)?.to_string();
            let target = want_str(method, args, 1)?.to_string();
            client.build(&verb, &target, Vec::new(), want_headers(method, args, 2)?)
        }
        _ => {
            return Err(crate::no_method_error(
                crate::http_client::CLIENT_TYPE_NAME,
                method,
            ));
        }
    };
    // Through the client's own `perform`, which applies the retry policy (http arc H9); without a
    // policy it is exactly `host.net_fetch`, so a non-retrying client costs nothing extra.
    Ok(crate::net::fetch_outcome(client.perform(request, host)))
}

/// The `HttpError` instance methods (http arc H6): pure reads over the transport failure. Mirrors
/// `JsonError` — `message`/`to_string` satisfy the `Error` + `Display` declarations on its
/// registration, so `?` converts it through `From` and `${e}` interpolates the sentence.
const HTTP_ERROR_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "message",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "kind",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "url",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "retryable",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
];

const HTTP_ERROR_DOCS: &[(&str, &str)] = &[
    (
        "message",
        "The composed human message (`timeout request to `https://api.example.com`: …`). The \
         `Error` trait's required method.",
    ),
    (
        "to_string",
        "Same as `message()` — the `Display` rendering, so `${e}` interpolates the message.",
    ),
    (
        "kind",
        "What went wrong: `\"timeout\"`, `\"dns\"`, `\"connect\"`, `\"tls\"`, `\"protocol\"` (the \
         response was unreadable), `\"invalid_url\"`, or `\"other\"`. A request never yields \
         `\"status\"` — an HTTP error status is an ordinary `Response`, checked with `ok()`; that \
         kind appears only when you opt in with `error_for_status()`. `\"interrupted\"` says the \
         run itself is stopping and the request was abandoned, which is the one kind that is not \
         about the network.",
    ),
    ("url", "The request URL that failed."),
    (
        "retryable",
        "Whether retrying the identical request could plausibly succeed. True for `timeout`, \
         `dns`, and `connect` (transient); false for `tls` and `invalid_url` (deterministic), for \
         `protocol`/`other`, where the request may already have been applied server-side, and for \
         `interrupted`, where nobody is left to read the answer.",
    ),
];

fn http_error_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(error) = recv.as_any().downcast_ref::<crate::NetError>() else {
        return Err(type_error(method, "HttpError"));
    };
    want_arity(method, args, 0)?;
    match method {
        "message" | "to_string" => Ok(NativeOut::Str(error.message())),
        "kind" => Ok(NativeOut::Str(error.kind.label().to_string())),
        "url" => Ok(NativeOut::Str(error.url.clone())),
        "retryable" => Ok(NativeOut::Scalar(Scalar::Bool(error.kind.retryable()))),
        _ => Err(crate::no_method_error(
            crate::net::HTTP_ERROR_TYPE_NAME,
            method,
        )),
    }
}

/// The `Response` instance methods (http arc H2): all pure reads over the wrapped response.
const RESPONSE_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "status",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "ok",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &[],
        name: "body",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "body_bytes",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        param_names: &["name"],
        name: "header",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["name", "value"],
        name: "with_header",
        params: &[Str, Str],
        ret: Concrete(RESPONSE_SIG),
    },
    // Reading a repeatable header. Deliberately **asymmetric** with the write side, and the
    // asymmetry tracks who controls the bytes: a peer may repeat any header it likes and you must
    // be able to see all of them, whereas what *you* emit can always be comma-joined — except for
    // `Set-Cookie`, which gets its own door below. So there is a generic multi-value read and no
    // generic multi-value write. `header` answers with the first match only, which is a lossy
    // question to ask of a repeated header. Empty when absent.
    ExtFn {
        param_names: &["name"],
        name: "headers_all",
        params: &[Str],
        ret: Concrete(SigType::List(&Str)),
    },
    // Set a cookie — the whole write side of repeatable headers, because `Set-Cookie` is the only
    // header RFC 7230 §3.2.2 exempts from comma-folding (a cookie's `Expires` attribute contains a
    // comma, so the fold would be ambiguous). Two cookies must therefore be two headers.
    //
    // It replaces per **cookie name** rather than appending blindly, which keeps one rule across
    // the whole type — `with_X` sets X — and leaves the multi-header shape an implementation
    // detail no caller reasons about. A generic append was the rejected alternative: it would have
    // been a second, subtly-different header operation existing to serve exactly one header.
    ExtFn {
        param_names: &["cookie"],
        name: "with_cookie",
        params: &[COOKIE_SIG],
        ret: Concrete(RESPONSE_SIG),
    },
    // The opt-in status-as-error door (http arc H6): `resp.error_for_status()?` turns a non-2xx
    // into the `Err` arm, for callers who want a 404 to short-circuit like a transport failure.
    // Kept explicit rather than folded into the verbs, so `?` on a request keeps one meaning.
    ExtFn {
        param_names: &[],
        name: "error_for_status",
        params: &[],
        ret: Concrete(RESPONSE_RESULT_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "url",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "links",
        params: &[],
        ret: Concrete(SigType::Map(&Str, &Str)),
    },
];

/// `Response`'s **call-site-typed** methods (http arc H8): `resp.json::<User>()`.
///
/// Recoverable by construction — it returns `Result<T, JsonError>`, the `json.try_parse::<T>`
/// wrapper, because a response body is remote input: a server that changes its shape must be a
/// value you can handle, never an abort. The aborting spelling stays available as
/// `json.parse::<T>(resp.body())` for callers who genuinely want a malformed body to be fatal.
const RESPONSE_TYPED_METHODS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "json",
    params: &[],
    ret: TypeArg(TypeArgWrap::Result(SigType::Named("JsonError"))),
}];

fn response_typed_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError> {
    let Some(resp) = recv.as_any().downcast_ref::<crate::NetResponse>() else {
        return Err(type_error(method, "Response"));
    };
    match method {
        "json" => {
            want_arity(method, args, 0)?;
            let body = String::from_utf8_lossy(&resp.body);
            Ok(match crate::json::try_parse_typed(&body, recipe) {
                Ok(out) => NativeOut::Ok(Box::new(out)),
                Err(error) => {
                    NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error))))
                }
            })
        }
        _ => Err(crate::no_method_error(
            crate::net::RESPONSE_TYPE_NAME,
            method,
        )),
    }
}

fn response_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(resp) = recv.as_any().downcast_ref::<crate::NetResponse>() else {
        return Err(type_error(method, "Response"));
    };
    match method {
        "status" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(i64::from(resp.status))))
        }
        "ok" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                (200..=299).contains(&resp.status),
            )))
        }
        "body" => {
            want_arity(method, args, 0)?;
            // Lossy UTF-8 is the friendly scripting default; `body_bytes` gives the raw buffer.
            Ok(NativeOut::Str(
                String::from_utf8_lossy(&resp.body).into_owned(),
            ))
        }
        "body_bytes" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(resp.body.clone()))
        }
        "header" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match resp.header_value(name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value.to_string()))),
                None => NativeOut::None,
            })
        }
        "with_header" => {
            want_arity(method, args, 2)?;
            let name = want_str(method, args, 0)?.to_string();
            let value = want_str(method, args, 1)?.to_string();
            // Copy-modify: a `Response` is immutable, so middleware builds a new one with the header
            // added (replacing any existing same-named header, case-insensitively).
            let mut next = resp.clone();
            next.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            next.headers.push((name, value));
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        "headers_all" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(NativeOut::List(
                resp.headers
                    .iter()
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| NativeOut::Str(v.clone()))
                    .collect(),
            ))
        }
        "with_cookie" => {
            want_arity(method, args, 1)?;
            let cookie = want_cookie(method, args, 0)?;
            let mut next = resp.clone();
            // Replace per **cookie**, not per header — the `with_header` rule ("this is the value
            // of X") applied to the right X. A different cookie name appends and so becomes a
            // second `Set-Cookie` header, which is what the wire format demands; the same name
            // replaces, because setting one cookie twice in a response is a bug in every case.
            // The multi-header shape is therefore an implementation detail no caller reasons about.
            next.headers.retain(|(k, v)| {
                !k.eq_ignore_ascii_case("set-cookie")
                    || !crate::cookie::header_sets_cookie_named(v, &cookie.name)
            });
            next.headers
                .push(("set-cookie".to_string(), cookie.to_header()));
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        "url" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(resp.url.clone()))
        }
        "links" => {
            want_arity(method, args, 0)?;
            // RFC 8288 `Link` relations, `rel -> target`. Empty when the header is absent, so a
            // caller can walk relations without first testing for the header.
            let header = resp.header_value("link").unwrap_or_default();
            Ok(NativeOut::Map(
                crate::http_client::parse_link_header(header)
                    .into_iter()
                    .map(|(rel, target)| (rel, NativeOut::Str(target)))
                    .collect(),
            ))
        }
        "error_for_status" => {
            want_arity(method, args, 0)?;
            // A status is not a transport failure, so it gets its OWN kind rather than borrowing
            // `Protocol` (which means "the response could not be read as HTTP" — a 404 is
            // perfectly valid HTTP). Sharing them would make `kind() == "protocol"` fire for every
            // opted-in 404, defeating the point of classifying at all.
            Ok(if (200..=299).contains(&resp.status) {
                NativeOut::Ok(Box::new(NativeOut::Extern(crate::ExternBox::new(
                    resp.clone(),
                ))))
            } else {
                NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(
                    crate::NetError::new(
                        crate::net::NetErrorKind::Status,
                        resp.url.clone(),
                        format!("the server answered with status {}", resp.status),
                    ),
                ))))
            })
        }
        _ => Err(crate::no_method_error(
            crate::net::RESPONSE_TYPE_NAME,
            method,
        )),
    }
}

/// The `Session` instance methods (session arc S3). Copy-modify like `Response` and `Cookie`, which
/// is what makes `dirty` trustworthy: the flag moves exactly where a builder ran, never because an
/// aliased handle mutated underneath.
const SESSION_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["name"],
        name: "get",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["name", "value"],
        name: "set",
        params: &[Str, Str],
        ret: Concrete(SESSION_SIG),
    },
    ExtFn {
        param_names: &["name"],
        name: "remove",
        params: &[Str],
        ret: Concrete(SESSION_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "clear",
        params: &[],
        ret: Concrete(SESSION_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "dirty",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &[],
        name: "data",
        params: &[],
        ret: Concrete(SESSION_DATA_SIG),
    },
    // Tag a copy with an opaque server-side id (a stored backend's row key). Metadata, not a data
    // change, so it does NOT mark the session dirty; and it rides alongside the data rather than in
    // it, so it never shows through `data()`.
    ExtFn {
        param_names: &["id"],
        name: "with_id",
        params: &[Str],
        ret: Concrete(SESSION_SIG),
    },
    // The opaque server-side id a stored backend tagged this session with, or none. Survives
    // `clear()`, so a logout can still name the row to delete after the data is gone.
    ExtFn {
        param_names: &[],
        name: "id",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
];

fn session_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let session = recv
        .as_any()
        .downcast_ref::<crate::session::Session>()
        .expect("a Session receiver wraps a Session");
    let rebuilt =
        |next: crate::session::Session| Ok(NativeOut::Extern(crate::ExternBox::new(next)));
    match method {
        "get" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match session.data.get(name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value.clone()))),
                None => NativeOut::None,
            })
        }
        "set" => {
            want_arity(method, args, 2)?;
            let name = want_str(method, args, 0)?.to_string();
            let value = want_str(method, args, 1)?;
            rebuilt(session.with(&name, value))
        }
        "remove" => {
            want_arity(method, args, 1)?;
            rebuilt(session.without(want_str(method, args, 0)?))
        }
        "clear" => {
            want_arity(method, args, 0)?;
            rebuilt(session.cleared())
        }
        "dirty" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(session.dirty)))
        }
        "data" => {
            want_arity(method, args, 0)?;
            Ok(session_data_out(&session.data))
        }
        "with_id" => {
            want_arity(method, args, 1)?;
            rebuilt(session.with_id(want_str(method, args, 0)?))
        }
        "id" => {
            want_arity(method, args, 0)?;
            Ok(match &session.id {
                Some(id) => NativeOut::Some(Box::new(NativeOut::Str(id.clone()))),
                None => NativeOut::None,
            })
        }
        _ => Err(crate::no_method_error(
            crate::session::SESSION_TYPE_NAME,
            method,
        )),
    }
}

/// The `Cookie` instance methods (cookie arc C1): accessors plus copy-modify builders, the
/// `Response.with_header` shape. Every builder that can be given an invalid component returns a
/// new `Cookie` only after validating it, so an unserializable cookie is unrepresentable.
const COOKIE_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "name",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "value",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["value"],
        name: "with_value",
        params: &[Str],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["path"],
        name: "with_path",
        params: &[Str],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["domain"],
        name: "with_domain",
        params: &[Str],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["seconds"],
        name: "with_max_age",
        params: &[Int],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["enabled"],
        name: "with_http_only",
        params: &[SigType::Bool],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["enabled"],
        name: "with_secure",
        params: &[SigType::Bool],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &["policy"],
        name: "with_same_site",
        params: &[Str],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "expired",
        params: &[],
        ret: Concrete(COOKIE_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "to_header",
        params: &[],
        ret: Concrete(Str),
    },
];

/// Read a `bool` argument. The checker has already typed it, so a mismatch here is a backend bug
/// rather than user error — but it reports as an arg-type error like every other `want_*`.
fn want_bool(func: &str, args: &[NativeValue], index: usize) -> Result<bool, StdError> {
    match args.get(index) {
        Some(NativeValue::Scalar(Scalar::Bool(b))) => Ok(*b),
        _ => Err(type_error(func, "bool")),
    }
}

/// Read a `Cookie` argument, downcast from its extern box (the `Client.send` pattern).
fn want_cookie<'a>(
    func: &str,
    args: &'a [NativeValue],
    index: usize,
) -> Result<&'a crate::cookie::Cookie, StdError> {
    let Some(NativeValue::Extern(value)) = args.get(index) else {
        return Err(type_error(func, crate::cookie::COOKIE_TYPE_NAME));
    };
    value
        .as_any()
        .downcast_ref::<crate::cookie::Cookie>()
        .ok_or_else(|| type_error(func, crate::cookie::COOKIE_TYPE_NAME))
}

fn cookie_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let cookie = recv
        .as_any()
        .downcast_ref::<crate::cookie::Cookie>()
        .expect("a Cookie receiver wraps a Cookie");
    let rebuilt = |next: crate::cookie::Cookie| Ok(NativeOut::Extern(crate::ExternBox::new(next)));
    match method {
        "name" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(cookie.name.clone()))
        }
        "value" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(cookie.value.clone()))
        }
        "with_value" => {
            want_arity(method, args, 1)?;
            rebuilt(cookie.with_value(want_str(method, args, 0)?)?)
        }
        "with_path" => {
            want_arity(method, args, 1)?;
            rebuilt(cookie.with_path(want_str(method, args, 0)?)?)
        }
        "with_domain" => {
            want_arity(method, args, 1)?;
            rebuilt(cookie.with_domain(want_str(method, args, 0)?)?)
        }
        "with_max_age" => {
            want_arity(method, args, 1)?;
            rebuilt(cookie.with_max_age(want_int(method, args, 0)?))
        }
        "with_http_only" => {
            want_arity(method, args, 1)?;
            rebuilt(crate::cookie::Cookie {
                http_only: want_bool(method, args, 0)?,
                ..cookie.clone()
            })
        }
        "with_secure" => {
            want_arity(method, args, 1)?;
            rebuilt(cookie.with_secure(want_bool(method, args, 0)?)?)
        }
        "with_same_site" => {
            want_arity(method, args, 1)?;
            let same_site = crate::cookie::SameSite::parse(want_str(method, args, 0)?)?;
            rebuilt(cookie.with_same_site(same_site))
        }
        "expired" => {
            want_arity(method, args, 0)?;
            rebuilt(cookie.expired())
        }
        "to_header" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(cookie.to_header()))
        }
        _ => Err(crate::no_method_error(
            crate::cookie::COOKIE_TYPE_NAME,
            method,
        )),
    }
}

/// The `Request` instance methods (http-server S2): pure reads over the wrapped inbound request,
/// plus the `with_*` copy-modify builders a middleware layer rewrites a request through.
const REQUEST_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "method",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "path",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &["name"],
        name: "query",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["name"],
        name: "header",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &[],
        name: "body",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "body_bytes",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
    // The `application/x-www-form-urlencoded` body, decoded — the same wire format `query` parses,
    // read from the body instead of the URL. `form(name)`/`form_all()` mirror `cookie`/`cookies`:
    // the single lookup is the common case, the map is there when you want to iterate.
    ExtFn {
        param_names: &["name"],
        name: "form",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &[],
        name: "form_all",
        params: &[],
        ret: Concrete(SigType::Map(&Str, &Str)),
    },
    ExtFn {
        param_names: &[],
        name: "url",
        params: &[],
        ret: Concrete(Str),
    },
    // Copy-modify (the `Response.with_header` shape). A middleware layer above std — para/api —
    // rewrites a request before passing it on, so `Request` needs builders, not just accessors.
    ExtFn {
        param_names: &["name", "value"],
        name: "with_header",
        params: &[Str, Str],
        ret: Concrete(REQUEST_SIG),
    },
    ExtFn {
        param_names: &["url"],
        name: "with_url",
        params: &[Str],
        ret: Concrete(REQUEST_SIG),
    },
    // The inbound read side of cookies (cookie arc C1). Every cookie the client sent, by name.
    //
    // A `Map` rather than a `List<Cookie>` because a request cookie *is* only a name/value pair —
    // attributes are write-only, never echoed back — so a `Cookie` here would carry six fields the
    // client never sent, each a plausible-looking lie. Empty when the header is absent.
    ExtFn {
        param_names: &[],
        name: "cookies",
        params: &[],
        ret: Concrete(SigType::Map(&Str, &Str)),
    },
    // One cookie by name — `cookies()[name]` without materializing the map, and the spelling that
    // matches `header`/`query` for the overwhelmingly common single lookup.
    ExtFn {
        param_names: &["name"],
        name: "cookie",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
];

fn request_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(request) = recv.as_any().downcast_ref::<crate::net::Request>() else {
        return Err(type_error(method, crate::net::REQUEST_TYPE_NAME));
    };
    let req = &request.inner;
    match method {
        "method" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(req.method.clone()))
        }
        "path" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(
                crate::net::request_path(&req.url).to_string(),
            ))
        }
        "query" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match crate::net::query_value(&req.url, name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value))),
                None => NativeOut::None,
            })
        }
        "header" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match crate::net::request_header(req, name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value.to_string()))),
                None => NativeOut::None,
            })
        }
        "body" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        }
        "body_bytes" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(req.body.clone()))
        }
        "form" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            let body = String::from_utf8_lossy(&req.body);
            Ok(match crate::net::form_value(&body, name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value))),
                None => NativeOut::None,
            })
        }
        "form_all" => {
            want_arity(method, args, 0)?;
            let body = String::from_utf8_lossy(&req.body);
            Ok(NativeOut::Map(
                crate::net::form_pairs(&body)
                    .into_iter()
                    .map(|(name, value)| (name, NativeOut::Str(value)))
                    .collect(),
            ))
        }
        "url" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(req.url.clone()))
        }
        "with_header" => {
            want_arity(method, args, 2)?;
            let name = want_str(method, args, 0)?.to_string();
            let value = want_str(method, args, 1)?.to_string();
            let mut next = request.clone();
            next.inner
                .headers
                .retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            next.inner.headers.push((name, value));
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        "with_url" => {
            want_arity(method, args, 1)?;
            let mut next = request.clone();
            next.inner.url = want_str(method, args, 0)?.to_string();
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        "cookies" => {
            want_arity(method, args, 0)?;
            let header = crate::net::request_header(&request.inner, "cookie").unwrap_or_default();
            Ok(NativeOut::Map(
                crate::cookie::parse_cookie_header(header)
                    .into_iter()
                    .map(|(name, value)| (name, NativeOut::Str(value)))
                    .collect(),
            ))
        }
        "cookie" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            let header = crate::net::request_header(&request.inner, "cookie").unwrap_or_default();
            Ok(
                match crate::cookie::parse_cookie_header(header)
                    .into_iter()
                    .find(|(k, _)| k == name)
                {
                    // Cookie names are case-SENSITIVE (unlike header names) — RFC 6265 — so this
                    // compares exactly where `header` compares case-insensitively.
                    Some((_, value)) => NativeOut::Some(Box::new(NativeOut::Str(value))),
                    None => NativeOut::None,
                },
            )
        }
        _ => Err(crate::no_method_error(
            crate::net::REQUEST_TYPE_NAME,
            method,
        )),
    }
}

// --- `env` / `args`: host introspection ---------------------------------------------------------

fn env_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "get" => {
            want_arity(func, args, 1)?;
            let key = want_str(func, args, 0)?;
            // An unset variable is `none`, not a failure: optional configuration is the common
            // case for CLI/config code, and the host layer already models absence as `Option`.
            Ok(match host.env_get(key) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value))),
                None => NativeOut::None,
            })
        }
        "keys" => {
            want_arity(func, args, 0)?;
            Ok(str_list(host.env_keys()))
        }
        // `.env` support (F5). `parse` is pure; `load` reads a file through the filesystem
        // capability, parses it, then overlays the ambient environment on top (real env wins).
        "parse" => {
            want_arity(func, args, 1)?;
            let text = want_str(func, args, 0)?;
            Ok(str_map(crate::env::parse_dotenv(text)))
        }
        "load" => {
            // `path` is optional (defaults to `.env`).
            if args.len() > 1 {
                return Err(arity_error(func, 1, args.len()));
            }
            let path = match args.first() {
                Some(_) => want_str(func, args, 0)?,
                None => crate::env::DEFAULT_DOTENV_PATH,
            };
            // The ambient environment: both the interpolation base for `${VAR}` (ambient wins) and
            // the overlay applied on top of the file (existing env wins on whole keys).
            let mut ambient = std::collections::BTreeMap::new();
            for key in host.env_keys() {
                if let Some(value) = host.env_get(&key) {
                    ambient.insert(key, value);
                }
            }
            // A missing `.env` is tolerated — the result is just the ambient environment.
            let mut merged = if host.fs_exists(path) {
                crate::env::parse_dotenv_with_env(&host.fs_read(path)?, &ambient)
            } else {
                std::collections::BTreeMap::new()
            };
            // Overlay the ambient environment on top so an existing variable always wins — the
            // cross-ecosystem `.env` precedence. The union is the full merged environment.
            merged.extend(ambient);
            Ok(str_map(merged))
        }
        "set" => {
            want_arity(func, args, 2)?;
            let key = want_str(func, args, 0)?;
            let value = want_str(func, args, 1)?;
            host.env_set(key, value);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_function_error("env", func)),
    }
}

// --- `os`: process execution + system introspection over the Os capability (stdlib-gaps) --------

/// Parse `os.exec`'s optional second argument — a `List<string>` argv (defaults to empty).
fn want_argv(func: &str, args: &[NativeValue], index: usize) -> Result<Vec<String>, StdError> {
    match args.get(index) {
        None => Ok(Vec::new()),
        Some(NativeValue::List(items)) => items
            .iter()
            .map(|item| match item {
                NativeValue::Str(s) => Ok(s.clone()),
                _ => Err(type_error(func, "list of strings")),
            })
            .collect(),
        Some(_) => Err(type_error(func, "list of strings")),
    }
}

fn os_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "platform" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_platform()))
        }
        "arch" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_arch()))
        }
        "hostname" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_hostname()))
        }
        "cpus" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.os_cpus())))
        }
        "cwd" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_cwd()))
        }
        "pid" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.os_pid())))
        }
        // `exec(command, args?)` — run a subprocess (no shell), wait, capture the outcome.
        "exec" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?;
            let argv = want_argv(func, args, 1)?;
            let result = host.os_exec(command, &argv)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(result)))
        }
        // The async twin: returns WORK the backend tickets on its executor, like `fs.read_async`.
        "exec_async" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?.to_string();
            let argv = want_argv(func, args, 1)?;
            Ok(NativeOut::Spawn(SpawnBox(
                host.os_exec_spawn(command, argv),
            )))
        }
        // `spawn(command, args?)` — start a child WITHOUT waiting and hand back a controllable
        // `Process` handle (process-handle arc), unlike `exec`'s run-to-completion. The **aborting**
        // door of the pair; `try_spawn` below is its recoverable twin.
        "spawn" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?;
            let argv = want_argv(func, args, 1)?;
            let id = host.os_spawn(command, &argv)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::os::Process { id },
            )))
        }
        // `try_spawn(command, args?)` — the **recoverable** door (subprocess-doors arc), the shape
        // `json.parse`/`json.try_parse` sets. A tool server that is not installed is an ordinary
        // condition for a client, not a reason to take the program down, so this hands back
        // `Result<Process, OsError>` and never a `StdError` abort.
        "try_spawn" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?;
            let argv = want_argv(func, args, 1)?;
            Ok(match host.os_try_spawn(command, &argv) {
                Ok(id) => NativeOut::Ok(Box::new(NativeOut::Extern(crate::ExternBox::new(
                    crate::os::Process { id },
                )))),
                Err(error) => {
                    NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error))))
                }
            })
        }
        // `exit(code?)` — deliberate termination. Not a host effect and not a diagnostic: the
        // distinguished `ErrorKind::Exit` unwinds the backend, which halts cleanly and surfaces
        // the code as the run's exit code.
        "exit" => {
            want_arity_range(func, args, 0, 1)?;
            let code = match args.first() {
                Some(_) => want_int(func, args, 0)?,
                None => 0,
            };
            Err(StdError {
                kind: ErrorKind::Exit(code as i32),
                message: format!("exit({code})"),
            })
        }
        // Quote a string so it is a single, literal token to a POSIX shell — for the explicit
        // `os.exec("sh", ["-c", ...])` escape hatch (the argv-vector `exec`/`spawn` API never
        // touches a shell and needs no quoting). Pure and deterministic.
        "shell_quote" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(shell_quote(want_str(func, args, 0)?)))
        }
        _ => Err(no_function_error("os", func)),
    }
}

/// POSIX-shell single-quote a token so it is passed to the shell literally (no word-splitting,
/// glob, or expansion). An empty string becomes `''`; a string of only safe characters is returned
/// unquoted; otherwise it is wrapped in single quotes with any embedded `'` written as `'\''`
/// (close-quote, escaped quote, reopen) — the canonical, injection-safe shell quoting.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if safe {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The `ExecResult` instance methods (stdlib-gaps): pure reads over the captured outcome, the
/// `Response` accessor model.
const EXEC_RESULT_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "status",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "ok",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &[],
        name: "stdout",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "stderr",
        params: &[],
        ret: Concrete(Str),
    },
];

fn exec_result_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(result) = recv.as_any().downcast_ref::<crate::ExecResult>() else {
        return Err(type_error(method, "ExecResult"));
    };
    want_arity(method, args, 0)?;
    match method {
        "status" => Ok(NativeOut::Scalar(Scalar::Int(result.status))),
        "ok" => Ok(NativeOut::Scalar(Scalar::Bool(result.status == 0))),
        "stdout" => Ok(NativeOut::Str(result.stdout.clone())),
        "stderr" => Ok(NativeOut::Str(result.stderr.clone())),
        _ => Err(crate::no_method_error(
            crate::os::EXEC_RESULT_TYPE_NAME,
            method,
        )),
    }
}

/// The `Process` instance methods (process-handle arc): lifecycle control over a spawned child,
/// each routing to the `Os` seam by the handle's id.
const PROCESS_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "pid",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "wait",
        params: &[],
        ret: Concrete(EXEC_RESULT_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "try_wait",
        params: &[],
        ret: Concrete(SigType::Option(&EXEC_RESULT_SIG)),
    },
    ExtFn {
        param_names: &[],
        name: "kill",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
    // Signalling (process-signals arc): the general form of `kill` — send a named OS signal.
    ExtFn {
        param_names: &["name"],
        name: "signal",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    // `wait_async` (process-signals arc): the awaitable twin of `wait` — yields a
    // `Future<ExecResult>`. Deterministic in the sandbox; genuinely overlapping on the real host.
    ExtFn {
        param_names: &[],
        name: "wait_async",
        params: &[],
        ret: Concrete(SigType::Future(&EXEC_RESULT_SIG)),
    },
    // Streaming (process-streaming arc): consume stdout line-by-line or by character count while
    // the child runs, read stderr, and feed / close its stdin. `wait` still returns the whole
    // captured output.
    ExtFn {
        param_names: &[],
        name: "read_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &["count"],
        name: "read",
        params: &[Int],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &[],
        name: "read_err_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    // The awaitable twins of the three reads (subprocess-async arc): each yields a `Future<?string>`
    // an `.await` unwraps, so `race([p.read_line_async(), task.tick(500)])` is how a program bounds
    // a child read. All three, not a chosen subset — a blocking read without a twin parks the
    // isolate's whole scheduler, and the stderr side parks it exactly as the stdout side does.
    ExtFn {
        param_names: &[],
        name: "read_line_async",
        params: &[],
        ret: Concrete(SigType::Future(&OPT_STR_SIG)),
    },
    ExtFn {
        param_names: &[],
        name: "read_err_line_async",
        params: &[],
        ret: Concrete(SigType::Future(&OPT_STR_SIG)),
    },
    ExtFn {
        param_names: &["count"],
        name: "read_async",
        params: &[Int],
        ret: Concrete(SigType::Future(&OPT_STR_SIG)),
    },
    ExtFn {
        param_names: &["contents"],
        name: "write",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    // The recoverable write door (subprocess-doors arc) — `write`'s `json.try_parse` twin.
    ExtFn {
        param_names: &["contents"],
        name: "try_write",
        params: &[Str],
        ret: Concrete(SigType::Result(&SigType::Unit, &OS_ERROR_SIG)),
    },
    ExtFn {
        param_names: &[],
        name: "close_stdin",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
];

/// `?string` — the return of every streaming read, and (wrapped in a `Future`) of its async twin.
const OPT_STR_SIG: SigType = SigType::Option(&Str);

/// The `OsError` signature — the payload of both recoverable subprocess doors.
const OS_ERROR_SIG: SigType = SigType::Named(crate::os::OS_ERROR_TYPE_NAME);

/// The `OsError` instance methods (subprocess-doors arc): pure reads over the recoverable
/// subprocess failure. The `HttpError`/`JsonError` shape — `message`/`to_string` satisfy the
/// `Error` + `Display` declarations on its registration, so `?` converts it through `From` and
/// `${e}` interpolates the sentence.
const OS_ERROR_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "message",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "kind",
        params: &[],
        ret: Concrete(Str),
    },
];

const OS_ERROR_DOCS: &[(&str, &str)] = &[
    (
        "message",
        "The composed human message (``spawn: cannot start `mcp-server`: No such file or \
         directory``) — identical to what the aborting twin reports. The `Error` trait's required \
         method.",
    ),
    (
        "to_string",
        "Same as `message()` — the `Display` rendering, so `${e}` interpolates the message.",
    ),
    (
        "kind",
        "What went wrong: `\"not_found\"` (the command is not on `PATH`), \
         `\"permission_denied\"`, `\"broken_pipe\"` (the child is gone and took its stdin with \
         it), `\"stdin_closed\"` (you closed it with `close_stdin`), or `\"other\"`. Branch on \
         this rather than on the message — `not_found` usually means \"tell the user to install \
         it\" while `broken_pipe` usually means \"restart the server and retry\".",
    ),
];

fn os_error_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(error) = recv.as_any().downcast_ref::<crate::os::OsError>() else {
        return Err(type_error(method, crate::os::OS_ERROR_TYPE_NAME));
    };
    want_arity(method, args, 0)?;
    match method {
        "message" | "to_string" => Ok(NativeOut::Str(error.message())),
        "kind" => Ok(NativeOut::Str(error.kind.label().to_string())),
        _ => Err(crate::no_method_error(
            crate::os::OS_ERROR_TYPE_NAME,
            method,
        )),
    }
}

/// Wrap an optional string read (a streaming `read_line`/`read`/`read_err_line`) into a native
/// `some(...)`/`none`.
fn opt_str_out(line: Option<String>) -> NativeOut {
    match line {
        Some(s) => NativeOut::Some(Box::new(NativeOut::Str(s))),
        None => NativeOut::None,
    }
}

fn process_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(process) = recv.as_any().downcast_ref::<crate::os::Process>() else {
        return Err(type_error(method, "Process"));
    };
    let id = process.id;
    let exec_out = |r: crate::ExecResult| NativeOut::Extern(crate::ExternBox::new(r));
    match method {
        "pid" => {
            want_arity(method, args, 0)?;
            match host.os_proc_pid(id) {
                Some(pid) => Ok(NativeOut::Scalar(Scalar::Int(pid))),
                None => Err(crate::os::unknown_process_error(id)),
            }
        }
        "wait" => {
            want_arity(method, args, 0)?;
            Ok(exec_out(host.os_proc_wait(id)?))
        }
        "try_wait" => {
            want_arity(method, args, 0)?;
            Ok(match host.os_proc_try_wait(id)? {
                Some(result) => NativeOut::Some(Box::new(exec_out(result))),
                None => NativeOut::None,
            })
        }
        "kill" => {
            want_arity(method, args, 0)?;
            host.os_proc_kill(id)?;
            Ok(NativeOut::Unit)
        }
        "signal" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            let signal = crate::os::Signal::parse(name)
                .ok_or_else(|| crate::os::unknown_signal_error(name))?;
            host.os_proc_signal(id, signal)?;
            Ok(NativeOut::Unit)
        }
        "wait_async" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(host.os_proc_wait_spawn(id))))
        }
        "read_line" => {
            want_arity(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read_line(id)?))
        }
        "read" => {
            want_arity(method, args, 1)?;
            let count = want_int(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read(id, count)?))
        }
        "read_err_line" => {
            want_arity(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read_stderr_line(id)?))
        }
        // The awaitable twins of the three reads above (subprocess-async arc). Every blocking read
        // has one, because whichever lacked it could still park the isolate's whole scheduler —
        // which is the condition that makes a bounded child read inexpressible.
        "read_line_async" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(
                host.os_proc_read_spawn(id, crate::os::ProcRead::StdoutLine),
            )))
        }
        "read_err_line_async" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(
                host.os_proc_read_spawn(id, crate::os::ProcRead::StderrLine),
            )))
        }
        "read_async" => {
            want_arity(method, args, 1)?;
            let count = want_int(method, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(host.os_proc_read_spawn(
                id,
                crate::os::ProcRead::Stdout(count),
            ))))
        }
        "write" => {
            want_arity(method, args, 1)?;
            let data = want_str(method, args, 0)?;
            host.os_proc_write_stdin(id, data)?;
            Ok(NativeOut::Unit)
        }
        // The **recoverable** write door (subprocess-doors arc). A child that exited between the
        // program's last liveness check and this write is an ordinary condition — and the race
        // cannot be closed from the language, because the child can die in the gap — so the failure
        // is a value the caller decides about.
        "try_write" => {
            want_arity(method, args, 1)?;
            let data = want_str(method, args, 0)?;
            Ok(match host.os_proc_try_write_stdin(id, data) {
                Ok(()) => NativeOut::Ok(Box::new(NativeOut::Unit)),
                Err(error) => {
                    NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error))))
                }
            })
        }
        "close_stdin" => {
            want_arity(method, args, 0)?;
            host.os_proc_close_stdin(id)?;
            Ok(NativeOut::Unit)
        }
        _ => Err(crate::no_method_error(crate::os::PROCESS_TYPE_NAME, method)),
    }
}

fn args_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "all" => {
            want_arity(func, args, 0)?;
            Ok(str_list(host.args()))
        }
        _ => Err(no_function_error("args", func)),
    }
}

// --- `fs`: file IO over the host's filesystem (sandbox VFS or real disk) ------------------------

fn fs_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "write" => {
            want_arity(func, args, 2)?;
            host.fs_write(want_str(func, args, 0)?, want_str(func, args, 1)?)?;
            Ok(NativeOut::Unit)
        }
        "append" => {
            want_arity(func, args, 2)?;
            host.fs_append(want_str(func, args, 0)?, want_str(func, args, 1)?)?;
            Ok(NativeOut::Unit)
        }
        "write_bytes" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?;
            let NativeValue::Bytes(data) = &args[1] else {
                return Err(StdError {
                    kind: crate::ErrorKind::ArgType,
                    message: format!(
                        "`fs.write_bytes` expects a `bytes` value, found {}",
                        native_type_name(&args[1])
                    ),
                });
            };
            host.fs_write_bytes(path, data)?;
            Ok(NativeOut::Unit)
        }
        "read_bytes" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(
                host.fs_read_bytes(want_str(func, args, 0)?)?,
            ))
        }
        "read" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(host.fs_read(want_str(func, args, 0)?)?))
        }
        "read_lines" => {
            want_arity(func, args, 1)?;
            let content = host.fs_read(want_str(func, args, 0)?)?;
            Ok(str_list(content.lines().map(str::to_string)))
        }
        "exists" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_exists(want_str(func, args, 0)?),
            )))
        }
        "remove" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_remove(want_str(func, args, 0)?)?,
            )))
        }
        "is_dir" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_is_dir(want_str(func, args, 0)?),
            )))
        }
        "mkdir" => {
            want_arity(func, args, 1)?;
            host.fs_mkdir(want_str(func, args, 0)?)?;
            Ok(NativeOut::Unit)
        }
        // `list()` lists every file; `list(dir)` lists a directory's immediate children — the one
        // optionally-arity'd function, so its arity is enforced here rather than by a fixed signature.
        "list" => {
            let paths = match args.len() {
                0 => host.fs_list()?,
                1 => host.fs_list_dir(want_str(func, args, 0)?)?,
                n => return Err(arity_error(func, 1, n)),
            };
            Ok(str_list(paths))
        }
        // `open(path, mode)` → a cursor file handle. Read mode snapshots the file (a missing file
        // is the same IO error as `fs.read`); write/append buffer until `close`.
        "open" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?;
            let mode_spec = want_str(func, args, 1)?;
            let Some(mode) = crate::FileMode::parse(mode_spec) else {
                return Err(crate::handle::unknown_mode_error(mode_spec));
            };
            let handle = match mode {
                // The host decides eager-vs-lazy delivery (sandbox snapshots; real host streams).
                crate::FileMode::Read => {
                    crate::FileHandle::open_read(path, host.fs_open_read(path)?)
                }
                crate::FileMode::Write => crate::FileHandle::open_write(path),
                crate::FileMode::Append => crate::FileHandle::open_append(path),
            };
            Ok(NativeOut::Extern(crate::ExternBox::new(handle)))
        }
        // The async fs surface (Track A.4c/A.10, on the open seam since extern-types X5): each
        // returns WORK (`NativeOut::Spawn`), which the backend tickets on its executor — the
        // per-backend by-name intercepts are gone.
        "read_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(Box::new(crate::FsIo::Read(
                path.to_string(),
            )))))
        }
        "read_bytes_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(Box::new(
                crate::FsIo::ReadBytes(path.to_string()),
            ))))
        }
        "write_async" | "append_async" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?.to_string();
            let content = want_str(func, args, 1)?.to_string();
            let io = if func == "write_async" {
                crate::FsIo::Write(path, content)
            } else {
                crate::FsIo::Append(path, content)
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(io))))
        }
        // The async metadata twins (extern-types X6; the directory pair is A.10 residue).
        "exists_async" | "remove_async" | "is_dir_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?.to_string();
            let io = match func {
                "exists_async" => crate::FsIo::Exists(path),
                "is_dir_async" => crate::FsIo::IsDir(path),
                _ => crate::FsIo::Remove(path),
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(io))))
        }
        "mkdir_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?.to_string();
            Ok(NativeOut::Spawn(SpawnBox(Box::new(crate::FsIo::Mkdir(
                path,
            )))))
        }
        "list_async" => {
            // 0-or-1 args, mirroring the sync `list` (whole sandbox vs one directory).
            let dir = match args.len() {
                0 => None,
                1 => Some(want_str(func, args, 0)?.to_string()),
                n => return Err(arity_error(func, 1, n)),
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(crate::FsIo::List(dir)))))
        }
        _ => Err(no_function_error("fs", func)),
    }
}

// --- `vec` / `quat`: scalar 3D-math over structural f32 objects ---------------------------------
//
// These exercise the *object* seam: read an argument's `f32` fields, compute (math in
// `noeta_stdlib::vec3`/`quat`), and return the result's field scalars — the backend supplies the
// result shape from the function's `RetTy::SameAsArg`. Only the **scalar** ops migrate here; the
// bulk `*_all` kernels operate on the packed `List<Vec3>` buffer and stay per-backend (they are a
// packed-layout specialization, not a value-seam concern), so they are not registered and the
// router falls through to the backend's `call_vec` for them.

/// Read a Vec3 argument — an object of exactly three `f32` fields — into `[f32; 3]`. The message
/// keeps the `vec.` prefix even for `quat.rotate_vec3`'s vector argument, matching the prior glue.
fn read_vec3(func: &str, args: &[NativeValue], i: usize) -> Result<[f32; 3], StdError> {
    if let Some(NativeValue::Object { fields, .. }) = args.get(i)
        && let [Scalar::F32(x), Scalar::F32(y), Scalar::F32(z)] = fields[..]
    {
        return Ok([x, y, z]);
    }
    Err(shape_error(
        "vec",
        func,
        "a Vec3 (a struct of three f32 fields)",
        args.get(i),
    ))
}

/// Read a Quat argument — an object of exactly four `f32` fields — into `[f32; 4]`.
fn read_quat(func: &str, args: &[NativeValue], i: usize) -> Result<[f32; 4], StdError> {
    if let Some(NativeValue::Object { fields, .. }) = args.get(i)
        && let [
            Scalar::F32(x),
            Scalar::F32(y),
            Scalar::F32(z),
            Scalar::F32(w),
        ] = fields[..]
    {
        return Ok([x, y, z, w]);
    }
    Err(shape_error(
        "quat",
        func,
        "a Quat (a struct of four f32 fields)",
        args.get(i),
    ))
}

/// Read a numeric scalar (`f32`/`float`/`int`) as an `f32` — e.g. the `vec.scale` factor.
fn read_factor(func: &str, args: &[NativeValue], i: usize) -> Result<f32, StdError> {
    match args.get(i) {
        Some(NativeValue::Scalar(Scalar::F32(f))) => Ok(*f),
        Some(NativeValue::Scalar(Scalar::Float(f))) => Ok(*f as f32),
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(*n as f32),
        other => Err(StdError {
            kind: crate::ErrorKind::ArgType,
            message: format!(
                "`vec.{func}` expects a number factor, found {}",
                other.map(native_type_name).unwrap_or("nothing")
            ),
        }),
    }
}

fn shape_error(module: &str, func: &str, expected: &str, value: Option<&NativeValue>) -> StdError {
    StdError {
        kind: crate::ErrorKind::ArgType,
        message: format!(
            "`{module}.{func}` expects {expected}, found {}",
            value.map(native_type_name).unwrap_or("nothing")
        ),
    }
}

fn vec3_out(c: [f32; 3]) -> NativeOut {
    NativeOut::Object(vec![
        Scalar::F32(c[0]),
        Scalar::F32(c[1]),
        Scalar::F32(c[2]),
    ])
}

fn quat_out(c: [f32; 4]) -> NativeOut {
    NativeOut::Object(vec![
        Scalar::F32(c[0]),
        Scalar::F32(c[1]),
        Scalar::F32(c[2]),
        Scalar::F32(c[3]),
    ])
}

fn vec_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::vec3;
    match func {
        "add" | "sub" | "cross" | "reflect" | "min" | "max" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(vec3_out(match func {
                "add" => vec3::add(a, b),
                "sub" => vec3::sub(a, b),
                "cross" => vec3::cross(a, b),
                "reflect" => vec3::reflect(a, b),
                "min" => vec3::min(a, b),
                _ => vec3::max(a, b),
            }))
        }
        "abs" => {
            want_arity(func, args, 1)?;
            Ok(vec3_out(vec3::abs(read_vec3(func, args, 0)?)))
        }
        "normalize" => {
            want_arity(func, args, 1)?;
            Ok(vec3_out(vec3::normalize(read_vec3(func, args, 0)?)))
        }
        "scale" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            Ok(vec3_out(vec3::scale(a, read_factor(func, args, 1)?)))
        }
        "lerp" => {
            want_arity(func, args, 3)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(vec3_out(vec3::lerp(a, b, read_factor(func, args, 2)?)))
        }
        "clamp" => {
            want_arity(func, args, 3)?;
            let v = read_vec3(func, args, 0)?;
            let lo = read_vec3(func, args, 1)?;
            let hi = read_vec3(func, args, 2)?;
            Ok(vec3_out(vec3::clamp(v, lo, hi)))
        }
        "dot" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::dot(a, b))))
        }
        "distance" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::distance(a, b))))
        }
        "length" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::length(read_vec3(
                func, args, 0,
            )?))))
        }
        _ => Err(no_function_error("vec", func)),
    }
}

fn quat_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::quat;
    match func {
        "mul" => {
            want_arity(func, args, 2)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(quat_out(quat::mul(a, b)))
        }
        "conjugate" => {
            want_arity(func, args, 1)?;
            Ok(quat_out(quat::conjugate(read_quat(func, args, 0)?)))
        }
        "normalize" => {
            want_arity(func, args, 1)?;
            Ok(quat_out(quat::normalize(read_quat(func, args, 0)?)))
        }
        "slerp" => {
            want_arity(func, args, 3)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(quat_out(quat::slerp(a, b, read_factor(func, args, 2)?)))
        }
        "dot" => {
            want_arity(func, args, 2)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(quat::dot(a, b))))
        }
        "length" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(quat::length(read_quat(
                func, args, 0,
            )?))))
        }
        "rotate_vec3" => {
            want_arity(func, args, 2)?;
            let q = read_quat(func, args, 0)?;
            let v = read_vec3(func, args, 1)?;
            Ok(vec3_out(quat::rotate_vec3(q, v)))
        }
        _ => Err(no_function_error("quat", func)),
    }
}

// --- the std extension's module table -----------------------------------------------------------

use RetTy::{Concrete, NumericPreserving, SameAsArg, TypeArg};
use SigType::{Dyn, Float, Int, String as Str};

const MATH_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "pi",
        params: &[],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &[],
        name: "e",
        params: &[],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "sqrt",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["base", "exp"],
        name: "pow",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "sin",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "cos",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "tan",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "floor",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &["x"],
        name: "ceil",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &["x"],
        name: "round",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &["x"],
        name: "abs",
        params: &[Dyn],
        ret: NumericPreserving,
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "min",
        params: &[Dyn, Dyn],
        ret: NumericPreserving,
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "max",
        params: &[Dyn, Dyn],
        ret: NumericPreserving,
    },
    // The transcendental family — real-valued like `sqrt`, so params pin to `Float` and the
    // return is always a float.
    ExtFn {
        param_names: &["x"],
        name: "asin",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "acos",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "atan",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["y", "x"],
        name: "atan2",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "ln",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x", "base"],
        name: "log",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "log2",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "log10",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "exp",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x", "y"],
        name: "hypot",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "sinh",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "cosh",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        param_names: &["x"],
        name: "tanh",
        params: &[Float],
        ret: Concrete(Float),
    },
];

/// Documentation prose for `std.math` (docs-browser Arc 2 pilot). Sparse — a function absent here
/// renders signature-only. Keyed by name; see [`ExtModule::docs`].
const MATH_DOCS: &[(&str, &str)] = &[
    ("pi", "The mathematical constant π ≈ 3.14159, as a `float`."),
    ("e", "Euler's number *e* ≈ 2.71828, as a `float`."),
    (
        "sqrt",
        "The non-negative square root of `x`. `x` must be ≥ 0.",
    ),
    ("pow", "`base` raised to the power `exp` (both `float`)."),
    ("sin", "The sine of `x`, with `x` in **radians**."),
    ("cos", "The cosine of `x`, with `x` in **radians**."),
    ("tan", "The tangent of `x`, with `x` in **radians**."),
    (
        "floor",
        "The largest `int` not greater than `x` (rounds toward −∞).",
    ),
    (
        "ceil",
        "The smallest `int` not less than `x` (rounds toward +∞).",
    ),
    (
        "round",
        "`x` rounded to the nearest `int` (ties away from zero).",
    ),
    (
        "abs",
        "The absolute value of `x`, preserving its numeric type (`int`→`int`, `float`→`float`).",
    ),
    (
        "min",
        "The smaller of `a` and `b`, preserving the numeric type.",
    ),
    (
        "max",
        "The larger of `a` and `b`, preserving the numeric type.",
    ),
    (
        "ln",
        "The natural logarithm (base *e*) of `x`. `x` must be > 0.",
    ),
    ("log", "The logarithm of `x` to the given `base`."),
    ("log2", "The base-2 logarithm of `x`."),
    ("log10", "The base-10 logarithm of `x`."),
    ("exp", "*e* raised to the power `x` — the inverse of `ln`."),
    (
        "hypot",
        "The Euclidean distance `sqrt(x*x + y*y)`, computed without intermediate overflow.",
    ),
    (
        "atan2",
        "The angle in radians between the positive x-axis and the point `(x, y)`, in `[-π, π]`.",
    ),
    (
        "asin",
        "The arcsine of `x` (which must be in `[-1, 1]`), in radians.",
    ),
    (
        "acos",
        "The arccosine of `x` (which must be in `[-1, 1]`), in radians.",
    ),
    (
        "atan",
        "The arctangent of `x`, in radians — see `atan2` for the two-argument form.",
    ),
    ("sinh", "The hyperbolic sine of `x`."),
    ("cosh", "The hyperbolic cosine of `x`."),
    ("tanh", "The hyperbolic tangent of `x`."),
];

/// Prose for the remaining `std.*` modules (docs-browser Arc 2 A3 backfill). Each table is keyed by
/// function name and wired into its module below via `docs: <MODULE>_DOCS`; a function absent from
/// its table renders signature-only. Kept next to the module tables so prose and signatures evolve
/// together.
const ARGS_DOCS: &[(&str, &str)] = &[(
    "all",
    "The program's argument vector: element 0 is the program/script path (the `argv[0]` \
     convention), followed by the arguments passed after it.",
)];

const IO_DOCS: &[(&str, &str)] = &[
    (
        "out",
        "Write a value's display form to standard output, with no trailing newline. The stdout \
         buffer is the same one the `echo` keyword writes to.",
    ),
    (
        "outln",
        "Write a value's display form to standard output, followed by a newline — the \
         programmatic twin of `echo`.",
    ),
    (
        "err",
        "Write a value's display form to standard error, with no trailing newline.",
    ),
    (
        "errln",
        "Write a value's display form to standard error, followed by a newline.",
    ),
    (
        "stdin_line",
        "The next line of standard input (without its trailing newline), or `none` at end of input \
         — pair it with `while let` to consume piped stdin a line at a time.",
    ),
    (
        "stdin_all",
        "All remaining standard input, read to end-of-input as one string.",
    ),
    (
        "is_tty",
        "Whether standard output is connected to an interactive terminal — the \"should I colorize?\" \
         check. Always `false` in the deterministic sandbox.",
    ),
    (
        "stdin_is_tty",
        "Whether standard input is a terminal rather than a pipe or file. Always `false` in the \
         sandbox.",
    ),
    (
        "prompt",
        "Write `msg` to the terminal immediately (bypassing the batch output buffer) and read one \
         line of response — the single interactive path that survives batch-captured output. \
         `none` at end of input.",
    ),
];

const CELL_DOCS: &[(&str, &str)] = &[(
    "new",
    "Create a mutable `Cell<T>` holding `value` — a single-slot interior-mutable container. Read \
     with `.get()`, replace with `.set(v)`.",
)];

const RANDOM_DOCS: &[(&str, &str)] = &[
    (
        "float",
        "A random `float` uniformly distributed in `[0, 1)`.",
    ),
    (
        "int",
        "A random `int` uniformly in `[low, high)` — `low` inclusive, `high` exclusive.",
    ),
    (
        "seed",
        "Seed the generator so subsequent draws are reproducible; the same seed yields the same \
         sequence.",
    ),
];

const TIME_DOCS: &[(&str, &str)] = &[
    (
        "monotonic",
        "A monotonic clock reading in nanoseconds — meaningful only for measuring elapsed time, \
         never as wall-clock.",
    ),
    (
        "sleep",
        "Block the current thread for `ms` milliseconds (synchronous — prefer `task.sleep` in async \
         code).",
    ),
];

const ENV_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "The value of environment variable `name`, or `none` if it is unset — pair it with `??` \
         to supply a default.",
    ),
    (
        "keys",
        "The names of every environment variable currently set.",
    ),
    (
        "load",
        "Load a `.env` file (the given path, or `.env` by default) into a key→value map, also \
         setting the variables in the process environment.",
    ),
    (
        "parse",
        "Parse `.env`-format text into a key→value map without touching the process environment.",
    ),
    (
        "set",
        "Set environment variable `name` to `value` for this process.",
    ),
];

const OS_DOCS: &[(&str, &str)] = &[
    (
        "arch",
        "The CPU architecture the program runs on (`\"x86_64\"`, `\"aarch64\"`, …).",
    ),
    ("cpus", "The number of logical CPUs available."),
    ("cwd", "The process's current working directory."),
    (
        "exec",
        "Run `program` with `args` to completion, returning its exit status, stdout, and stderr as \
         an `ExecResult`.",
    ),
    (
        "exec_async",
        "Async `exec` — runs off the executor, yielding a `Future<ExecResult>`.",
    ),
    (
        "exit",
        "Exit the process immediately with the given status code (default 0).",
    ),
    ("hostname", "The machine's hostname."),
    ("pid", "The process id of the current process."),
    (
        "platform",
        "The operating system the program runs on (`\"linux\"`, `\"macos\"`, `\"windows\"`).",
    ),
    (
        "shell_quote",
        "Quote `s` so it is safe to embed as a single argument in a POSIX shell command.",
    ),
    (
        "spawn",
        "Start `program` with `args` as a child `Process` and return immediately — for streaming \
         its I/O or awaiting it later. **Aborts** (E0021) if the child cannot be started at all; \
         use `try_spawn` when a missing program is a case to handle rather than a bug.",
    ),
    (
        "try_spawn",
        "Recoverable `spawn`: `Result<Process, OsError>` instead of an abort. This is the door for \
         starting a program you do not control — a language server, an MCP server, a formatter — \
         where \"not installed\" is an ordinary answer to give the user. Branch on \
         `e.kind()` (`\"not_found\"`, `\"permission_denied\"`).",
    ),
];

const FS_DOCS: &[(&str, &str)] = &[
    ("read", "Read the whole file at `path` as a UTF-8 string."),
    ("read_async", "Async `read` — yields a `Future<string>`."),
    (
        "read_bytes",
        "Read the whole file at `path` as raw `bytes`.",
    ),
    (
        "read_bytes_async",
        "Async `read_bytes` — yields a `Future<bytes>`.",
    ),
    (
        "read_lines",
        "Read the file at `path` and split it into a list of lines (newlines removed).",
    ),
    (
        "write",
        "Write `contents` to `path`, replacing any existing file.",
    ),
    ("write_async", "Async `write` — yields a `Future<void>`."),
    (
        "write_bytes",
        "Write raw `bytes` to `path`, replacing any existing file.",
    ),
    (
        "append",
        "Append `contents` to the file at `path`, creating it if absent.",
    ),
    ("append_async", "Async `append`."),
    ("exists", "Whether a file or directory exists at `path`."),
    ("exists_async", "Async `exists`."),
    ("is_dir", "Whether `path` exists and is a directory."),
    ("is_dir_async", "Async `is_dir` — yields a `Future<bool>`."),
    (
        "list",
        "The entry names of a directory (the given path, or the current directory).",
    ),
    ("list_async", "Async `list`."),
    (
        "mkdir",
        "Create the directory at `path`, including any missing parent directories.",
    ),
    ("mkdir_async", "Async `mkdir` — yields a `Future<void>`."),
    (
        "open",
        "Open the file at `path` in mode `\"r\"`/`\"w\"`/`\"a\"`, returning a `FileHandle` cursor for \
         streaming reads/writes.",
    ),
    (
        "remove",
        "Delete the file at `path`; returns `true` if it existed.",
    ),
    ("remove_async", "Async `remove`."),
];

const JSON_DOCS: &[(&str, &str)] = &[
    (
        "parse",
        "Parse a JSON string, **aborting** on a malformed document (E0007).\n\n\
         Two doors share the name. `json.parse(text)` decodes into a dynamic value — a `dyn` \
         map/list/scalar tree, addressed with `v[\"key\"]`. `json.parse::<T>(text)` decodes into \
         the type you name at the call site, filling declared field defaults and reporting a shape \
         mismatch by path. Reach for either when a malformed document means the program is wrong; \
         use `try_parse` when it means the *input* is wrong — including for the enum rules, which \
         are the same for both doors and written up under `try_parse`.",
    ),
    (
        "try_parse",
        "Parse a JSON string **recoverably**: `Ok(value)`, or `Err(JsonError)` naming the exact \
         failure — its `path()`, `kind()`, and, for a malformed document, `line()`/`column()`. \
         Never aborts.\n\n\
         Two doors share the name. `json.try_parse(text): Result<dyn, JsonError>` needs no target \
         type — the door for a body read off a wire, where the shape is the remote party's. \
         `json.try_parse::<T>(text): Result<T, JsonError>` additionally checks the document \
         against `T` and hands back a real `T`.\n\n\
         Either door composes with `?` and with `match … { Ok(v) => …, Err(e) => … }`; `JsonError` \
         implements `Error` and `Display`, so `${e}` interpolates its composed message.\n\n\
         **Enums decode from the wire values their JSON Schema advertises** (this holds for \
         `parse::<T>` and `decode_typed` too — one recipe walk serves all three). A *backed* enum \
         is selected by its backing: `enum Plan: string { Free = \"free\" }` decodes `\"free\"`, \
         not `\"Free\"`. A *plain* enum is selected by its case name. Those are exactly what each \
         derives as its `{\"enum\": […]}` schema, so a document a schema describes is a document \
         the decode accepts. Backings of any scalar type work, so an `int`-backed enum decodes \
         from JSON numbers. The result is a real enum value — it `match`es exhaustively and \
         compares equal to a case written in source, rather than a string standing in for one.\n\n\
         A value naming no case is an `\"unknown_variant\"` error whose detail lists every accepted \
         wire value, reported at the failing value's path — never a panic, never a silently-wrong \
         value. A value of the wrong JSON *kind* (an object where a string-backed enum was \
         expected) is an ordinary `\"mismatch\"`.\n\n\
         An enum with a **payload-carrying** variant has no JSON decoding at all, and a type with \
         such a field is refused at check time: a data-carrying sum has no canonical JSON \
         spelling, and decoding only its payload-free half would accept documents against a schema \
         that cannot describe the type. Build such a case with `construct(\"Enum.Variant\", \
         payload)` instead.",
    ),
    ("stringify", "Serialize a value to a JSON string."),
];

const LOG_DOCS: &[(&str, &str)] = &[
    ("debug", "Emit a debug-level log record with `message`."),
    ("info", "Emit an info-level log record with `message`."),
    ("warn", "Emit a warning-level log record with `message`."),
    ("error", "Emit an error-level log record with `message`."),
    (
        "debug_with",
        "Emit a debug-level record with `message` and structured key→value `fields`.",
    ),
    (
        "info_with",
        "Emit an info-level record with `message` and structured `fields`.",
    ),
    (
        "warn_with",
        "Emit a warning-level record with `message` and structured `fields`.",
    ),
    (
        "error_with",
        "Emit an error-level record with `message` and structured `fields`.",
    ),
    (
        "log",
        "Emit a log record at an arbitrary `level` with `message`.",
    ),
    (
        "log_with",
        "Emit a log record at an arbitrary `level` with `message` and structured `fields`.",
    ),
];

const CRYPTO_DOCS: &[(&str, &str)] = &[
    (
        "sha256",
        "The SHA-256 digest of the input (`string` or `bytes`) as raw `bytes`.",
    ),
    ("sha512", "The SHA-512 digest of the input as raw `bytes`."),
    (
        "sha1",
        "The SHA-1 digest as `bytes`. **Weak** — avoid for new security uses.",
    ),
    (
        "md5",
        "The MD5 digest as `bytes`. **Insecure** — for checksums/compatibility only, never security.",
    ),
    (
        "sha256_hasher",
        "A streaming SHA-256 `Hasher` — `.update(data)` incrementally, then `.digest()`.",
    ),
    (
        "sha512_hasher",
        "A streaming SHA-512 `Hasher` (see `sha256_hasher`).",
    ),
    (
        "hmac_sha256",
        "The HMAC-SHA-256 of `message` under `key`, as `bytes`.",
    ),
    (
        "hmac_sha512",
        "The HMAC-SHA-512 of `message` under `key`, as `bytes`.",
    ),
    (
        "hmac_sha256_verify",
        "Verify that `tag` is the HMAC-SHA-256 of `message` under `key`, in constant time.",
    ),
    (
        "hmac_sha512_verify",
        "Verify that `tag` is the HMAC-SHA-512 of `message` under `key`, in constant time.",
    ),
    (
        "bcrypt_hash",
        "Hash `password` with bcrypt at the given `cost` (work factor, typically 10–12), returning \
         the salted `$2b$` hash string.",
    ),
    (
        "bcrypt_verify",
        "Check `password` against a bcrypt `hash` in constant time; `true` on match.",
    ),
    (
        "constant_time_eq",
        "Compare two values byte-for-byte in constant time, so timing never leaks how much matched \
         — for secrets and MACs.",
    ),
    (
        "random_bytes",
        "`n` cryptographically secure random bytes from the system CSPRNG.",
    ),
];

const ID_DOCS: &[(&str, &str)] = &[
    ("uuid", "A random (version 4) `Uuid`."),
    (
        "uuid_v7",
        "A time-ordered (version 7) `Uuid` — sortable by creation time, ideal for database keys.",
    ),
    (
        "uuid_v5",
        "A deterministic (version 5) `Uuid` from a namespace UUID and a name — identical inputs \
         always yield the same UUID.",
    ),
    (
        "parse",
        "Parse a UUID string into a `Uuid`; `none` if malformed.",
    ),
    (
        "next_id",
        "A process-unique, monotonically increasing integer id.",
    ),
    (
        "namespace_dns",
        "The well-known DNS namespace `Uuid`, for deriving v5 UUIDs with `uuid_v5`.",
    ),
    (
        "namespace_url",
        "The well-known URL namespace `Uuid`, for `uuid_v5`.",
    ),
    (
        "namespace_oid",
        "The well-known OID namespace `Uuid`, for `uuid_v5`.",
    ),
    (
        "namespace_x500",
        "The well-known X.500 namespace `Uuid`, for `uuid_v5`.",
    ),
];

const HTTP_CLIENT_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "Perform an HTTP GET to `url` with optional headers, returning the `Response` (blocking).",
    ),
    (
        "head",
        "Perform an HTTP HEAD to `url` with optional headers, returning the `Response`.",
    ),
    (
        "delete",
        "Perform an HTTP DELETE to `url` with optional headers, returning the `Response`.",
    ),
    (
        "post",
        "Perform an HTTP POST to `url` with the given body and optional headers.",
    ),
    (
        "put",
        "Perform an HTTP PUT to `url` with the given body and optional headers.",
    ),
    (
        "query",
        "Perform an HTTP QUERY to `url` with the given body and optional headers.",
    ),
    (
        "request",
        "Perform an HTTP request with an arbitrary `method` to `url` — the general form the verb \
         helpers build on.",
    ),
    (
        "stream",
        "Read a response body **incrementally**, cut into frames: `stream(req, framing)` sends the \
         prepared request and returns a `FrameStream` whose `recv()` yields the next `Frame` (or \
         `none` at the end of the body). Use it for anything that arrives over time rather than all \
         at once — an LLM token stream, a progress feed, a log tail.\n\n\
         `framing` picks the cut. `Framing.Sse` parses `text/event-stream` (what OpenAI-compatible \
         endpoints speak) to the WHATWG rules: multi-line `data:` fields join with newlines, `event:`/\
         `id:`/`retry:` populate the frame, comments and blocks without data dispatch nothing. \
         `Framing.Ndjson` yields one JSON document per line, unparsed, in `data` (Ollama's native \
         shape). `Framing.Lines` yields one raw line per frame, blank lines included.\n\n\
         The `Err` arm means the request never produced a response — an HTTP error *status* opens a \
         stream normally, because an error page streams like any other body. **Check the status \
         before you drain it**: `stream.status()`/`ok()` answer from the response head, without a \
         `recv()`. A rate-limited provider replies `429` with a bare JSON error document, and since \
         that is not an event stream, `Framing.Sse` cuts it into zero frames — so an unchecked reader \
         sees an empty stream and cannot tell a rate limit from a model with nothing to say. \
         `stream.header(\"retry-after\")` carries the backoff, and \
         `client.stream(req, framing)?.error_for_status()?` short-circuits the whole case in one \
         line.\n\n\
         A body that is cut off mid-frame simply ends: with `Framing.Sse` the incomplete trailing \
         block is discarded, since a frame only exists once its terminating blank line arrives.\n\n\
         Call `close()` when abandoning a stream early; a drained one needs no close.",
    ),
    ("get_async", "Async `get` — yields a `Future<Response>`."),
    ("head_async", "Async `head`."),
    ("delete_async", "Async `delete`."),
    ("post_async", "Async `post`."),
    ("put_async", "Async `put`."),
    ("query_async", "Async `query`."),
    ("request_async", "Async `request`."),
];

const HTTP_URL_DOCS: &[(&str, &str)] = &[
    (
        "encode",
        "Percent-encode one URL component (RFC 3986), leaving `A-Za-z0-9-_.~` alone. Encoding is \
         over UTF-8 bytes, so one character may become several `%XX` escapes. Encode the pieces of \
         a query or path, then join them — encoding the whole thing would escape its separators.",
    ),
    (
        "decode",
        "Percent-decode one URL component: every `%XX` back to its byte — the exact inverse of \
         `encode`. A `+` stays a `+` (that it means a space is a *form*-encoding rule, and a plus \
         in a path is literal); a query parser substitutes it before decoding. Total — a stray `%` \
         stays a literal `%` and invalid UTF-8 is replaced, so a malformed URL is read rather than \
         refused.",
    ),
];

const HTTP_SERVER_DOCS: &[(&str, &str)] = &[
    (
        "serve",
        "Start an HTTP server on `port`, dispatching each `Request` to `handler` and replying with \
         its `Response`. Blocks, serving until the process exits.",
    ),
    (
        "response",
        "Build an HTTP `Response` from a status code, an optional body, and optional headers — what \
         a `serve` handler returns.",
    ),
    (
        "cookie",
        "Build a `Cookie` named `name` with `value`, to attach with `Response.with_cookie`. The \
         defaults are the safe ones — `Path=/`, `HttpOnly`, `SameSite=Lax`. An invalid name or \
         value is refused here, so a crafted value can never split the response.",
    ),
    (
        "websocket",
        "Upgrade the current request to a WebSocket, driving the connection with `handler(socket)`.",
    ),
    (
        "sse",
        "Answer the current request with a **server-sent events** stream, driving it with \
         `handler(sink)`. Return it from a `fetch` handler exactly like `server.websocket`: the \
         response head goes out as `text/event-stream`, the connection is held open, and the handler \
         pushes frames with `sink.send(frame)` until it returns (which closes the stream).\n\n\
         The one-way twin of `websocket`, and the write side of `client.stream`. Use it when the \
         client only needs to *listen* — a progress endpoint, a log tail, a build-status feed, an \
         LLM token stream re-emitted to a browser. It needs no handshake and no client opt-in, so any \
         request can be answered with one, and a browser consumes it with a plain `EventSource`.\n\n\
         Send `sink.comment(\"keepalive\")` periodically on an otherwise idle stream: it puts bytes on \
         the wire without dispatching an event, which stops an intermediary reaping the connection.",
    ),
    (
        "liveview_js",
        "The client-side LiveView JavaScript runtime as a string, to embed in a served page.",
    ),
];

const TASK_DOCS: &[(&str, &str)] = &[
    (
        "sleep",
        "A future that completes after `ms` milliseconds, yielding to other tasks meanwhile.",
    ),
    (
        "all",
        "Await every future in the list concurrently and return their results in order; fails if any \
         fails.",
    ),
    (
        "race",
        "Await the first future in the list to complete and return its result.",
    ),
    (
        "map_bounded",
        "Map `f` over the list concurrently with at most `limit` futures in flight at once, \
         preserving order.",
    ),
];

const REACTIVE_DOCS: &[(&str, &str)] = &[
    (
        "signal",
        "A writable reactive `Signal<T>` holding `value` — `.get()` reads it (tracking the reader), \
         `.set(v)` updates it and notifies dependents.",
    ),
    (
        "computed",
        "A derived `Computed<T>` that memoizes `f()` and recomputes when a signal it read changes.",
    ),
    (
        "effect",
        "Run `f` now and re-run it whenever a signal it read changes — for side effects; returns an \
         `Effect` handle to stop it.",
    ),
    (
        "view",
        "The current reactive `View` — the root for rendering reactive UI.",
    ),
];

const TEMPLATE_DOCS: &[(&str, &str)] = &[(
    "render",
    "Assemble a string from a template's literal `parts` and the rendered values of its `holes`, \
     interleaved — the desugaring target of `@template` string tiers.",
)];

const METRICS_DOCS: &[(&str, &str)] = &[
    (
        "counter",
        "A monotonically increasing `Counter` metric named `name` — record with `.add(n)`.",
    ),
    (
        "up_down_counter",
        "A `Counter` named `name` that can increase and decrease (`.add(n)`, negatives allowed).",
    ),
    (
        "gauge",
        "A `Gauge` metric named `name` recording a current value with `.record(v)`.",
    ),
    (
        "histogram",
        "A `Histogram` metric named `name` recording a distribution with `.record(v)`.",
    ),
];

const TRACING_DOCS: &[(&str, &str)] = &[
    (
        "span",
        "Start a new tracing `Span` named `name` in the current trace context.",
    ),
    (
        "span_from",
        "Start a `Span` named `name` as a child of the context serialized in `parent`.",
    ),
    (
        "current_context",
        "The current trace context serialized to a string, to propagate across a boundary (e.g. \
         into `span_from`).",
    ),
    (
        "with_span",
        "Run `f` inside a new span named `name`, closing the span when it returns; returns `f`'s \
         result.",
    ),
    (
        "set_attribute",
        "Set an attribute on the **active** span — the span you are already inside (a `with_span` \
         body, or a request handler under the auto-instrumented SERVER span), which no handle \
         names. Returns whether a live active span received it: `false` at top level, so the \
         no-span case is visible rather than silent. Use `Span.set_attribute` for a span you hold.",
    ),
    (
        "add_event",
        "Add a timestamped event to the **active** span. This is the annotation to reach for \
         instead of opening a short child span for something that merely *happened* during the \
         current unit of work — a child span per event inflates trace volume and buries the \
         signal. Returns whether a live active span received it (`false` at top level).",
    ),
    (
        "add_event_with",
        "Add a timestamped event carrying its own attributes to the **active** span. Prefer this \
         over `set_attribute` for a *structured fact*: several facts recorded on one span each keep \
         their own attribute set, where span-level attributes would overwrite each other by key. \
         Returns whether a live active span received it.",
    ),
    (
        "record_error",
        "Set the **active** span's status to error with `message` — mark the span you are inside \
         as failed without holding its handle. Returns whether a live active span received it; \
         check it on a path that must not lose an error (`if !tracing.record_error(msg) { \
         log.error(msg) }`), since at top level there is no span to carry it.",
    ),
];

const QUAT_DOCS: &[(&str, &str)] = &[
    (
        "mul",
        "The Hamilton product `a * b` — composes two rotations.",
    ),
    (
        "conjugate",
        "The conjugate of `q` (negates its vector part) — the inverse of a unit rotation.",
    ),
    (
        "normalize",
        "`q` scaled to unit length — a valid rotation quaternion.",
    ),
    ("length", "The magnitude (norm) of quaternion `q`."),
    ("dot", "The dot product of two quaternions."),
    (
        "rotate_vec3",
        "Rotate a 3-vector by the unit quaternion `q`.",
    ),
    (
        "slerp",
        "Spherical linear interpolation between rotations `a` and `b` by `t` in `[0, 1]` — smooth, \
         constant angular speed.",
    ),
];

const VEC_DOCS: &[(&str, &str)] = &[
    ("add", "The component-wise sum of two vectors."),
    ("sub", "The component-wise difference `a - b`."),
    (
        "scale",
        "Vector `v` scaled by the scalar `s` (component-wise).",
    ),
    ("dot", "The dot product of two vectors."),
    ("cross", "The cross product of two 3-vectors."),
    ("length", "The magnitude (Euclidean length) of vector `v`."),
    ("distance", "The Euclidean distance between two points."),
    ("normalize", "`v` scaled to unit length."),
    (
        "lerp",
        "Linear interpolation between `a` and `b` by `t` in `[0, 1]`.",
    ),
    ("clamp", "`v` clamped component-wise between `lo` and `hi`."),
    ("min", "The component-wise minimum of two vectors."),
    ("max", "The component-wise maximum of two vectors."),
    ("abs", "The component-wise absolute value of `v`."),
    (
        "reflect",
        "Reflect vector `v` about the plane with unit normal `n`.",
    ),
    (
        "add_all",
        "Bulk kernel: component-wise add across two flat packed vector buffers in one pass.",
    ),
    (
        "sub_all",
        "Bulk kernel: component-wise subtract across two packed vector buffers.",
    ),
    (
        "scale_all",
        "Bulk kernel: scale every vector in a packed buffer by a scalar.",
    ),
    (
        "dot_all",
        "Bulk kernel: the per-element dot products of two packed vector buffers.",
    ),
    (
        "length_all",
        "Bulk kernel: the magnitude of every vector in a packed buffer.",
    ),
];

// ---- Extern-type method prose (docs-browser Arc 2 A3), wired below via `docs:` on each ExtType. --

const CELL_METHOD_DOCS: &[(&str, &str)] = &[
    ("get", "The current value."),
    ("set", "Replace the stored value with `v`."),
    ("update", "Replace the value with `f(current)`."),
];

const HASHER_METHOD_DOCS: &[(&str, &str)] = &[
    ("update", "Feed more `data` into the running hash."),
    ("digest", "Finish and return the digest as `bytes`."),
];

const FILE_HANDLE_DOCS: &[(&str, &str)] = &[
    ("read", "Read up to `n` bytes from the cursor as a string."),
    (
        "read_line",
        "Read the next line (through the newline); empty at end of file.",
    ),
    ("write", "Write a string at the cursor, advancing it."),
    ("close", "Flush and close the handle."),
];

const REQUEST_DOCS: &[(&str, &str)] = &[
    ("method", "The HTTP method (`\"GET\"`, `\"POST\"`, …)."),
    ("path", "The request path."),
    (
        "query",
        "The value of query parameter `name`, or none. Percent-decoded: `?q=caf%C3%A9` reads as \
         `café` and `?title=buy+milk` as `buy milk`, since a query string is percent-encoded by \
         definition and every caller would otherwise hand-roll the same decoder.",
    ),
    ("url", "The full request URL, as received."),
    (
        "header",
        "The value of request header `name`, or none if absent.",
    ),
    ("body", "The request body as a string."),
    ("body_bytes", "The request body as raw `bytes`."),
    (
        "form",
        "The value of `application/x-www-form-urlencoded` body field `name`, or none — the same \
         wire format `query` parses, read from the body instead of the URL.",
    ),
    (
        "form_all",
        "Every decoded form field, by name. `form(name)`/`form_all()` mirror `cookie`/`cookies`: \
         the single lookup is the common case, the map is there when you want to iterate.",
    ),
    (
        "cookies",
        "Every cookie the client sent, by name. Empty when the request carries no `Cookie` \
         header. Values arrive exactly as they were sent — decoding is yours, since only you \
         know how you encoded them.",
    ),
    (
        "cookie",
        "The value of the cookie named `name`, or none. Cookie names are case-sensitive, unlike \
         header names.",
    ),
    (
        "with_header",
        "A copy of the request with header `name` set to `value`. The `Response.with_header` \
         shape: a middleware layer above std rewrites a request before passing it on, so \
         `Request` carries builders and not only accessors.",
    ),
    (
        "with_url",
        "A copy of the request pointed at `url` — the rewrite half of the same builder pair.",
    ),
];
const KEYRING_DOCS: &[(&str, &str)] = &[];
const SESSION_DOCS: &[(&str, &str)] = &[
    ("get", "The value stored under `name`, or none."),
    (
        "set",
        "A copy with `name` set to `value`, marked dirty so `session.attach` re-emits the cookie.",
    ),
    (
        "remove",
        "A copy without `name`. Marked dirty only if something was actually removed — otherwise a \
         speculative `remove` on every request would re-emit the cookie and keep extending its \
         own expiry.",
    ),
    (
        "clear",
        "A copy with nothing in it — the logout. `session.attach` turns an emptied session into an \
         expired cookie rather than a valid token for empty data, so the browser drops it at once.",
    ),
    (
        "dirty",
        "Whether this session changed since it was opened. `session.attach` consults it so an \
         unchanged session costs no header.",
    ),
    ("data", "Every entry, as a map."),
    (
        "with_id",
        "A copy tagged with an opaque server-side id — a stored backend's row key. Metadata, not a \
         data change, so it does not mark the session dirty, it never shows through `data()`, and \
         it survives `clear()` so a logout can still name the row to delete.",
    ),
    (
        "id",
        "The opaque server-side id a stored backend tagged this session with, or none. A \
         cookie-only session never has one.",
    ),
];
const SESSION_DOCS_MODULE: &[(&str, &str)] = &[
    (
        "keyring",
        "The signing secrets, newest first: signing uses the first, verification accepts any, so a \
         key can be rotated without logging everyone out. Each must be at least 16 bytes — \
         generate one with `crypto.random_bytes(32).to_hex()` and load it from the environment, \
         never a literal in source.",
    ),
    (
        "encode",
        "Sign `data` into a token valid for `max_age` seconds. Errors past the 4096-byte cookie \
         limit rather than emitting one a browser would silently drop.",
    ),
    (
        "decode",
        "Verify and decode a token, or none. A bad signature, an expired token, and a malformed \
         one are all none: a caller has one correct response to all three, and distinguishing them \
         would tell an attacker which guess was closer.",
    ),
    (
        "of",
        "Build a clean (unchanged) session from data — the inverse of `data()`. A stored backend \
         uses it to rebuild the session from a row it loaded, so a handler reads a stored session \
         exactly as it reads a cookie one. Clean, so a pure read never triggers a row write.",
    ),
    (
        "open",
        "Read the session off a request. Never fails — an absent, forged, or expired cookie all \
         give an empty session.",
    ),
    (
        "attach",
        "Write the session back to a response, but only if it changed. `secure` restricts the \
         cookie to https and has no default on purpose: `true` in production, `false` only for a \
         local plain-http server. Both wrong answers fail silently, so the choice is stated out \
         loud at the call.",
    ),
];
const COOKIE_DOCS: &[(&str, &str)] = &[
    ("name", "The cookie's name."),
    ("value", "The cookie's value."),
    (
        "with_value",
        "A copy with a new value. Rejects whitespace, `\"`, `,`, `;`, `\\`, and control characters \
         — encode (base64url) anything else first.",
    ),
    (
        "with_path",
        "A copy with `Path` set. Defaults to `/`. A browser only sends the cookie back to paths \
         under this one — and only *deletes* it when the deleting cookie's path matches.",
    ),
    (
        "with_domain",
        "A copy with `Domain` set, which widens the cookie to subdomains. Omitted by default, \
         which is the narrower and safer host-only behaviour.",
    ),
    (
        "with_max_age",
        "A copy with `Max-Age` set, in seconds. Omitted by default, making it a session cookie \
         the browser drops when it closes. `0` expires it immediately — see `expired`.",
    ),
    (
        "with_http_only",
        "A copy with `HttpOnly` set. On by default: it hides the cookie from JavaScript, so an \
         XSS bug cannot read a session out of `document.cookie`. Turn it off only for a cookie \
         the page's own scripts must read.",
    ),
    (
        "with_secure",
        "A copy with `Secure` set, restricting the cookie to https. Off by default so a \
         plain-http localhost server works; turn it on in production. Cannot be turned off on a \
         `SameSite=None` cookie, which a browser would then discard.",
    ),
    (
        "with_same_site",
        "A copy with `SameSite` set: `\"strict\"`, `\"lax\"` (the default), or `\"none\"`. This is \
         the cross-site request defence — `lax` sends the cookie on top-level navigations but not \
         on cross-site form posts or subresource requests. `\"none\"` implies `Secure` and sets it.",
    ),
    (
        "expired",
        "The deletion form of this cookie: same name, path, and domain, empty value, `Max-Age=0`. \
         Deleting means overwriting, and a browser only matches the overwrite when path and domain \
         match — which is why this is a method on the original rather than a free function.",
    ),
    (
        "to_header",
        "The `Set-Cookie` header value. Prefer `Response.with_cookie`, which attaches it \
         correctly; reach for this only when building the header yourself.",
    ),
];
const RESPONSE_DOCS: &[(&str, &str)] = &[
    ("status", "The HTTP status code."),
    ("ok", "Whether the status is 2xx."),
    (
        "header",
        "The value of response header `name`, or empty if absent.",
    ),
    ("body", "The response body as a string."),
    ("body_bytes", "The response body as raw `bytes`."),
    (
        "with_header",
        "A copy of the response with header `name: value` set.",
    ),
    (
        "headers_all",
        "Every value of response header `name`, in order, or empty if absent. The multi-value \
         read `header` cannot express, since it answers with the first match only.",
    ),
    (
        "with_cookie",
        "A copy of the response setting `cookie`. Replaces any cookie of the same name, and \
         otherwise adds one — so setting two different cookies keeps both, as `Set-Cookie` \
         requires, while setting the same one twice does what you meant.",
    ),
    (
        "url",
        "The final URL this response came from, after redirects — the correct base for resolving \
         a relative `Location` or `Link` target. Empty for a response the program built with \
         `http.server.response(…)`.",
    ),
    (
        "links",
        "The response's RFC 8288 `Link` relations as `rel -> target` (`links()[\"next\"]` is the \
         next page for any API that uses the standard header). Empty when the header is absent. \
         Targets may be relative — resolve them against `url()`.",
    ),
    (
        "json",
        "Decode the body into the caller-named type: `resp.json::<User>()` yields \
         `Result<User, JsonError>`. Recoverable by construction — a response body is remote \
         input, so a server that changes shape is a value you handle, not an abort. Use \
         `json.parse::<T>(resp.body())` when you do want a malformed body to be fatal.",
    ),
    (
        "error_for_status",
        "`Ok(self)` for a 2xx status, `Err(HttpError)` otherwise — the opt-in door for callers \
         who want a non-2xx to short-circuit through `?` like a transport failure. Requests \
         themselves never do this: a 404 is an answer, not a broken network.",
    ),
];
const SOCKET_DOCS: &[(&str, &str)] = &[
    ("send", "Send a message over the WebSocket."),
    (
        "recv",
        "Await the next message; `none` when the socket closes.",
    ),
    (
        "recv_timeout",
        "Await the next message for at most `ms` milliseconds; `none` if none arrived in time. \
         This is the door to a session that acts on its own schedule — push a periodic update, \
         poll a server-side source — instead of only when the client speaks. The deadline lives \
         *inside* the read rather than racing `recv` against a timer, because a race cancels the \
         losing `recv` and loses any message it had already consumed. Pair it with `closed()` to \
         tell the two `none`s apart.",
    ),
    (
        "closed",
        "Whether the peer has closed the connection — so a `none` from `recv_timeout` reads as \
         \"nothing yet\" rather than \"we are done\".",
    ),
    ("close", "Close the WebSocket connection."),
];

const UUID_METHOD_DOCS: &[(&str, &str)] = &[
    (
        "to_string",
        "The canonical hyphenated string form (`550e8400-e29b-…`).",
    ),
    (
        "version",
        "The UUID version number (4 = random, 5 = name-based, 7 = time-ordered).",
    ),
    (
        "timestamp_ms",
        "The embedded timestamp in milliseconds since the Unix epoch for a time-based UUID (v7); \
         `none` otherwise.",
    ),
];

const COUNTER_METHOD_DOCS: &[(&str, &str)] = &[
    ("add", "Add `n` to the counter."),
    (
        "add_with",
        "Add `n` with structured attributes attached to the measurement.",
    ),
];
const GAUGE_DOCS: &[(&str, &str)] = &[
    ("record", "Record the current value."),
    (
        "record_with",
        "Record the current value with structured attributes.",
    ),
];
const HISTOGRAM_DOCS: &[(&str, &str)] = &[
    ("record", "Record an observation into the distribution."),
    (
        "record_with",
        "Record an observation with structured attributes.",
    ),
];

const EXEC_RESULT_DOCS: &[(&str, &str)] = &[
    ("status", "The process exit code."),
    ("ok", "Whether the process exited successfully (status 0)."),
    ("stdout", "The captured standard output as a string."),
    ("stderr", "The captured standard error as a string."),
];
const PROCESS_DOCS: &[(&str, &str)] = &[
    ("pid", "The child process id."),
    (
        "wait",
        "Wait for the process to exit and return its status.",
    ),
    (
        "try_wait",
        "The exit status if the process has finished, else `none`, without blocking.",
    ),
    ("kill", "Terminate the process."),
    (
        "signal",
        "Send a named OS signal (e.g. `\"TERM\"`, `\"HUP\"`) to the process — the general form of `kill`.",
    ),
    (
        "wait_async",
        "Await the process's exit, yielding a `Future<ExecResult>` — the async twin of `wait`.",
    ),
    ("read", "Read available bytes from the process's stdout."),
    ("read_line", "Read the next line from the process's stdout."),
    (
        "read_err_line",
        "Read the next line from the process's stderr.",
    ),
    (
        "read_line_async",
        "Await the next line of the process's stdout, yielding a `Future<?string>` — the async twin \
         of `read_line`. This is how a child read is **bounded**: `race([p.read_line_async(), \
         task.tick(500)])`. The blocking `read_line` parks the whole isolate, so a sibling \
         watchdog task cannot fire while it waits.",
    ),
    (
        "read_err_line_async",
        "Await the next line of the process's stderr, yielding a `Future<?string>` — the async twin \
         of `read_err_line`, on its own cursor.",
    ),
    (
        "read_async",
        "Await up to `count` characters of the process's stdout, yielding a `Future<?string>` — \
         the async twin of `read`.",
    ),
    (
        "write",
        "Write to the process's stdin. **Aborts** (E0021) if the pipe is gone — a child that \
         exited is a broken pipe; use `try_write` when that is a case to handle.",
    ),
    (
        "try_write",
        "Recoverable `write`: `Result<void, OsError>` instead of an abort. Prefer it whenever the \
         child is a program you do not control, because no check can make the aborting door safe \
         — the child can exit between a `try_wait()` poll and the write that follows it.",
    ),
    (
        "close_stdin",
        "Close the process's stdin, signalling end of input.",
    ),
];

const SIGNAL_METHOD_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "The current value — tracked as a dependency when read inside a `computed`/`effect`.",
    ),
    ("set", "Set the value and notify dependents."),
    (
        "update",
        "Set the value to `f(current)` and notify dependents.",
    ),
];
const COMPUTED_METHOD_DOCS: &[(&str, &str)] = &[(
    "get",
    "The memoized derived value, recomputed only if a dependency changed.",
)];
const EFFECT_METHOD_DOCS: &[(&str, &str)] =
    &[("dispose", "Stop the effect so it no longer re-runs.")];
const VIEW_METHOD_DOCS: &[(&str, &str)] = &[
    ("snapshot", "A snapshot of the current reactive view tree."),
    (
        "diff",
        "The changes since the previous snapshot — what a client needs to patch.",
    ),
    (
        "expose",
        "Expose a named value into the view for the client.",
    ),
    (
        "unexpose",
        "Drop a named binding and dispose its handle — a diff never pushes it again and its scope reclaims.",
    ),
];

const SPAN_METHOD_DOCS: &[(&str, &str)] = &[
    (
        "set_attribute",
        "Attach a key→value attribute to the span. This is for a span you hold; to annotate the \
         span you are merely *inside* (a `with_span` body, a handler under the SERVER span), call \
         `tracing.set_attribute` — no handle needed.",
    ),
    (
        "add_event",
        "Record a timestamped event on the span. `tracing.add_event` does the same to the *active* \
         span, which is what you want instead of opening a short child span for something that \
         merely happened.",
    ),
    (
        "add_event_with",
        "Record a timestamped event carrying its own attributes. Prefer it over `set_attribute` for \
         a structured fact you may record more than once on one span — events accumulate, span \
         attributes overwrite by key.",
    ),
    (
        "record_error",
        "Record an error on the span (sets its status). `tracing.record_error` does the same to \
         the *active* span.",
    ),
    (
        "end",
        "End the span, fixing its duration. Deliberately absent from the active-span surface: \
         ending a span you did not open would close a `with_span`'s span early, or the \
         auto-instrumented SERVER span out from under `http.serve`.",
    ),
    (
        "context",
        "The span's trace context, serialized for propagation across a boundary.",
    ),
];

const RANDOM_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["seed"],
        name: "seed",
        params: &[Int],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["low", "high"],
        name: "int",
        params: &[Int, Int],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "float",
        params: &[],
        ret: Concrete(Float),
    },
];

const TIME_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "monotonic",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &["ms"],
        name: "sleep",
        params: &[Int],
        ret: Concrete(SigType::Unit),
    },
];

const ID_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "next_id",
        params: &[],
        ret: Concrete(Int),
    },
    // `uuid()` is v4 — the "just give me a UUID" default; `uuid_v7()` (time-ordered keys) is the
    // explicit opt-in. Both return the first-class `Uuid` (extern-types X2), which displays in
    // canonical hyphenated lowercase.
    ExtFn {
        param_names: &[],
        name: "uuid",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "uuid_v7",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        param_names: &["text"],
        name: "parse",
        params: &[Str],
        ret: Concrete(SigType::Option(&UUID_SIG)),
    },
    // Name-based UUIDs (crypto arc C5): pure — same namespace + name = same UUID, everywhere.
    ExtFn {
        param_names: &["namespace", "name"],
        name: "uuid_v5",
        params: &[UUID_SIG, Str],
        ret: Concrete(UUID_SIG),
    },
    // The RFC 9562 well-known namespaces, as zero-arg constructors (a module has no constants).
    ExtFn {
        param_names: &[],
        name: "namespace_dns",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "namespace_url",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "namespace_oid",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        param_names: &[],
        name: "namespace_x500",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
];

/// The `Uuid` signature type, named once (`SigType::Option` borrows a static).
const UUID_SIG: SigType = SigType::Named(crate::id::TYPE_NAME);

/// The `Uuid` instance methods (extern-types X2): all pure (`key_capable` demands it).
/// `version()` reads the version nibble back; `timestamp_ms()` is `some(ms)` iff the version
/// carries a timestamp (v7) — the Option IS the version distinction.
const UUID_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "version",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "timestamp_ms",
        params: &[],
        ret: Concrete(SigType::Option(&SigType::Int)),
    },
];

/// Method dispatch for `Uuid` — downcast the receiver, run the pure accessor. No mutation, no
/// host (the whole point of `key_capable`).
fn uuid_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(u) = recv.as_any().downcast_ref::<crate::id::Uuid>() else {
        return Err(type_error(method, "Uuid"));
    };
    match method {
        "to_string" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(u.to_string()))
        }
        "version" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(u.get_version_num() as i64)))
        }
        "timestamp_ms" => {
            want_arity(method, args, 0)?;
            Ok(match crate::id::timestamp_ms(u) {
                Some(ms) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(ms as i64)))),
                None => NativeOut::None,
            })
        }
        _ => Err(crate::no_method_error(crate::id::TYPE_NAME, method)),
    }
}

/// The `Map<string, string>` a `.env` parse/load yields (F5) — shared by `env.parse`/`env.load`.
const STR_MAP: SigType = SigType::Map(&Str, &Str);

const ENV_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["name"],
        name: "get",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        param_names: &[],
        name: "keys",
        params: &[],
        ret: Concrete(SigType::List(&Str)),
    },
    // `.env` support folded into the same namespace (F5): a pure parser and a file loader that
    // applies a `.env`'s defaults under real-env-wins precedence.
    ExtFn {
        param_names: &["text"],
        name: "parse",
        params: &[Str],
        ret: Concrete(STR_MAP),
    },
    ExtFn {
        param_names: &["path"],
        name: "load",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(STR_MAP),
    },
    // `set(key, value)` writes the program's view of the environment (stdlib-gaps): sandbox
    // fixture map, or `RealHost`'s thread-safe overlay (children via `os.exec` observe it).
    ExtFn {
        param_names: &["name", "value"],
        name: "set",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
];

const ARGS_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "all",
    params: &[],
    ret: Concrete(SigType::List(&Str)),
}];

/// The `ExecResult` signature — `os.exec`'s return (stdlib-gaps).
const EXEC_RESULT_SIG: SigType = SigType::Named(crate::os::EXEC_RESULT_TYPE_NAME);

/// The `Process` signature — `os.spawn`'s return (process-handle arc).
const PROCESS_SIG: SigType = SigType::Named(crate::os::PROCESS_TYPE_NAME);

/// The `os` module (stdlib-gaps): system introspection leaves + subprocess execution + exit.
const OS_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "platform",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "arch",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "hostname",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "cpus",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &[],
        name: "cwd",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "pid",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        param_names: &["command", "args"],
        name: "exec",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(EXEC_RESULT_SIG),
    },
    ExtFn {
        param_names: &["command", "args"],
        name: "exec_async",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(SigType::Future(&EXEC_RESULT_SIG)),
    },
    // `spawn(command, args?)` — start a child and return a controllable `Process` handle.
    ExtFn {
        param_names: &["command", "args"],
        name: "spawn",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(PROCESS_SIG),
    },
    // `try_spawn(command, args?)` — the recoverable twin (subprocess-doors arc).
    ExtFn {
        param_names: &["command", "args"],
        name: "try_spawn",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(SigType::Result(&PROCESS_SIG, &OS_ERROR_SIG)),
    },
    // `exit(code?)` — the archetypal diverging call: it unwinds the backend with
    // `ErrorKind::Exit` and the process is gone. Declared `never`, not `void`: "returns nothing"
    // and "does not return" are different facts, and only the second one tells a tier runner
    // that `os.exit(run())` at the top of a CLI entry must not join the shared test setup.
    ExtFn {
        param_names: &["code"],
        name: "exit",
        params: &[SigType::Optional(&Int)],
        ret: Concrete(SigType::Never),
    },
    // `shell_quote(s)` — POSIX-shell-safe quoting for the explicit `sh -c` escape hatch.
    ExtFn {
        param_names: &["text"],
        name: "shell_quote",
        params: &[Str],
        ret: Concrete(Str),
    },
];

const FS_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["path", "contents"],
        name: "write",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["path", "contents"],
        name: "append",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["path", "contents"],
        name: "write_bytes",
        params: &[Str, SigType::Bytes],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["path"],
        name: "read_bytes",
        params: &[Str],
        ret: Concrete(SigType::Bytes),
    },
    // A.10 residue: the async twin of `read_bytes` — a `Future<bytes>`.
    ExtFn {
        param_names: &["path"],
        name: "read_bytes_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bytes)),
    },
    ExtFn {
        param_names: &["path"],
        name: "read",
        params: &[Str],
        ret: Concrete(Str),
    },
    // Track A.4c/A.10: the async twins of `read`/`write`/`append` — each returns a `Future<T>` an
    // async context `.await`s. On the sandbox they resolve deterministically (in-oracle); on the real
    // executor they suspend and the IO runs concurrently on tokio (CLI-only, out-of-oracle).
    ExtFn {
        param_names: &["path"],
        name: "read_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&Str)),
    },
    ExtFn {
        param_names: &["path", "contents"],
        name: "write_async",
        params: &[Str, Str],
        ret: Concrete(SigType::Future(&SigType::Unit)),
    },
    ExtFn {
        param_names: &["path", "contents"],
        name: "append_async",
        params: &[Str, Str],
        ret: Concrete(SigType::Future(&SigType::Unit)),
    },
    // The async metadata twins (extern-types X6) — pure `FsIo` additions: no backend code
    // changed to add these, which is the point of the open seam.
    ExtFn {
        param_names: &["path"],
        name: "exists_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bool)),
    },
    ExtFn {
        param_names: &["path"],
        name: "remove_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bool)),
    },
    // Trailing-optional dir, like the sync `list` (package-manager N3.4).
    ExtFn {
        param_names: &["path"],
        name: "list_async",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(SigType::Future(&SigType::List(&Str))),
    },
    ExtFn {
        param_names: &["path"],
        name: "read_lines",
        params: &[Str],
        ret: Concrete(SigType::List(&Str)),
    },
    ExtFn {
        param_names: &["path"],
        name: "exists",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["path"],
        name: "remove",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["path"],
        name: "is_dir",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["path"],
        name: "mkdir",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    // A.10 residue: the async directory twins — `is_dir_async` → `Future<bool>`, `mkdir_async`
    // → `Future<void>`. `list_async` already covers directory listing.
    ExtFn {
        param_names: &["path"],
        name: "is_dir_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bool)),
    },
    ExtFn {
        param_names: &["path"],
        name: "mkdir_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Unit)),
    },
    // `list([dir])` — the directory argument is trailing-optional (the http-arc H4 machinery,
    // which post-dates this function's old "checker special-cases the arity" note).
    ExtFn {
        param_names: &["path"],
        name: "list",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(SigType::List(&Str)),
    },
    ExtFn {
        param_names: &["path", "mode"],
        name: "open",
        params: &[Str, Str],
        ret: Concrete(SigType::Named("FileHandle")),
    },
];

// The *scalar* `vec`/`quat` ops (the bulk `*_all` kernels are ctx functions — see
// `crate::vec3::VEC_CTX_FNS`). Structural arguments are `Dyn` (the 3/4-`f32` shape is checked at
// dispatch); object results are `SameAsArg` (same shape as the indicated argument).
const VEC_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["a", "b"],
        name: "add",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "sub",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "cross",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["v", "normal"],
        name: "reflect",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "min",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "max",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["v"],
        name: "abs",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["v"],
        name: "normalize",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["v", "factor"],
        name: "scale",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b", "t"],
        name: "lerp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["v", "lo", "hi"],
        name: "clamp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "dot",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "distance",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        param_names: &["v"],
        name: "length",
        params: &[Dyn],
        ret: Concrete(SigType::F32),
    },
];

const QUAT_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["a", "b"],
        name: "mul",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["q"],
        name: "conjugate",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["q"],
        name: "normalize",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b", "t"],
        name: "slerp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        param_names: &["a", "b"],
        name: "dot",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        param_names: &["q"],
        name: "length",
        params: &[Dyn],
        ret: Concrete(SigType::F32),
    },
    // `rotate_vec3(q, v)` returns the *vector* (its second argument's shape).
    ExtFn {
        param_names: &["q", "v"],
        name: "rotate_vec3",
        params: &[Dyn, Dyn],
        ret: SameAsArg(1),
    },
];

// --- `json`: parse (dynamic) + stringify, over the recursive value seam ------------------------
//
// `json.parse(text)` decodes into a dynamic value tree (`NativeOut::Map`/`List`/scalars); the
// turbofish form `json.parse::<T>(text)` is a separate call-site-typed path (`Op::TypedModuleCall` + a
// `TypeRecipe`), not this dynamic dispatch. `json.stringify(value)` serializes a **deeply**
// marshalled argument (the module sets `deep_marshal`) through the shared `json::stringify`.

/// `JsonError`'s signature spelling — the error arm of every recoverable `json` door.
const JSON_ERROR_SIG: SigType = SigType::Named(crate::json::JSON_ERROR_TYPE_NAME);

/// What the recoverable **dynamic** door returns: `Result<dyn, JsonError>`.
///
/// The non-turbofish twin of `try_parse::<T>`'s `Result<T, JsonError>`, and the only recoverable
/// decode that needs no declared recipe — which is what makes it the right door for a body read off
/// a wire, where the shape is the remote party's and a malformed document must be a value the
/// program handles rather than an abort.
const DYN_JSON_RESULT_SIG: SigType = SigType::Result(&Dyn, &JSON_ERROR_SIG);

const JSON_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["text"],
        name: "parse",
        params: &[Str],
        ret: Concrete(Dyn),
    },
    // The recoverable dynamic door. It shares the name `try_parse` with the call-site-typed
    // `try_parse::<T>` below, exactly as `parse` shares its name with `parse::<T>`: the plain and
    // turbofish call surfaces are separate tables, so a name in both is two doors, not a collision.
    ExtFn {
        param_names: &["text"],
        name: "try_parse",
        params: &[Str],
        ret: Concrete(DYN_JSON_RESULT_SIG),
    },
    ExtFn {
        param_names: &["value"],
        name: "stringify",
        params: &[Dyn],
        ret: Concrete(Str),
    },
];

// The **call-site-typed** doors — the turbofish forms `json.parse::<T>` (aborting) and
// `json.try_parse::<T>` (recoverable → `Result<T, JsonError>`). A separate table from `JSON_FNS`:
// the dynamic `parse(text): dyn` and the typed `parse::<T>: T` legitimately share the name
// `parse` — and, since the recoverable dynamic door landed, `try_parse` is the second such pair
// (`try_parse(text): Result<dyn, JsonError>` here, `try_parse::<T>(): Result<T, JsonError>` there).
// The two call surfaces are disjoint tables, so sharing a name is two doors, not a collision.
// Each declares `RetTy::TypeArg` with the wrapper the checker types the call by;
// `json_typed_dispatch` produces the matching `NativeOut` tree threaded with the resolved recipe.
const JSON_TYPED_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["text"],
        name: "parse",
        params: &[Str],
        ret: TypeArg(TypeArgWrap::Plain),
    },
    ExtFn {
        param_names: &["text"],
        name: "try_parse",
        params: &[Str],
        ret: TypeArg(TypeArgWrap::Result(JSON_ERROR_SIG)),
    },
];

fn json_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "parse" => {
            want_arity(func, args, 1)?;
            crate::json::parse_dynamic(want_str(func, args, 0)?)
        }
        // The recoverable dynamic door: it never uses the `Err` channel (that would be an abort),
        // returning the whole `Result` inside the `NativeOut` — the same contract `try_parse::<T>`
        // honors below, so both backends materialize one tree and stay identical by construction.
        "try_parse" => {
            want_arity(func, args, 1)?;
            Ok(
                match crate::json::try_parse_dynamic(want_str(func, args, 0)?) {
                    Ok(out) => NativeOut::Ok(Box::new(out)),
                    Err(error) => {
                        NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error))))
                    }
                },
            )
        }
        "stringify" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(crate::json::stringify(&args[0])))
        }
        _ => Err(no_function_error("json", func)),
    }
}

/// The `json` module's call-site-typed dispatch (`json.parse::<T>` / `json.try_parse::<T>`): decode
/// the string argument against the checker-resolved `recipe` into a value of the turbofish `T`.
/// `parse` is the **aborting** door — a decode failure is `Err(StdError)`, a runtime abort. `try_parse`
/// is the **recoverable** door — it never uses the `Err` channel, returning the whole `Result` inside
/// the `NativeOut` (`Ok(value)` on success, `Err(JsonError)` — a path-rich extern — on failure), so
/// both backends materialize one tree and stay byte-identical to the former hardcoded branch.
fn json_typed_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError> {
    match func {
        "parse" => {
            want_arity(func, args, 1)?;
            crate::json::parse_typed(want_str(func, args, 0)?, recipe)
        }
        "try_parse" => {
            want_arity(func, args, 1)?;
            Ok(
                match crate::json::try_parse_typed(want_str(func, args, 0)?, recipe) {
                    Ok(out) => NativeOut::Ok(Box::new(out)),
                    Err(error) => {
                        NativeOut::Err(Box::new(NativeOut::Extern(crate::ExternBox::new(error))))
                    }
                },
            )
        }
        _ => Err(no_function_error("json", func)),
    }
}

/// The `JsonError` instance methods (error-machinery arc): pure reads over the decode failure —
/// the `ExecResult` accessor model. `message` is `impl Error`'s required method; `to_string` is
/// `impl Display`'s (both declared on the type's registration below), and both return the same
/// composed message the value also displays as.
const JSON_ERROR_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "message",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "kind",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "path",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        param_names: &[],
        name: "line",
        params: &[],
        ret: Concrete(SigType::Option(&Int)),
    },
    ExtFn {
        param_names: &[],
        name: "column",
        params: &[],
        ret: Concrete(SigType::Option(&Int)),
    },
];

const JSON_ERROR_DOCS: &[(&str, &str)] = &[
    (
        "message",
        "The composed human message — the path-prefixed detail (`items[2].price: expected float, \
         found JSON number`). The `Error` trait's required method.",
    ),
    (
        "to_string",
        "Same as `message()` — the `Display` rendering, so `${e}` interpolates the message.",
    ),
    (
        "kind",
        "What went wrong: `\"syntax\"` (malformed document), `\"mismatch\"` (wrong value kind), \
         `\"missing_field\"`, `\"unknown_variant\"` (a right-kind value naming no case of a target \
         enum — the detail lists every accepted one), `\"validation\"` (a shape-correct value its \
         type's `Validate::validate` rejected), or `\"unknown_type\"` (a `decode_typed` name with \
         no recipe).\n\n\
         `\"unknown_variant\"` is deliberately distinct from `\"mismatch\"`: a mismatch means the \
         document has the wrong *shape*, while this means it has the right shape and an \
         out-of-vocabulary *value* — something a caller can act on, since the accepted set is in \
         the message.",
    ),
    (
        "path",
        "The path from the document root to the failing value (`items[2].price`); empty for a \
         document-level failure.",
    ),
    (
        "line",
        "The 1-based source line of a `syntax` failure, `none` otherwise.",
    ),
    (
        "column",
        "The 1-based source column of a `syntax` failure, `none` otherwise.",
    ),
];

fn json_error_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::json::{JSON_ERROR_TYPE_NAME, JsonError};
    let Some(error) = recv.as_any().downcast_ref::<JsonError>() else {
        return Err(type_error(method, JSON_ERROR_TYPE_NAME));
    };
    want_arity(method, args, 0)?;
    let opt_int = |v: Option<u32>| match v {
        Some(n) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(i64::from(n))))),
        None => NativeOut::None,
    };
    match method {
        // `message` (Error) and `to_string` (Display) are the same composed message by design.
        "message" | "to_string" => Ok(NativeOut::Str(error.message())),
        "kind" => Ok(NativeOut::Str(error.kind.label().to_string())),
        "path" => Ok(NativeOut::Str(error.path.clone())),
        "line" => Ok(opt_int(error.line)),
        "column" => Ok(opt_int(error.column)),
        _ => Err(crate::no_method_error(JSON_ERROR_TYPE_NAME, method)),
    }
}

/// [`CoreExtension`]'s modules — the always-on Ring-1/2 surface (no separable heavy native dep):
/// pure scalar/collection/host-IO/introspection plus the higher-order concurrency primitives.
const CORE_MODULES: &[ExtModule] = &[
    ExtModule {
        name: "math",
        functions: MATH_FNS,
        dispatch: math_dispatch,
        deep_marshal: false,
        docs: MATH_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "random",
        functions: RANDOM_FNS,
        dispatch: random_dispatch,
        deep_marshal: false,
        docs: RANDOM_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "time",
        functions: TIME_FNS,
        dispatch: time_dispatch,
        deep_marshal: false,
        docs: TIME_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "env",
        functions: ENV_FNS,
        dispatch: env_dispatch,
        deep_marshal: false,
        docs: ENV_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `os` (stdlib-gaps): system introspection + subprocess exec + exit over the Os capability.
    // `deep_marshal` so `exec`'s `List<string>` argv arrives as a full `NativeValue::List`
    // (like `http`'s headers map) — the shallow projection collapses containers to opaque.
    ExtModule {
        name: "os",
        functions: OS_FNS,
        dispatch: os_dispatch,
        deep_marshal: true,
        docs: OS_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `tracing` (native OTEL T1–T2) — the tracing SDK facade. `span`/`with_span`/`current_context`
    // reach the per-run active-span stack (and `with_span` calls a closure), so they are ctx
    // functions; the `Span` type's own methods stay plain (they only touch the host). The span tree
    // lives host-side (recorder / OTLP exporter).
    ExtModule {
        name: "tracing",
        ctx_functions: crate::tracing::TRACING_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::tracing::tracing_ctx_dispatch(func, ctx, args)),
        docs: TRACING_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `log` (native OTEL Phase L) — the logs SDK facade. Emits OTel `LogRecord`s auto-correlated to
    // the active span, so its functions read the per-task active-span stack and are ctx functions
    // (like `tracing`). Records go host-side (recorder / OTLP `/v1/logs` exporter), never to stdout.
    ExtModule {
        name: "log",
        ctx_functions: crate::log::LOG_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::log::log_ctx_dispatch(func, ctx, args)),
        docs: LOG_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `metrics` (native OTEL Phase M) — the metrics SDK facade. Instrument constructors are
    // get-or-create over host-owned aggregation, so they are ctx functions; the `Counter`/`Histogram`/
    // `Gauge` handle methods are plain (host-only). Aggregation + export live host-side.
    ExtModule {
        name: "metrics",
        ctx_functions: crate::metrics::METRICS_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::metrics::metrics_ctx_dispatch(func, ctx, args)),
        docs: METRICS_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `io` (CLI-completion slice 1) — the program's stdout/stderr streams. `out`/`outln` write to
    // the same stdout buffer the `echo` keyword uses; `err`/`errln` write to stderr. Both are
    // *observable output* routed through the backends' buffers (via the `NativeCtx` write seam), so
    // they are ctx functions and the differential oracle holds them byte-identical across backends.
    // The `io` module carries BOTH dispatch tables: the ctx `out`/`outln`/`err`/`errln` reach the
    // backends' output buffers, while the host-backed `stdin_*`/`is_tty`/`prompt` (CLI-completion
    // slice 2) are plain `Console`-capability effects (sandbox fixture / real stdin). The two name
    // sets are disjoint (assembly enforces it), and the backends resolve plain `functions` before
    // `ctx_functions`, so each call reaches the table that declares it.
    ExtModule {
        name: "io",
        functions: crate::io::IO_FNS,
        dispatch: crate::io::io_dispatch,
        ctx_functions: crate::io::IO_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::io::io_ctx_dispatch(func, ctx, args)),
        docs: IO_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "args",
        functions: ARGS_FNS,
        dispatch: args_dispatch,
        deep_marshal: false,
        docs: ARGS_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "fs",
        functions: FS_FNS,
        dispatch: fs_dispatch,
        deep_marshal: false,
        docs: FS_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `base64` (RFC 4648) — the binary-over-text envelope every LLM provider, MCP resource, JWT,
    // and data URI speaks. Pure and dep-light (the `base64` crate is already linked for HTTP Basic
    // auth), so it is always-on core with no ring of its own.
    ExtModule {
        name: "base64",
        functions: BASE64_FNS,
        dispatch: base64_dispatch,
        deep_marshal: false,
        docs: BASE64_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "json",
        functions: JSON_FNS,
        dispatch: json_dispatch,
        // `json.stringify` introspects an arbitrary value, so its arguments are marshalled deeply.
        deep_marshal: true,
        docs: JSON_DOCS,
        // The turbofish decode doors (`json.parse::<T>` / `json.try_parse::<T>`).
        typed_functions: JSON_TYPED_FNS,
        typed_dispatch: Some(json_typed_dispatch),
        ..ExtModule::DEFAULTS
    },
    // The `task` concurrency module (higher-order-abi H0/H2): its functions need the executor,
    // so they live in the **ctx** table and dispatch through the `NativeCtx` seam.
    ExtModule {
        name: "task",
        ctx_functions: crate::task::TASK_CTX_FNS,
        ctx_dispatch: Some(crate::task::task_ctx_dispatch),
        docs: TASK_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `cell` (higher-order-abi H4) — the Class-3 proving module: `cell.new(v)` retains the value
    // in the per-run arena and hands back a `Cell<T>` extern handle.
    ExtModule {
        name: "cell",
        ctx_functions: crate::cell::CELL_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::cell::cell_ctx_dispatch(func, ctx, args)),
        docs: CELL_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `reactive` (higher-order-abi H5) — the last virtual module, now fully registry-backed:
    // creation retains the value/body into the arena and hands back a generic extern handle.
    ExtModule {
        name: "reactive",
        ctx_functions: crate::reactive::REACTIVE_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| {
            crate::reactive::reactive_ctx_dispatch(func, ctx, args)
        }),
        docs: REACTIVE_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `template` (expr-tiers arc) — the native handler for the `@json` expression tier: takes the
    // block's statics and hole closures, returns the rendered string. The dogfood proving a native
    // package can ship an expression tier with a native handler.
    ExtModule {
        name: "template",
        ctx_functions: crate::template::TEMPLATE_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| {
            crate::template::template_ctx_dispatch(func, ctx, args)
        }),
        docs: TEMPLATE_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// [`IdExtension`]'s module — sequential ids + UUIDs (id-entropy U2).
const ID_MODULES: &[ExtModule] = &[ExtModule {
    name: "id",
    functions: ID_FNS,
    dispatch: id_dispatch,
    deep_marshal: false,
    docs: ID_DOCS,
    ..ExtModule::DEFAULTS
}];

/// [`CryptoExtension`]'s module — digests / HMAC / bcrypt (crypto arc).
const CRYPTO_MODULES: &[ExtModule] = &[ExtModule {
    name: "crypto",
    functions: CRYPTO_FNS,
    dispatch: crypto_dispatch,
    deep_marshal: false,
    docs: CRYPTO_DOCS,
    ..ExtModule::DEFAULTS
}];

/// [`HttpExtension`]'s modules — the outbound client (its own ring) and inbound server (P0.3b split).
const HTTP_MODULES: &[ExtModule] = &[
    ExtModule {
        // The outbound client (package-manager P0.3b): `get`/`post`/…/`_async`. Its reqwest/TLS tree
        // is the ~5 MB `ring-http-client` payload, so isolating it from the server lets a
        // server-only program shed it. `http_dispatch` is shared with the server module (the two
        // function-name sets are disjoint, so one func-name router serves both).
        name: "http.client",
        functions: HTTP_CLIENT_FNS,
        dispatch: http_dispatch,
        // The optional `headers` argument is a `Map` — needs the deep marshalling that surfaces
        // it as `NativeValue::Map` (http arc H5). url/body strings project fine either way.
        deep_marshal: true,
        // The reqwest/TLS tree (~3 MB) rides behind this ring — a tailored AOT archive links it only
        // when the program can reach a client function (package-manager P1.0). Single source of truth
        // for the module→ring map the footprint scan reads.
        ring: Some("ring-http-client"),
        docs: HTTP_CLIENT_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        // Signed-cookie sessions (session arc S2/S3). Registered in this unit rather than its own
        // because `open`/`attach` name `Request` and `Response`; the module is still `std.session`,
        // since a session is a concept above HTTP and the codec half has no HTTP in it at all.
        //
        // No ring: `crypto` and `base64` are already linked, so there is no separable native
        // payload to gate.
        name: "session",
        functions: SESSION_FNS,
        dispatch: session_dispatch,
        // `encode`/`decode` take and return `Map<string, string>`.
        deep_marshal: true,
        ring: None,
        docs: SESSION_DOCS_MODULE,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        // The inbound server (package-manager P0.3b): the pure `response` builder + the `serve`
        // accept→dispatch→reply loop. `serve` (higher-order-abi H3) is a higher-order orchestrator
        // (closure handler, many futures in flight), so it lives in the ctx table. No reqwest.
        name: "http.server",
        functions: HTTP_SERVER_FNS,
        dispatch: http_dispatch,
        deep_marshal: true,
        ctx_functions: crate::serve::HTTP_CTX_FNS,
        ctx_dispatch: Some(crate::serve::http_ctx_dispatch),
        // The inbound serve loop rides tokio (already linked for `fs`) — no separable native dep, so
        // no ring. A `use std.http.server` program links no reqwest, precisely (P0.3b split).
        ring: None,
        docs: HTTP_SERVER_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        // Percent-encoding (RFC 3986). Native because the transformation is over UTF-8 BYTES, which
        // Noeta's scalar-based string surface does not reach — a `.noe` implementation gets ASCII
        // right and mangles everything else. It sits under `http` because every consumer is HTTP:
        // assembling a query string, taking one apart, escaping a path segment.
        //
        // No ring and no host: both functions are pure string transformations, so a program that
        // uses them links no transport at all.
        name: "http.url",
        functions: HTTP_URL_FNS,
        dispatch: http_url_dispatch,
        deep_marshal: false,
        ring: None,
        docs: HTTP_URL_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// [`VecExtension`]'s modules — the `vec`/`quat` packed-3D-math pair (extraction-prep unit).
const VEC_MODULES: &[ExtModule] = &[
    ExtModule {
        name: "vec",
        functions: VEC_FNS,
        dispatch: vec_dispatch,
        deep_marshal: false,
        // The bulk `*_all` kernels (package-manager N3.4): they read/produce packed buffers
        // through the raw-buffer ctx seam, so they live in the ctx table — the LAST per-backend
        // intercepts, migrated.
        ctx_functions: crate::vec3::VEC_CTX_FNS,
        ctx_dispatch: Some(crate::vec3::vec_ctx_dispatch),
        // The same kernels as opt-in METHODS (`impl vec.Kernels for T {}`). Since the ExtBundle→ExtTrait
        // fold-in (slice 4) they are native `ExtTrait`s — [`VEC_TRAITS`], registered through
        // `VecExtension::traits()` — not a module `bundles` field. The TWO unified traits generic over
        // the element type: `vec.Kernels` (default arithmetic, every numeric width incl. f64) and
        // `vec.SatKernels` (saturating, integer/`Color`); one generic kernel body per op serves every width.
        docs: VEC_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "quat",
        functions: QUAT_FNS,
        dispatch: quat_dispatch,
        deep_marshal: false,
        docs: QUAT_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// Compiled-in fast route for ctx **functions** (H5 perf): the same generic dispatch fns the
/// dyn table stores, instantiated over the backend's **concrete** ctx so every small ctx op
/// (arena read/write, slot bookkeeping, closure call) inlines — the Rust generics/dyn duality
/// applied to the extension ABI. `None` = the module is not compiled in (a future
/// dynamically-loaded extension); the caller falls back to the dyn table, which behaves
/// identically, just without the inlining.
#[inline]
pub fn static_dispatch_ctx<C: crate::NativeCtx + ?Sized>(
    module: &str,
    func: &str,
    ctx: &mut C,
    args: &[crate::Slot],
) -> Option<Result<crate::CtxOut, crate::CtxError>> {
    if !has_static_ctx_route(module) {
        // `para.synced` is out-of-`std` (the para-p2p package) — it has no compiled-in fast route here
        // and dispatches through the registered ExtModule's dyn `ctx_dispatch` instead. Nor does
        // an out-of-std module that merely *ends* in `.cell`/`.reactive` (a session extension):
        // only std's own identities take the compiled-in route.
        return None;
    }
    match module_name(module) {
        "cell" => Some(crate::cell::cell_ctx_dispatch(func, ctx, args)),
        "reactive" => Some(crate::reactive::reactive_ctx_dispatch(func, ctx, args)),
        _ => None,
    }
}

/// Whether `module` names a compiled-in ctx fast route ([`static_dispatch_ctx`]) — split out so
/// the route keys are testable against the exact identity the compiler emits. Module identities
/// are **root-qualified** end to end since the namespaced-types arc (`use std.cell` compiles to
/// the constant `"std.cell"`), which is what this matches; the bare spellings are kept for any
/// pre-qualification caller. This predicate rotted silently once before: the match keyed on the
/// bare names after identities became qualified, so the monomorphized H5 route never fired and
/// everything fell through to the dyn table with no behavioral difference to notice.
#[inline]
pub fn has_static_ctx_route(module: &str) -> bool {
    matches!(module, "std.cell" | "std.reactive" | "cell" | "reactive")
}

/// Compiled-in fast route for ctx **type methods** (H5 perf) — the type-method twin of
/// [`static_dispatch_ctx`].
#[inline]
pub fn static_dispatch_ctx_method<C: crate::NativeCtx + ?Sized>(
    type_name: &str,
    method: &str,
    ctx: &mut C,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Option<Result<crate::CtxOut, crate::CtxError>> {
    match type_name {
        crate::cell::CELL_TYPE_IDENTITY => Some(crate::cell::cell_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::SIGNAL_TYPE_IDENTITY => Some(crate::reactive::signal_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::COMPUTED_TYPE_IDENTITY => Some(
            crate::reactive::computed_ctx_method_dispatch(method, ctx, recv, args),
        ),
        crate::reactive::EFFECT_TYPE_IDENTITY => Some(crate::reactive::effect_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::VIEW_TYPE_IDENTITY => Some(crate::reactive::view_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        // `para.synced`'s `SyncedSignal` is out-of-`std` — dispatched via its registered ExtType's
        // dyn `ctx_dispatch`, not this compiled-in fast route.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxHost;

    fn host() -> SandboxHost {
        SandboxHost::new()
    }

    /// std's own declarations must pass the registry's assembly sweep — including the
    /// **cross-reference** half, which resolves every field that names something declared elsewhere
    /// (a `traits` entry, a `BoundedVar` bound, a `docs` key, a tier's `config`/`handler`, a derive's
    /// handler, a backed enum's variant constants). Those all fail *silently* at runtime when the
    /// name resolves to nothing, so assembly is where they are caught — and this is the assertion
    /// that the shape std actually ships still assembles under them. The panicking `Registry::new`
    /// runs on every real path; `try_new` is used here so a failure reports the message rather than
    /// aborting the test binary.
    #[test]
    fn the_std_units_assemble() {
        noeta_ext_abi::registry::Registry::try_new(std_units())
            .expect("std's own extension units must pass the registry's assembly sweep");
    }

    #[test]
    fn shell_quote_is_injection_safe() {
        // Safe tokens pass through unquoted; anything with shell metacharacters is single-quoted.
        assert_eq!(shell_quote("plain-1.0_x"), "plain-1.0_x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("has space"), "'has space'");
        // An embedded single quote is closed, escaped, and reopened — the canonical POSIX form.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        // A metacharacter payload becomes one literal token (no word-splitting / command chaining).
        assert_eq!(shell_quote("x; rm -rf / #"), "'x; rm -rf / #'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
    }

    #[test]
    fn required_count_stops_at_the_first_optional_param() {
        // All-required.
        assert_eq!(SigType::required_count(&[SigType::String, SigType::Int]), 2);
        // Trailing optional.
        assert_eq!(
            SigType::required_count(&[SigType::String, SigType::Optional(&SigType::Int)]),
            1
        );
        // Every param optional.
        assert_eq!(
            SigType::required_count(&[SigType::Optional(&SigType::String)]),
            0
        );
        assert_eq!(SigType::required_count(&[]), 0);
    }

    /// `redirect(n)` reaches the request, and a negative count is refused at the call.
    ///
    /// The corpus can pin an error's code and span but not its text, and a runtime abort and a
    /// static rejection would raise E0007 at the same place here — so the sentence a caller
    /// actually reads is asserted where it can be: against the dispatch itself.
    #[test]
    fn a_redirect_limit_is_configured_and_a_negative_one_is_refused() {
        let mut host = crate::SandboxHost::new();
        let mut client: Box<dyn crate::ExternValue> =
            Box::new(crate::http_client::HttpClient::new("https://svc.test"));

        let out = client_method_dispatch(
            client.as_mut(),
            "redirect",
            &mut host,
            &[NativeValue::Scalar(Scalar::Int(3))],
        )
        .expect("a non-negative limit configures the client");
        let NativeOut::Extern(configured) = out else {
            panic!("`redirect` returns a client");
        };
        let configured = configured
            .as_any()
            .downcast_ref::<crate::http_client::HttpClient>()
            .expect("a client");
        assert_eq!(configured.redirect_limit, Some(3));
        assert_eq!(
            configured
                .build("GET", "/a", Vec::new(), Vec::new())
                .redirect_limit,
            Some(3),
            "the limit has to reach the request, which is the only place that reads it"
        );

        let refused = client_method_dispatch(
            client.as_mut(),
            "redirect",
            &mut host,
            &[NativeValue::Scalar(Scalar::Int(-1))],
        )
        .expect_err("a negative hop count is a mistake, not a way to say `unlimited`");
        assert_eq!(
            refused.message,
            "method `redirect` expects a non-negative redirect limit argument"
        );
    }

    #[test]
    fn request_accessors_read_the_inbound_request() {
        let mut req = crate::net::Request {
            conn: Some(0),
            inner: crate::NetRequest {
                method: "POST".to_string(),
                url: "/users/42?active=true".to_string(),
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: b"{}".to_vec(),
                timeout_ms: None,
                redirect_limit: None,
            },
        };
        let call = |req: &mut crate::net::Request, method: &str, args: &[NativeValue]| {
            let ty = find_type(crate::net::REQUEST_TYPE_NAME).unwrap();
            (ty.dispatch)(req, method, &mut SandboxHost::new(), args)
        };
        assert_eq!(
            call(&mut req, "method", &[]),
            Ok(NativeOut::Str("POST".to_string()))
        );
        assert_eq!(
            call(&mut req, "path", &[]),
            Ok(NativeOut::Str("/users/42".to_string()))
        );
        // A present query param, then a missing one.
        assert_eq!(
            call(&mut req, "query", &[NativeValue::Str("active".to_string())]),
            Ok(NativeOut::Some(Box::new(NativeOut::Str(
                "true".to_string()
            ))))
        );
        assert_eq!(
            call(
                &mut req,
                "query",
                &[NativeValue::Str("missing".to_string())]
            ),
            Ok(NativeOut::None)
        );
        // Header lookup is case-insensitive.
        assert_eq!(
            call(
                &mut req,
                "header",
                &[NativeValue::Str("content-type".to_string())]
            ),
            Ok(NativeOut::Some(Box::new(NativeOut::Str(
                "application/json".to_string()
            ))))
        );
        assert_eq!(
            call(&mut req, "body", &[]),
            Ok(NativeOut::Str("{}".to_string()))
        );
    }

    #[test]
    fn response_builder_and_copy_modify() {
        let mut h = host();
        // Status + body + headers.
        let built = dispatch(
            "http.server",
            "response",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(201)),
                NativeValue::Str("ok".to_string()),
                NativeValue::Map(vec![("x-a".to_string(), NativeValue::Str("1".to_string()))]),
            ],
        )
        .unwrap();
        let NativeOut::Extern(boxed) = &built else {
            panic!("response builds an extern value");
        };
        let resp = boxed
            .as_any()
            .downcast_ref::<crate::NetResponse>()
            .expect("a Response");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"ok");
        assert_eq!(resp.header_value("x-a"), Some("1"));

        // An out-of-range status is rejected.
        assert!(
            dispatch(
                "http.server",
                "response",
                &mut h,
                &[NativeValue::Scalar(Scalar::Int(700))],
            )
            .is_err()
        );
    }

    #[test]
    fn math_dispatches_through_the_registry() {
        let mut h = host();
        let out = dispatch(
            "math",
            "sqrt",
            &mut h,
            &[NativeValue::Scalar(Scalar::Float(4.0))],
        );
        assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Float(2.0))));
    }

    #[test]
    fn math_floor_returns_an_int() {
        let mut h = host();
        let out = dispatch(
            "math",
            "floor",
            &mut h,
            &[NativeValue::Scalar(Scalar::Float(3.7))],
        );
        assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Int(3))));
    }

    #[test]
    fn random_is_seeded_and_deterministic() {
        let mut h = host();
        dispatch(
            "random",
            "seed",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(42))],
        )
        .unwrap();
        let a = dispatch(
            "random",
            "int",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(1)),
                NativeValue::Scalar(Scalar::Int(6)),
            ],
        );
        // Re-seed and draw again — identical.
        dispatch(
            "random",
            "seed",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(42))],
        )
        .unwrap();
        let b = dispatch(
            "random",
            "int",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(1)),
                NativeValue::Scalar(Scalar::Int(6)),
            ],
        );
        assert_eq!(a, b);
        assert!(matches!(a, Ok(NativeOut::Scalar(Scalar::Int(n))) if (1..=6).contains(&n)));
    }

    #[test]
    fn env_get_reads_the_sandbox_fixture() {
        let mut h = host();
        let out = dispatch(
            "env",
            "get",
            &mut h,
            &[NativeValue::Str("HOME".to_string())],
        );
        assert_eq!(
            out,
            Ok(NativeOut::Some(Box::new(NativeOut::Str(
                "/home/sandbox".to_string()
            ))))
        );
    }

    #[test]
    fn env_get_is_none_when_unset() {
        // An absent variable is a value, not an abort — the whole point of the `?string` return.
        let mut h = host();
        let out = dispatch(
            "env",
            "get",
            &mut h,
            &[NativeValue::Str("DOES_NOT_EXIST".to_string())],
        );
        assert_eq!(out, Ok(NativeOut::None));
    }

    #[test]
    fn env_keys_is_a_sorted_string_list() {
        let mut h = host();
        let out = dispatch("env", "keys", &mut h, &[]);
        assert_eq!(
            out,
            Ok(NativeOut::List(vec![
                NativeOut::Str("HOME".to_string()),
                NativeOut::Str("USER".to_string()),
            ]))
        );
    }

    #[test]
    fn arity_misuse_is_an_error() {
        let mut h = host();
        let out = dispatch(
            "time",
            "monotonic",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(1))],
        );
        assert!(matches!(out, Err(e) if e.kind == crate::ErrorKind::Arity));
    }

    #[test]
    fn id_module_is_registry_backed_and_sandbox_deterministic() {
        // `next_id` reads the host's counter: 1, 2, 3 — one dispatch shared by both backends.
        let mut h = host();
        for want in 1..=3 {
            let out = dispatch("id", "next_id", &mut h, &[]);
            assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Int(want))));
        }
        // UUIDs draw from the sandbox entropy/wall-time streams, so a fresh sandbox reproduces
        // them exactly (what lets conformance pin exact values) — and consecutive draws differ.
        let a = dispatch("id", "uuid", &mut h, &[]).unwrap();
        let b = dispatch("id", "uuid", &mut h, &[]).unwrap();
        assert_ne!(a, b);
        let mut fresh = host();
        assert_eq!(dispatch("id", "uuid", &mut fresh, &[]), Ok(a));
        // v7: an extern `Uuid` value (extern-types X2) — version nibble 7, the sandbox epoch in
        // the leading 48 bits.
        let Ok(NativeOut::Extern(v7)) = dispatch("id", "uuid_v7", &mut h, &[]) else {
            panic!("uuid_v7 should produce a Uuid");
        };
        let v7 = v7.display_string();
        assert_eq!(&v7[14..15], "7");
        let ms = u64::from_str_radix(&v7[..13].replace('-', ""), 16).unwrap();
        assert_eq!(ms, crate::host::SANDBOX_EPOCH_MS);
        // `id` is an ordinary registry module (the virtual table itself died at H5).
        assert!(find_function("id", "uuid_v7").is_some());
        // The `Uuid` extern type is registered with its method table, and `parse` round-trips
        // (`none` on malformed input).
        assert!(find_type("Uuid").is_some_and(|t| t.key_capable));
        assert!(find_type_method("Uuid", "timestamp_ms").is_some());
        let parsed = dispatch("id", "parse", &mut h, &[NativeValue::Str(v7.clone())]).unwrap();
        let NativeOut::Some(inner) = parsed else {
            panic!("parse of a canonical uuid should be some");
        };
        let NativeOut::Extern(u) = *inner else {
            panic!("parse should yield a Uuid");
        };
        assert_eq!(u.display_string(), v7);
        assert_eq!(
            dispatch("id", "parse", &mut h, &[NativeValue::Str("nope".into())]),
            Ok(NativeOut::None)
        );
    }

    #[test]
    fn every_extern_type_carries_a_namespace_and_qualified_identity() {
        // Each registered type has a `std.<unit>` namespace; its qualified identity is
        // `namespace.name`, and `find_type_qualified` recovers it. This is the identity the checker
        // and runtime will key on so a native `Counter` can coexist with a user's own.
        let expected = [
            ("Uuid", "std.id.Uuid"),
            ("Hasher", "std.crypto.Hasher"),
            #[cfg(feature = "ring-regex")]
            ("Pattern", "std.regex.Pattern"),
            #[cfg(feature = "ring-regex")]
            ("Match", "std.regex.Match"),
            ("Response", "std.http.Response"),
            ("Request", "std.http.Request"),
            ("FileHandle", "std.fs.FileHandle"),
            ("ExecResult", "std.os.ExecResult"),
            ("Process", "std.os.Process"),
            ("Span", "std.tracing.Span"),
            ("Counter", "std.metrics.Counter"),
            ("Histogram", "std.metrics.Histogram"),
            ("Gauge", "std.metrics.Gauge"),
            ("Cell", "std.cell.Cell"),
            ("Signal", "std.reactive.Signal"),
            ("Computed", "std.reactive.Computed"),
            ("Effect", "std.reactive.Effect"),
            ("View", "std.reactive.View"),
            // The CRDT/synced types (`GCounter`/`PnCounter`/`GSet`/`SyncedSignal`) left `std` for the
            // `para` namespace (the out-of-tree para-p2p package); its own repo's tests cover them.
        ];
        for (short, qualified) in expected {
            let t = find_type(short).expect("registered type");
            assert_eq!(t.qualified(), qualified, "qualified identity of `{short}`");
            assert!(
                std::ptr::eq(find_type_qualified(qualified).unwrap(), t),
                "find_type_qualified round-trips `{qualified}`"
            );
        }
        // No type was left on the bare `std` default.
        for t in extensions().iter().flat_map(|e| e.types()) {
            assert!(
                t.namespace.contains('.'),
                "`{}` must declare a `std.<unit>` namespace, got `{}`",
                t.name,
                t.namespace
            );
        }
    }

    #[test]
    fn signatures_are_queryable() {
        assert_eq!(
            find_function("math", "pow").map(|f| f.params.len()),
            Some(2)
        );
        assert!(matches!(
            find_function("env", "keys").map(|f| f.ret),
            Some(Concrete(SigType::List(_)))
        ));
        assert!(find_function("math", "nope").is_none());
        // `vec.add` is registered (a scalar op) and returns the same shape as its first argument;
        // the bulk `vec.add_all` kernel is *not* registered (it stays per-backend).
        assert!(matches!(
            find_function("vec", "add").map(|f| f.ret),
            Some(SameAsArg(0))
        ));
        assert!(find_function("vec", "add_all").is_none());
        // `json` is registered (B4): dynamic `parse` + `stringify` dispatch through the registry.
        assert!(matches!(
            find_function("json", "parse").map(|f| f.ret),
            Some(Concrete(SigType::Dyn))
        ));
        assert!(find_module("json").is_some_and(|m| m.deep_marshal));
    }

    #[test]
    fn qualified_lookup_resolves_under_the_std_root() {
        // `std` is a registered extension root; nothing else is (until the manifest populates it).
        assert!(is_extension_root("std"));
        assert!(!is_extension_root("guzzle"));
        // A fully-qualified path resolves to the same module the bare name does.
        assert!(std::ptr::eq(
            find_module_qualified(&["std".into(), "math".into()]).unwrap(),
            find_module("math").unwrap(),
        ));
        // The root must match, the remainder must be non-empty, and a bare root names no module.
        assert!(find_module_qualified(&["guzzle".into(), "math".into()]).is_none());
        assert!(find_module_qualified(&["std".into()]).is_none());
    }
}
