//! Streaming HTTP bodies (http-streaming arc) — the seam types behind `std.http.client.stream`
//! and `std.http.server.sse`.
//!
//! The read side answers "consume a response body incrementally, cut into frames"; the write side
//! answers its exact inverse, "serve an event stream". Both are ordinary HTTP with no domain
//! specificity — an LLM token stream, a build-status feed, and a log tail are the same two
//! mechanisms — so they live in std rather than in a package.
//!
//! **One [`Framing`] enum, not an SSE reader with NDJSON bolted on.** OpenAI-compatible endpoints
//! speak SSE; Ollama's native `/api/chat` speaks newline-delimited JSON; a log tail is raw lines.
//! All three are "read the body incrementally and cut it into frames" and differ only in the cut,
//! so the cut is a parameter ([`FrameDecoder`]) rather than three parallel readers.
//!
//! The [`Frame`] a decoder yields is a **value** type, which is what lets a consuming pipeline be
//! channel-based: it is plain owned data, so it is `Send` and crosses a channel or an isolate.
//! (A `class`/`dyn` is `!Send` and could not.)
//!
//! Only the *decoding* lives here — dependency-free, so both the deterministic sandbox and the
//! real reqwest-backed host cut bytes into frames with the identical parser. The sandbox's scripted
//! stream bodies live in `noeta-stdlib`'s `net` module, beside the request script they mirror.

use crate::extern_value::ExternValue;
use std::any::Any;
use std::cmp::Ordering;

// --------------------------------------------------------------------------- the framing choice

/// The registered extern-enum name of a body framing.
pub const FRAMING_TYPE_NAME: &str = "Framing";

/// `Framing`'s qualified runtime identity — the [`crate::net::RESPONSE_TYPE_IDENTITY`] twin.
pub const FRAMING_TYPE_IDENTITY: &str = "std.http.Framing";

/// How a response body is cut into [`Frame`]s.
///
/// The three cuts that cover the deployed world. They are deliberately one enum and not three
/// entry points: a caller switching an LLM client between an OpenAI-compatible endpoint and a
/// native Ollama one changes this argument and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Framing {
    /// `text/event-stream` — W3C/WHATWG server-sent events. The default because it is what the
    /// overwhelming majority of streaming HTTP APIs speak.
    #[default]
    Sse,
    /// Newline-delimited JSON: one JSON document per line, blank lines skipped. The frame's
    /// [`Frame::data`] is the line verbatim — **not** parsed, so the caller chooses its own
    /// decoding (`json.parse`, a typed `json::<T>()`), and a malformed line is the caller's error
    /// to report rather than one the reader swallows.
    Ndjson,
    /// One raw line per frame, blank lines **kept** (a blank line is content in a log tail, and is
    /// not a JSON document — that is the whole difference from [`Framing::Ndjson`]).
    Lines,
}

/// The variant names as the language spells them — the single source the registration, the
/// `FromStr`, and the `Display` all read, so they cannot drift apart.
pub const FRAMING_VARIANTS: &[(&str, Framing)] = &[
    ("Sse", Framing::Sse),
    ("Ndjson", Framing::Ndjson),
    ("Lines", Framing::Lines),
];

impl Framing {
    /// The variant's language-facing name (`Framing.Sse` → `"Sse"`).
    pub fn label(self) -> &'static str {
        // Derived from the one table rather than a second `match`, so adding a framing cannot
        // leave a stale name behind.
        FRAMING_VARIANTS
            .iter()
            .find(|(_, f)| *f == self)
            .map(|(name, _)| *name)
            .expect("every Framing is in FRAMING_VARIANTS")
    }
}

impl std::fmt::Display for Framing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for Framing {
    type Err = ();
    fn from_str(s: &str) -> Result<Framing, ()> {
        FRAMING_VARIANTS
            .iter()
            .find(|(name, _)| *name == s)
            .map(|(_, f)| *f)
            .ok_or(())
    }
}

// ---------------------------------------------------------------------------------- the frame

/// The registered extern-struct name of one decoded frame.
pub const FRAME_TYPE_NAME: &str = "Frame";

/// `Frame`'s qualified runtime identity.
pub const FRAME_TYPE_IDENTITY: &str = "std.http.Frame";

/// `Frame`'s field names, in declaration order — the single source the registration, the
/// construction, and the read-back all share.
pub const FRAME_FIELDS: [&str; 4] = ["event", "data", "id", "retry"];

/// One frame cut out of a streaming body — a **value** struct, not a handle.
///
/// Value-ness is load-bearing rather than incidental: a frame is `Send`, so a consuming pipeline
/// can push it down a channel or hand it to another isolate. That is exactly what an LLM client
/// re-emitting provider tokens to a browser needs, and what a `class`/`dyn` (both `!Send`) could
/// not provide.
///
/// What the fields carry depends on the [`Framing`]:
/// - [`Framing::Sse`] — the parsed SSE fields. `event` is empty when the frame names none
///   (deliberately **not** defaulted to `"message"` the way a browser's `EventSource` does: an
///   empty string is an honest "the server said nothing", and a caller that wants the browser
///   default can spell it). `id` carries the stream's last-seen event id, which persists across
///   frames per the spec. `retry` is set only on a frame whose own block carried a `retry:` field.
/// - [`Framing::Ndjson`] — one JSON document per line, verbatim in `data`; `event`/`id` empty.
/// - [`Framing::Lines`] — one raw line in `data`; `event`/`id` empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// The SSE `event:` field — the frame's name. Empty when the framing has no notion of one, or
    /// when an SSE frame names none.
    pub event: String,
    /// The frame's payload: the joined SSE `data:` lines, one NDJSON document, or one raw line.
    pub data: String,
    /// The SSE `id:` field (the stream's last-seen event id, which persists across frames). Empty
    /// for the line-oriented framings.
    pub id: String,
    /// The SSE `retry:` reconnection hint in **milliseconds**, when the frame's block carried one.
    pub retry: Option<i64>,
}

impl Frame {
    /// A frame carrying only `data` — what both line-oriented framings produce, and the shape an
    /// `sse` sink most often sends.
    pub fn data(data: impl Into<String>) -> Frame {
        Frame {
            data: data.into(),
            ..Frame::default()
        }
    }

    /// A named frame (`event:` + `data:`).
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Frame {
        Frame {
            event: event.into(),
            data: data.into(),
            ..Frame::default()
        }
    }

    /// Encode this frame as `text/event-stream` wire bytes, terminated by the blank line that
    /// dispatches it. The exact inverse of what [`FrameDecoder`] parses, so a `sse` response read
    /// back by `stream(..., Framing.Sse)` round-trips.
    ///
    /// A multi-line `data` is emitted as one `data:` line per line, which is the only legal way to
    /// carry a newline through SSE — a raw `\n` inside a field value would terminate the field and
    /// silently split one frame into two.
    pub fn to_sse_wire(&self) -> String {
        let mut out = String::new();
        if !self.event.is_empty() {
            out.push_str("event: ");
            out.push_str(&self.event);
            out.push('\n');
        }
        if !self.id.is_empty() {
            out.push_str("id: ");
            out.push_str(&self.id);
            out.push('\n');
        }
        if let Some(retry) = self.retry {
            out.push_str("retry: ");
            out.push_str(&retry.to_string());
            out.push('\n');
        }
        // An empty payload still needs a `data:` line: a block with no data field dispatches
        // nothing at all on the receiving side, so the frame would vanish in transit.
        for line in split_data_lines(&self.data) {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// The `data:` lines a payload is emitted as: its `\n`-separated pieces, and a single empty line
/// for an empty payload (so the frame carries a `data:` field and therefore dispatches).
fn split_data_lines(data: &str) -> impl Iterator<Item = &str> {
    // `split('\n')` on "" yields exactly one empty piece, which is the wanted `data:` line.
    data.split('\n')
}

/// Encode an SSE **comment** — the `: keepalive` heartbeat. A comment carries no data and so
/// dispatches no event; its purpose is to put bytes on the wire so an idle connection is not
/// reaped by an intermediary.
///
/// Embedded newlines are each re-prefixed, because a bare `\n` would end the comment and let the
/// remainder be parsed as fields — a comment must never be able to inject a frame.
pub fn sse_comment_wire(text: &str) -> String {
    let mut out = String::new();
    for line in text.split('\n') {
        out.push(':');
        // A single leading space after the colon is stripped by every reader, so emitting one
        // keeps `comment("x")` and the conventional `: x` spelling identical on the wire.
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The `content-type` an `sse` response is served with.
pub const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// The headers an `sse` response carries. `cache-control: no-cache` stops an intermediary
/// answering a later request from a cached prefix of the stream, and `connection: keep-alive` is
/// the HTTP/1.1 spelling of "do not close this after one response".
pub const SSE_HEADERS: [(&str, &str); 3] = [
    ("content-type", SSE_CONTENT_TYPE),
    ("cache-control", "no-cache"),
    ("connection", "keep-alive"),
];

// -------------------------------------------------------------------------------- the decoder

/// The incremental body decoder: push bytes in with [`FrameDecoder::feed`], pull whole frames out
/// with [`FrameDecoder::next_frame`], and settle the tail with [`FrameDecoder::finish`].
///
/// Incremental by construction, because a chunk boundary lands wherever the network puts it —
/// mid-line, mid-frame, and (the corner that breaks naive readers) **between the `\r` and the
/// `\n` of one CRLF**. Every one of those is a partial read that must produce nothing until the
/// rest arrives, which is why this is a state machine and not `body.split("\n\n")`.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    framing: Framing,
    /// Bytes received and not yet consumed into a frame.
    buffer: String,
    /// Frames decoded and not yet taken.
    ready: std::collections::VecDeque<Frame>,
    /// The in-progress SSE event's `data:` lines, joined with `\n` at dispatch.
    data: String,
    /// Whether the in-progress SSE block carried any `data:` field at all — the spec's
    /// "data buffer is empty" test, which a `data:` with an empty value must still pass.
    saw_data: bool,
    /// The in-progress SSE event's `event:` field.
    event: String,
    /// The stream's **last event id**, which persists across frames per the SSE spec (it is what a
    /// reconnecting client replays with), unlike `data`/`event` which reset at every dispatch.
    last_id: String,
    /// The in-progress SSE block's `retry:` field.
    retry: Option<i64>,
    /// Whether the leading UTF-8 BOM has been considered. The spec strips one BOM at the very
    /// start of the stream and only there.
    bom_checked: bool,
    /// Whether [`FrameDecoder::finish`] has run — a decoder yields nothing new afterwards.
    finished: bool,
}

impl FrameDecoder {
    /// A decoder cutting with `framing`.
    pub fn new(framing: Framing) -> FrameDecoder {
        FrameDecoder {
            framing,
            ..FrameDecoder::default()
        }
    }

    /// Which cut this decoder applies.
    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// Feed the next chunk of body bytes.
    ///
    /// Invalid UTF-8 is replaced rather than fatal: a body is decoded lossily so one bad byte
    /// costs a character and not the rest of the stream. (A chunk boundary splitting a multi-byte
    /// character is handled by [`FrameDecoder::feed_str`]'s caller contract — see
    /// [`Utf8Chunker`], which the byte-fed hosts use.)
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_str(&String::from_utf8_lossy(bytes));
    }

    /// Feed the next chunk as already-decoded text.
    pub fn feed_str(&mut self, text: &str) {
        if self.finished {
            return;
        }
        self.buffer.push_str(text);
        if !self.bom_checked {
            // Only meaningful once there is at least one character; an empty first chunk must not
            // spend the one chance to see the BOM.
            if !self.buffer.is_empty() {
                self.bom_checked = true;
                if let Some(rest) = self.buffer.strip_prefix('\u{feff}') {
                    self.buffer = rest.to_string();
                }
            }
        }
        self.drain_lines(false);
    }

    /// Signal end of body: no more bytes will arrive.
    ///
    /// What a trailing partial means differs by framing, and the difference is the spec's, not a
    /// choice. A line-oriented body's final unterminated line **is** a line (the `str::lines`
    /// rule), so it is emitted. An SSE block that never received its terminating blank line is
    /// **discarded** — a frame dispatches on the blank line, so an interrupted body has not
    /// delivered one, and inventing it would hand the caller a truncated payload as if it were
    /// complete. That is exactly what a truncated stream must not do.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.drain_lines(true);
        self.finished = true;
        // Whatever survives is an undispatched partial: drop it (see above).
        self.buffer.clear();
        self.reset_event();
    }

    /// Take the next decoded frame, if one is ready.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.ready.pop_front()
    }

    /// How many frames are waiting to be taken.
    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Whether the body has ended and every decoded frame has been taken.
    pub fn is_done(&self) -> bool {
        self.finished && self.ready.is_empty()
    }

    /// Consume every complete line currently in the buffer. When `at_end`, the trailing partial
    /// line counts as a line too (for the line-oriented framings).
    fn drain_lines(&mut self, at_end: bool) {
        while let Some(line) = self.take_line(at_end) {
            match self.framing {
                Framing::Sse => self.sse_line(&line),
                // A blank line is not a JSON document, so NDJSON skips it; `Lines` keeps it,
                // because a blank line is content in a log tail.
                Framing::Ndjson => {
                    if !line.is_empty() {
                        self.ready.push_back(Frame::data(line));
                    }
                }
                Framing::Lines => self.ready.push_back(Frame::data(line)),
            }
        }
    }

    /// Take one line off the buffer, consuming its terminator.
    ///
    /// SSE recognizes three terminators — `\r\n`, `\n`, and a lone `\r` — and the last two make
    /// the split-CRLF case real: a buffer ending in `\r` may be a complete lone-`\r` line **or**
    /// the first half of a CRLF whose `\n` is in the next chunk. Emitting it immediately would
    /// turn one line ending into two and dispatch a phantom empty line, so a trailing `\r` waits
    /// for more input unless the body has ended.
    fn take_line(&mut self, at_end: bool) -> Option<String> {
        let Some(pos) = self.buffer.find(['\n', '\r']) else {
            // No terminator: only a final unterminated line, at end of body, is a line.
            if at_end && !self.buffer.is_empty() {
                return Some(std::mem::take(&mut self.buffer));
            }
            return None;
        };
        let is_cr = self.buffer.as_bytes()[pos] == b'\r';
        let after_cr = pos + 1;
        if is_cr && after_cr == self.buffer.len() && !at_end {
            // The ambiguous trailing `\r` — wait for the byte that disambiguates it.
            return None;
        }
        let line = self.buffer[..pos].to_string();
        // A `\r` immediately followed by `\n` is ONE terminator.
        let consumed = match is_cr && self.buffer[after_cr..].starts_with('\n') {
            true => after_cr + 1,
            false => after_cr,
        };
        self.buffer.replace_range(..consumed, "");
        Some(line)
    }

    /// Process one SSE line per the WHATWG event-stream parse rules.
    fn sse_line(&mut self, line: &str) {
        // A blank line dispatches the block.
        if line.is_empty() {
            self.dispatch();
            return;
        }
        // A `:`-prefixed line is a comment — ignored entirely (the keepalive heartbeat).
        if line.starts_with(':') {
            return;
        }
        let (field, raw) = match line.split_once(':') {
            Some((field, rest)) => (field, rest),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };
        // Exactly ONE leading space is stripped — `data:  x` legitimately carries " x".
        let value = raw.strip_prefix(' ').unwrap_or(raw);
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.saw_data {
                    // Multi-line `data:` fields concatenate with `\n`.
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.saw_data = true;
            }
            // An id containing a NUL is ignored per spec rather than truncated.
            "id" => {
                if !value.contains('\0') {
                    self.last_id = value.to_string();
                }
            }
            // `retry:` is milliseconds, and only if the value is all ASCII digits; anything else
            // is ignored rather than defaulted, so a malformed hint cannot become a real one.
            // (`parse` alone would accept `+5` and `-5`, which the spec's digit rule excludes.)
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                self.retry = value.parse::<i64>().ok();
            }
            // Any other field name is ignored, per the spec's forgiving-reader rule.
            _ => {}
        }
    }

    /// Dispatch the in-progress SSE block, if it has data.
    ///
    /// A block with **no** `data:` field dispatches nothing — that is the spec, and it is what
    /// makes a lone `retry: 3000` block or a run of comments produce no frame. A `data:` with an
    /// empty value is not that case: it carries a field, so it dispatches a frame with an empty
    /// payload.
    fn dispatch(&mut self) {
        if !self.saw_data {
            self.reset_event();
            return;
        }
        self.ready.push_back(Frame {
            event: std::mem::take(&mut self.event),
            data: std::mem::take(&mut self.data),
            id: self.last_id.clone(),
            retry: self.retry,
        });
        self.reset_event();
    }

    /// Clear the per-block state. `last_id` deliberately survives: it is the stream's last event
    /// id, which the spec keeps across frames.
    fn reset_event(&mut self) {
        self.data.clear();
        self.saw_data = false;
        self.event.clear();
        self.retry = None;
    }
}

/// A byte-to-text adapter for hosts that receive the body as arbitrary chunks.
///
/// [`FrameDecoder::feed`] decodes lossily, which is right for a genuinely invalid byte and wrong
/// for a **valid** multi-byte character split across two chunks — that would replace a legal
/// character with `U+FFFD` on every chunk boundary that happens to fall inside one. This holds the
/// trailing incomplete sequence back until its continuation bytes arrive.
#[derive(Debug, Default)]
pub struct Utf8Chunker {
    partial: Vec<u8>,
}

impl Utf8Chunker {
    /// A fresh chunker.
    pub fn new() -> Utf8Chunker {
        Utf8Chunker::default()
    }

    /// Decode `bytes`, holding back a trailing incomplete UTF-8 sequence for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.partial.extend_from_slice(bytes);
        let taken = std::mem::take(&mut self.partial);
        let mut out = String::new();
        let mut at = 0usize;
        // Loop rather than handle one error: a chunk may contain SEVERAL invalid sequences, and
        // returning after the first would hold every byte behind it back until the next push —
        // which for a stream that never gets another chunk means losing them entirely.
        loop {
            let rest = &taken[at..];
            let Err(error) = std::str::from_utf8(rest) else {
                out.push_str(&String::from_utf8_lossy(rest));
                return out;
            };
            let good = error.valid_up_to();
            out.push_str(&String::from_utf8_lossy(&rest[..good]));
            match error.error_len() {
                // A genuinely invalid sequence: consume it lossily and keep scanning.
                Some(bad) => {
                    out.push('\u{fffd}');
                    at += good + bad;
                }
                // An incomplete tail: hold exactly that back for the next chunk.
                None => {
                    self.partial = rest[good..].to_vec();
                    return out;
                }
            }
        }
    }

    /// Flush whatever incomplete tail remains at end of body, lossily.
    pub fn finish(&mut self) -> String {
        let taken = std::mem::take(&mut self.partial);
        match taken.is_empty() {
            true => String::new(),
            false => String::from_utf8_lossy(&taken).into_owned(),
        }
    }
}

// ------------------------------------------------------------------- the language-facing handles

/// The registered extern-type name of an incremental body reader.
pub const FRAME_STREAM_TYPE_NAME: &str = "FrameStream";

/// `FrameStream`'s qualified runtime identity.
pub const FRAME_STREAM_TYPE_IDENTITY: &str = "std.http.FrameStream";

/// What opening a stream produced: the host-side id **and the response head that came back with
/// it** — the [`crate::NetResponse`] minus its body, which is the one thing a streamed response
/// cannot hand over whole.
///
/// The head is returned rather than queried later because it is produced exactly once, at the
/// opening handshake, and is immutable afterwards. A separate `net_stream_status(id)` accessor was
/// the alternative and is worse in the specific way this type exists to prevent: it can be
/// *forgotten*. A host that implements streaming but not the accessor would answer every request
/// with a plausible default, and a silently-wrong `200` for a real `429` is the same class of
/// invisible failure as discarding the status altogether. Returning it here makes a status
/// structurally unavoidable — a host cannot open a stream without saying what the server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamHead {
    /// The host-side stream id, passed to [`crate::host::Network::net_stream_recv`] /
    /// [`crate::host::Network::net_stream_close`].
    pub stream: u64,
    /// The response status. A non-2xx is **not** an error here, exactly as it is not one for
    /// [`crate::NetResponse`]: the request reached the server and was answered.
    pub status: u16,
    /// The response headers, in the order the server sent them.
    pub headers: Vec<(String, String)>,
    /// The URL the response came from — the *final* one after any redirects, like
    /// [`crate::NetResponse::url`].
    pub url: String,
}

impl StreamHead {
    /// A `200 OK` head for `stream` against `url`, carrying no headers — the shape a host with no
    /// head information to report would produce.
    pub fn ok(stream: u64, url: impl Into<String>) -> StreamHead {
        StreamHead {
            stream,
            status: 200,
            headers: Vec::new(),
            url: url.into(),
        }
    }
}

/// An open response body being read incrementally — a host-resource id, exactly like
/// [`crate::net::Request`]'s `conn` and the serve loop's `Socket`, plus the response head the
/// opening handshake received.
///
/// **Why the head rides along.** A streamed response has the same two halves as a buffered one, a
/// head and a body, but only the body arrives incrementally. Discarding the head made a streamed
/// `429` indistinguishable from a model with nothing to say: the vendor answers a rate limit with a
/// bare JSON document, which the SSE decoder correctly cuts into **zero** frames (it is not an
/// event stream), so the caller saw an empty stream and no way to learn why. Carrying `status` and
/// `headers` on the handle means the answer is readable *before* the first `recv()` — an API where
/// you must consume a frame to discover the request failed would be a quieter version of the same
/// bug — and it survives `close()`, because a head is a fact about the response, not a live
/// resource.
///
/// **Reference semantics**: a copy aliases the same underlying body, because a body is a single
/// consumable resource and two independent cursors over one connection do not exist. The head is
/// copied with it, which changes nothing — two handles to one stream saw one handshake. Not
/// key-capable (it identifies a host resource, not a value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStream {
    /// The host-side stream id.
    pub stream: u64,
    /// The response status the opening handshake received.
    pub status: u16,
    /// The response headers, in the order the server sent them.
    pub headers: Vec<(String, String)>,
    /// The URL the response came from.
    pub url: String,
}

impl FrameStream {
    /// The reader for an opened [`StreamHead`].
    pub fn new(head: StreamHead) -> FrameStream {
        FrameStream {
            stream: head.stream,
            status: head.status,
            headers: head.headers,
            url: head.url,
        }
    }

    /// Whether the response status is a 2xx — the [`crate::NetResponse::ok`] twin.
    pub fn is_ok(&self) -> bool {
        (200..=299).contains(&self.status)
    }

    /// The first value of header `name`, matched case-insensitively — the
    /// [`crate::NetResponse::header_value`] twin.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl ExternValue for FrameStream {
    fn type_identity(&self) -> &'static str {
        FRAME_STREAM_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<FrameStream>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable (identifies a host resource)
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<frame stream {}>", self.stream)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The registered extern-type name of a server-sent-events sink.
pub const SSE_SINK_TYPE_NAME: &str = "SseSink";

/// `SseSink`'s qualified runtime identity.
pub const SSE_SINK_TYPE_IDENTITY: &str = "std.http.SseSink";

/// The write half: an accepted connection switched to `text/event-stream`, held open while the
/// handler pushes frames. The write-side twin of the serve loop's `Socket`, and a plain conn id
/// for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseSink {
    /// The connection being streamed to.
    pub conn: u64,
}

impl ExternValue for SseSink {
    fn type_identity(&self) -> &'static str {
        SSE_SINK_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<SseSink>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable (identifies a host resource)
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<sse sink {}>", self.conn)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ------------------------------------------------------------------------------ the async leaves

/// Materialize a recv outcome as the language-facing `?Frame` — the [`crate::net::ws_recv_outcome`]
/// twin. `None` means the body ended.
pub fn frame_recv_outcome(next: Option<Frame>) -> crate::NativeOut {
    match next {
        Some(frame) => crate::NativeOut::Some(Box::new(frame_out(frame))),
        None => crate::NativeOut::None,
    }
}

/// Marshal a [`Frame`] as the native **value struct** the language sees.
///
/// A [`crate::NativeOut::Instance`] of [`crate::FieldedKind::Struct`] — a struct-kind shape, which
/// is what gives the value structural equality, copy-on-assign, and (the load-bearing part) a
/// language-level `Send` derived from its all-`Send` fields. An extern box would be an opaque
/// handle instead, and a class-kind instance would be `!Send` outright.
///
/// Fields are emitted in the type's **declared slot order** ([`FRAME_FIELDS`]).
pub fn frame_out(frame: Frame) -> crate::NativeOut {
    crate::NativeOut::Instance {
        class: FRAME_TYPE_NAME.to_string(),
        fields: vec![
            (
                FRAME_FIELDS[0].to_string(),
                crate::NativeOut::Str(frame.event),
            ),
            (
                FRAME_FIELDS[1].to_string(),
                crate::NativeOut::Str(frame.data),
            ),
            (FRAME_FIELDS[2].to_string(), crate::NativeOut::Str(frame.id)),
            (
                FRAME_FIELDS[3].to_string(),
                match frame.retry {
                    Some(ms) => crate::NativeOut::Some(Box::new(crate::NativeOut::Scalar(
                        crate::Scalar::Int(ms),
                    ))),
                    None => crate::NativeOut::None,
                },
            ),
        ],
        kind: crate::FieldedKind::Struct,
    }
}

/// Read a [`Frame`] back out of the argument view — the inverse of [`frame_out`], for
/// `SseSink.send(frame)`.
///
/// **Both projections are accepted**, and that is not defensive padding. A value crosses the seam
/// through one of two views: the *shallow* one, which carries a registered native struct as
/// [`crate::NativeValue::Instance`], and the *deep* (JSON-shaped) one, which flattens any object to
/// [`crate::NativeValue::Map`] of its fields in declared order. Which one a given call site uses is
/// a property of the caller (`ctx.view` and a `deep_marshal` module both take the deep view), not of
/// this type — so reading only one of them makes the seam silently sensitive to a decision made
/// elsewhere. Both backends produce the identical shape either way, so the differential is
/// unaffected.
///
/// A missing or mistyped field degrades to the type's default rather than failing: the checker has
/// already proven the argument is a `Frame`, so a surprise here is an ABI-shape bug and not user
/// input, and a sink that sends an empty `data:` is far easier to diagnose than one that aborts a
/// live stream.
pub fn frame_from_value(value: &crate::NativeValue) -> Option<Frame> {
    let fields = match value {
        crate::NativeValue::Instance { fields, .. } => fields,
        crate::NativeValue::Map(fields) => fields,
        _ => return None,
    };
    let text = |name: &str| -> String {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| match v {
                crate::NativeValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let retry = fields
        .iter()
        .find(|(k, _)| k == FRAME_FIELDS[3])
        .and_then(|(_, v)| optional_int(v));
    Some(Frame {
        event: text(FRAME_FIELDS[0]),
        data: text(FRAME_FIELDS[1]),
        id: text(FRAME_FIELDS[2]),
        retry,
    })
}

/// Read an `?int` field out of the argument view.
///
/// Accepts both shapes an optional arrives in — the bare payload (the deep view marshals an
/// `Option` *through* its payload, so `some(1500)` is just `1500` and `none` is a unit) and a real
/// `Option` variant (the shallow view). Anything else, unit included, is absent.
fn optional_int(value: &crate::NativeValue) -> Option<i64> {
    match value {
        crate::NativeValue::Scalar(crate::Scalar::Int(ms)) => Some(*ms),
        crate::NativeValue::Variant {
            variant, fields, ..
        } if variant == "Some" => match fields.first() {
            Some(crate::NativeValue::Scalar(crate::Scalar::Int(ms))) => Some(*ms),
            _ => None,
        },
        _ => None,
    }
}

/// The default incremental-read descriptor: it pulls the next frame synchronously through the Host
/// **at spawn**. The sandbox uses this (its scripted body is decoded up front, so a recv is a pop —
/// deterministic and resolved at spawn); `RealHost` overrides
/// [`crate::host::Network::net_stream_recv`] with a genuinely async body read. The same "serial
/// degradation for free" [`crate::net::NetFetchIo`] and the ws family rely on.
#[derive(Debug)]
pub struct StreamRecvIo {
    /// The stream to take the next frame from.
    pub stream: u64,
}

impl crate::ExternIo for StreamRecvIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        Ok(frame_recv_outcome(host.net_stream_recv_next(self.stream)?))
    }
}

/// The default SSE **start** descriptor: writes the `text/event-stream` response head through the
/// Host at spawn. The [`crate::net::WsUpgradeIo`] twin — one leaf that switches the connection out
/// of one-reply-and-close.
#[derive(Debug)]
pub struct SseStartIo {
    /// The connection switching to an event stream.
    pub conn: u64,
}

impl crate::ExternIo for SseStartIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        host.net_sse_start_now(self.conn)?;
        Ok(crate::NativeOut::Unit)
    }
}

/// The default SSE **send** descriptor: writes one frame's wire bytes through the Host at spawn.
/// One-shot — the frame is moved out on the single run.
#[derive(Debug)]
pub struct SseSendIo {
    /// The connection to write on.
    pub conn: u64,
    /// The already-encoded wire bytes — `Some` until the one run consumes them.
    pub wire: Option<String>,
}

impl crate::ExternIo for SseSendIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let wire = self
            .wire
            .take()
            .expect("an sse send descriptor is run exactly once");
        host.net_sse_send_now(self.conn, &wire)?;
        Ok(crate::NativeOut::Unit)
    }
}

/// The default SSE **close** descriptor: ends the event stream and releases the connection.
#[derive(Debug)]
pub struct SseCloseIo {
    /// The connection to close.
    pub conn: u64,
}

impl crate::ExternIo for SseCloseIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        host.net_sse_close_now(self.conn)?;
        Ok(crate::NativeOut::Unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a whole body in one feed, then finish — the "it all arrived at once" baseline.
    fn decode_all(framing: Framing, body: &str) -> Vec<Frame> {
        let mut decoder = FrameDecoder::new(framing);
        decoder.feed_str(body);
        decoder.finish();
        std::iter::from_fn(|| decoder.next_frame()).collect()
    }

    /// Decode the same body one **byte** at a time — the adversarial chunking that catches every
    /// "this only works when the chunk boundary is convenient" bug at once.
    fn decode_byte_by_byte(framing: Framing, body: &str) -> Vec<Frame> {
        let mut decoder = FrameDecoder::new(framing);
        let mut chunker = Utf8Chunker::new();
        for byte in body.as_bytes() {
            let text = chunker.push(&[*byte]);
            decoder.feed_str(&text);
        }
        let tail = chunker.finish();
        decoder.feed_str(&tail);
        decoder.finish();
        std::iter::from_fn(|| decoder.next_frame()).collect()
    }

    /// Every body must decode identically however the network chops it up. Asserted on every SSE
    /// case below, because chunk-boundary handling is where an event-stream reader actually breaks.
    fn assert_chunk_invariant(framing: Framing, body: &str) -> Vec<Frame> {
        let whole = decode_all(framing, body);
        let split = decode_byte_by_byte(framing, body);
        assert_eq!(
            whole, split,
            "byte-by-byte decoding must equal whole-body decoding for {body:?}"
        );
        whole
    }

    #[test]
    fn sse_parses_the_basic_event() {
        let frames = assert_chunk_invariant(Framing::Sse, "data: hello\n\n");
        assert_eq!(frames, vec![Frame::data("hello")]);
    }

    #[test]
    fn sse_multi_line_data_joins_with_newlines() {
        // The corner a `split("\n\n")` reader gets wrong in the other direction.
        let frames = assert_chunk_invariant(Framing::Sse, "data: a\ndata: b\ndata: c\n\n");
        assert_eq!(frames, vec![Frame::data("a\nb\nc")]);
    }

    #[test]
    fn sse_strips_exactly_one_leading_space() {
        // `data:  x` (two spaces) legitimately carries " x" — stripping greedily loses real data.
        let frames = assert_chunk_invariant(Framing::Sse, "data:  x\n\ndata:y\n\ndata: z\n\n");
        assert_eq!(
            frames,
            vec![Frame::data(" x"), Frame::data("y"), Frame::data("z")]
        );
    }

    #[test]
    fn sse_comments_are_ignored_and_dispatch_nothing() {
        // The `: keepalive` heartbeat: bytes on the wire, no event.
        let frames =
            assert_chunk_invariant(Framing::Sse, ": keepalive\n\n: another\ndata: real\n\n");
        assert_eq!(frames, vec![Frame::data("real")]);
    }

    #[test]
    fn sse_event_name_is_carried_and_resets_per_frame() {
        let frames = assert_chunk_invariant(Framing::Sse, "event: token\ndata: a\n\ndata: b\n\n");
        assert_eq!(
            frames,
            vec![Frame::named("token", "a"), Frame::data("b")],
            "an unnamed frame's event is empty, not the previous frame's name"
        );
    }

    #[test]
    fn sse_id_persists_across_frames_per_spec() {
        // The last-event-id buffer is the stream's, not the frame's — it is what a reconnecting
        // client replays with, so it deliberately survives a dispatch.
        let frames = assert_chunk_invariant(Framing::Sse, "id: 1\ndata: a\n\ndata: b\n\n");
        assert_eq!(frames[0].id, "1");
        assert_eq!(
            frames[1].id, "1",
            "the id persists until the server changes it"
        );
    }

    #[test]
    fn sse_id_with_a_nul_is_ignored() {
        let frames = decode_all(Framing::Sse, "id: 1\ndata: a\n\nid: b\0d\ndata: b\n\n");
        assert_eq!(frames[1].id, "1", "the NUL id is ignored, not adopted");
    }

    #[test]
    fn sse_retry_is_milliseconds_and_only_when_numeric() {
        let frames = assert_chunk_invariant(
            Framing::Sse,
            "retry: 3000\ndata: a\n\nretry: soon\ndata: b\n\ndata: c\n\n",
        );
        assert_eq!(frames[0].retry, Some(3000));
        assert_eq!(frames[1].retry, None, "a non-numeric retry is ignored");
        assert_eq!(
            frames[2].retry, None,
            "retry does not persist across frames"
        );
    }

    #[test]
    fn sse_a_block_without_data_dispatches_nothing() {
        // Per spec: the data buffer being empty means no event fires. A bare `retry:` block and a
        // bare `event:` block are both real traffic.
        let frames =
            assert_chunk_invariant(Framing::Sse, "retry: 3000\n\nevent: ping\n\ndata: real\n\n");
        assert_eq!(frames, vec![Frame::data("real")]);
    }

    #[test]
    fn sse_a_data_field_with_an_empty_value_does_dispatch() {
        // The distinction the `saw_data` flag exists for: `data:` carries a field (payload ""),
        // which is NOT the same as a block that carried no data field at all.
        let frames = assert_chunk_invariant(Framing::Sse, "data:\n\n");
        assert_eq!(frames, vec![Frame::data("")]);
    }

    #[test]
    fn sse_accepts_crlf_and_lone_cr_line_endings() {
        // All three terminators are legal, and a body may mix them.
        for body in [
            "data: a\r\n\r\n",
            "data: a\r\r",
            "data: a\n\n",
            "event: e\r\ndata: a\n\r\n",
        ] {
            let frames = assert_chunk_invariant(Framing::Sse, body);
            assert_eq!(frames.len(), 1, "body {body:?}");
            assert_eq!(frames[0].data, "a", "body {body:?}");
        }
    }

    #[test]
    fn a_crlf_split_across_chunks_is_one_line_ending() {
        // The specific bug: `\r` arrives at the end of one chunk and `\n` at the start of the
        // next. Treating the `\r` as a complete terminator dispatches a phantom empty line, which
        // ends the frame one field early.
        let mut decoder = FrameDecoder::new(Framing::Sse);
        decoder.feed_str("data: a\r");
        assert_eq!(decoder.ready_len(), 0, "an ambiguous trailing CR must wait");
        decoder.feed_str("\ndata: b\r\n\r\n");
        decoder.finish();
        let frames: Vec<Frame> = std::iter::from_fn(|| decoder.next_frame()).collect();
        assert_eq!(
            frames,
            vec![Frame::data("a\nb")],
            "the split CRLF is one terminator, so both data lines join into one frame"
        );
    }

    #[test]
    fn sse_unknown_fields_are_ignored() {
        let frames = assert_chunk_invariant(Framing::Sse, "foo: bar\ndata: a\n\n");
        assert_eq!(frames, vec![Frame::data("a")]);
    }

    #[test]
    fn sse_a_field_with_no_colon_has_an_empty_value() {
        // `data` alone is a `data:` field with value "" — so it dispatches an empty payload.
        let frames = assert_chunk_invariant(Framing::Sse, "data\n\n");
        assert_eq!(frames, vec![Frame::data("")]);
    }

    #[test]
    fn a_leading_bom_is_stripped_once() {
        let frames = assert_chunk_invariant(Framing::Sse, "\u{feff}data: a\n\n");
        assert_eq!(frames, vec![Frame::data("a")]);
        // Only at the very start: a later BOM is ordinary content.
        let frames = decode_all(Framing::Sse, "data: a\n\ndata: \u{feff}b\n\n");
        assert_eq!(frames[1].data, "\u{feff}b");
    }

    #[test]
    fn sse_discards_a_block_truncated_before_its_blank_line() {
        // The truncated-body contract: a frame dispatches on the blank line, so a body cut short
        // has not delivered one. Handing the partial over as if complete is the failure mode this
        // must not have.
        let frames = assert_chunk_invariant(Framing::Sse, "data: complete\n\ndata: partial");
        assert_eq!(frames, vec![Frame::data("complete")]);
    }

    #[test]
    fn an_empty_body_yields_no_frames() {
        for framing in [Framing::Sse, Framing::Ndjson, Framing::Lines] {
            assert_eq!(decode_all(framing, ""), vec![], "{framing}");
            assert_eq!(decode_byte_by_byte(framing, ""), vec![], "{framing}");
        }
    }

    #[test]
    fn the_openai_shape_decodes() {
        // A real OpenAI-compatible chunk sequence, terminator included.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
                    data: [DONE]\n\n";
        let frames = assert_chunk_invariant(Framing::Sse, body);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].data, "[DONE]");
        assert!(frames[0].data.contains("\"He\""));
    }

    #[test]
    fn ndjson_yields_one_frame_per_non_blank_line() {
        // Ollama's native shape, with the blank lines a server may pad with.
        let frames = assert_chunk_invariant(Framing::Ndjson, "{\"a\":1}\n\n{\"b\":2}\n");
        assert_eq!(
            frames,
            vec![Frame::data("{\"a\":1}"), Frame::data("{\"b\":2}")]
        );
        for frame in &frames {
            assert!(frame.event.is_empty() && frame.id.is_empty());
        }
    }

    #[test]
    fn ndjson_emits_a_final_unterminated_line() {
        // Unlike SSE: a line-oriented body's last line is a line even without its newline.
        let frames = assert_chunk_invariant(Framing::Ndjson, "{\"a\":1}\n{\"b\":2}");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].data, "{\"b\":2}");
    }

    #[test]
    fn lines_keeps_blank_lines_and_ndjson_does_not() {
        // The one behavioral difference between the two line framings.
        let body = "a\n\nb\n";
        assert_eq!(
            assert_chunk_invariant(Framing::Lines, body),
            vec![Frame::data("a"), Frame::data(""), Frame::data("b")]
        );
        assert_eq!(
            assert_chunk_invariant(Framing::Ndjson, body),
            vec![Frame::data("a"), Frame::data("b")]
        );
    }

    #[test]
    fn lines_handles_crlf_bodies() {
        let frames = assert_chunk_invariant(Framing::Lines, "a\r\nb\r\n");
        assert_eq!(frames, vec![Frame::data("a"), Frame::data("b")]);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        // The lossy-decode trap: feeding half of a `é` and then the other half must not produce
        // two replacement characters.
        let mut decoder = FrameDecoder::new(Framing::Lines);
        let mut chunker = Utf8Chunker::new();
        let bytes = "café\n".as_bytes();
        let (head, tail) = bytes.split_at(4); // splits the two-byte `é`
        decoder.feed_str(&chunker.push(head));
        decoder.feed_str(&chunker.push(tail));
        decoder.feed_str(&chunker.finish());
        decoder.finish();
        assert_eq!(decoder.next_frame(), Some(Frame::data("café")));
    }

    #[test]
    fn genuinely_invalid_bytes_degrade_to_a_replacement_character() {
        let mut chunker = Utf8Chunker::new();
        let text = chunker.push(&[b'a', 0xff, b'b']);
        assert_eq!(text, "a\u{fffd}b");
        assert_eq!(chunker.finish(), "");
    }

    #[test]
    fn several_invalid_sequences_in_one_chunk_do_not_stall_the_bytes_behind_them() {
        // The bug the scan loop exists for: stopping at the first invalid sequence held every
        // later byte back until the *next* chunk — and for a body whose last chunk this is, they
        // would never be emitted at all.
        let mut chunker = Utf8Chunker::new();
        assert_eq!(
            chunker.push(&[b'a', 0xff, b'b', 0xfe, b'c']),
            "a\u{fffd}b\u{fffd}c"
        );
        assert_eq!(chunker.finish(), "");
    }

    #[test]
    fn an_incomplete_tail_after_an_invalid_sequence_is_still_held_back() {
        // Both cases in one chunk: the invalid byte is consumed now, the split character waits.
        let mut chunker = Utf8Chunker::new();
        let mut bytes = vec![b'a', 0xff];
        bytes.extend_from_slice(&"é".as_bytes()[..1]); // the lead byte only
        assert_eq!(chunker.push(&bytes), "a\u{fffd}");
        assert_eq!(chunker.push(&"é".as_bytes()[1..]), "é");
        assert_eq!(chunker.finish(), "");
    }

    #[test]
    fn feeding_after_finish_is_inert() {
        let mut decoder = FrameDecoder::new(Framing::Lines);
        decoder.feed_str("a\n");
        decoder.finish();
        decoder.feed_str("b\n");
        assert_eq!(decoder.next_frame(), Some(Frame::data("a")));
        assert_eq!(decoder.next_frame(), None);
        assert!(decoder.is_done());
    }

    #[test]
    fn sse_wire_encoding_round_trips_through_the_decoder() {
        // The write side and the read side are inverses — an `sse` response read back by
        // `stream(..., Framing.Sse)` must yield exactly what was sent.
        let originals = vec![
            Frame::data("hello"),
            Frame::named("token", "a\nb"),
            Frame {
                event: "done".to_string(),
                data: String::new(),
                id: "42".to_string(),
                retry: Some(1500),
            },
        ];
        let mut wire = String::new();
        for frame in &originals {
            wire.push_str(&frame.to_sse_wire());
        }
        let decoded = assert_chunk_invariant(Framing::Sse, &wire);
        assert_eq!(decoded.len(), originals.len());
        for (decoded, original) in decoded.iter().zip(&originals) {
            assert_eq!(decoded.event, original.event);
            assert_eq!(decoded.data, original.data);
        }
        // The id persists per spec, so the third frame's id is the one that was sent.
        assert_eq!(decoded[2].id, "42");
        assert_eq!(decoded[2].retry, Some(1500));
    }

    #[test]
    fn an_empty_payload_still_produces_a_data_line() {
        // Without it the block carries no data field and the receiver dispatches nothing — the
        // frame would silently vanish in transit.
        assert_eq!(Frame::data("").to_sse_wire(), "data: \n\n");
        assert_eq!(
            decode_all(Framing::Sse, &Frame::data("").to_sse_wire()).len(),
            1
        );
    }

    #[test]
    fn a_comment_cannot_inject_a_frame() {
        // A newline inside a comment must not end the comment and let the rest parse as fields.
        let wire = sse_comment_wire("keepalive\ndata: injected");
        assert_eq!(wire, ": keepalive\n: data: injected\n");
        assert_eq!(
            decode_all(Framing::Sse, &format!("{wire}\ndata: real\n\n")),
            vec![Frame::data("real")]
        );
    }

    #[test]
    fn a_frame_stream_answers_its_head_without_reading_a_frame() {
        // The whole point of carrying the head on the handle: every question below is answerable
        // with zero frames consumed, which is the only state a streamed 429 ever reaches.
        let stream = FrameStream::new(StreamHead {
            stream: 7,
            status: 429,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Retry-After".to_string(), "30".to_string()),
            ],
            url: "https://api.test/v1/chat".to_string(),
        });
        assert_eq!(stream.status, 429);
        assert!(!stream.is_ok());
        // Case-insensitive, like `NetResponse::header_value` — a server may send any casing.
        assert_eq!(stream.header_value("retry-after"), Some("30"));
        assert_eq!(stream.header_value("x-absent"), None);
        assert!(StreamHead::ok(1, "https://api.test").status == 200);
        assert!(FrameStream::new(StreamHead::ok(1, "https://api.test")).is_ok());
        // A 2xx boundary either side, since `ok` is a range test and not `== 200`.
        for (status, ok) in [(199, false), (200, true), (299, true), (300, false)] {
            let mut probe = stream.clone();
            probe.status = status;
            assert_eq!(probe.is_ok(), ok, "status {status}");
        }
    }

    #[test]
    fn framing_names_round_trip() {
        for (name, framing) in FRAMING_VARIANTS {
            assert_eq!(framing.label(), *name);
            assert_eq!(name.parse::<Framing>(), Ok(*framing));
            assert_eq!(framing.to_string(), *name);
        }
        assert_eq!("Nope".parse::<Framing>(), Err(()));
    }
}
