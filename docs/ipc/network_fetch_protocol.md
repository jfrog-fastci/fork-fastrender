# Browser ↔ Network fetch IPC protocol

This document specifies the **byte-stream IPC protocol** used between the trusted **Browser**
process and the sandboxed **Network** process to perform HTTP(S) fetches.

Primary goals:

1. **Implementability**: another agent can implement both ends (browser↔network) without reading
   unrelated code.
2. **Auditability**: the protocol has explicit framing, explicit limits, and predictable state
   machines (multiplexing + cancellation).
3. **Security**: the Network process must not be able to force unbounded allocations in the Browser,
   and vice‑versa. Every decoding step has a hard limit.

Non-goals:

* This is **not** the Renderer↔Browser protocol. Renderers never talk to the Network process
  directly.
* This is **not** a full WHATWG Fetch implementation (CORS, CSP, mixed content, redirect policy).
  Those policy decisions are made by the Browser before sending a request to the Network process.

---

## Transport & framing

The protocol runs over a **reliable ordered byte stream** (e.g. a Unix domain socket, a pipe, or a
Windows named pipe).

### Frames

Each message is carried in a *frame*:

```
u32_be length
u8[length] payload
```

* `length` is the number of payload bytes (not including the 4‑byte prefix).
* `length` **MUST** be `<= MAX_FRAME_LEN` or the receiver **MUST** treat it as a protocol error and
  close the connection.
* The receiver must read exactly `length` bytes for the payload; partial reads are normal on a
  stream transport.

### Payload encoding

`payload` is a single encoded `NetworkFetchFrame` value (a Rust `enum` / tagged union).

Encoding is currently specified as:

* **bincode 1.x** encoding of the Rust enums/structs in this document
* with a hard decode limit of `MAX_FRAME_LEN` bytes per frame (defense in depth).

Rationale:

* binary encoding supports `Vec<u8>` without base64 (important for body chunks),
* the protocol is internal to FastRender (all participants are Rust),
* frame length caps keep decoding bounded.

If the project later moves to a different codec (e.g. `postcard`), the **frame format and state
machine stay the same**, and version negotiation (below) handles the transition.

---

## Version negotiation (Hello)

Immediately after connecting, the Browser must send `Hello`. No other message is valid before
`Hello` is processed.

Negotiation is *range-based*:

* Browser sends `[min_version, max_version]` it supports.
* Network responds with a single selected `version`, or rejects.

If negotiation fails, the connection must be closed (fail fast).

### `Hello` schema

```rust
/// Protocol version number.
pub type ProtocolVersion = u32;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
  pub min_version: ProtocolVersion,
  pub max_version: ProtocolVersion,

  /// Sender's hard cap for a single frame payload.
  pub max_frame_len: u32,

  /// Sender's preferred max inline body size (bytes) for *Start* messages.
  pub inline_body_max: u32,

  /// Sender's preferred max chunk payload size (bytes).
  pub body_chunk_max: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HelloAck {
  pub version: ProtocolVersion,

  /// Negotiated values (typically `min(browser, network)`).
  pub max_frame_len: u32,
  pub inline_body_max: u32,
  pub body_chunk_max: u32,
}
```

### Current constants (v1)

These are the **v1** hard limits. Implementations must reject larger values during negotiation:

* `PROTOCOL_VERSION = 1`
* `MAX_FRAME_LEN = 2_097_152` (2 MiB)
* `INLINE_BODY_MAX = 65_536` (64 KiB)
* `BODY_CHUNK_MAX = 65_536` (64 KiB)

Implementations may choose smaller negotiated values, but they must never exceed these caps.

---

## Request IDs and multiplexing

All fetches are multiplexed on a single connection using `RequestId`.

### `RequestId` rules

```rust
pub type RequestId = u64;
```

Rules:

* **Browser allocates** request IDs.
* `RequestId` is scoped to a single connection.
* `RequestId` **MUST NOT** be reused within a connection. Use a monotonically increasing counter.
  This avoids “stale message” ambiguity after cancellations/timeouts.
* `RequestId = 0` is reserved (must not be used) to keep “default/zeroed struct” bugs loud.

### Multiplexing rules

* Messages for different request IDs may be interleaved arbitrarily.
* For a given `request_id`, the message order must form a valid per-request state machine (see
  below). Because the transport is ordered, the receiver can validate this strictly.

---

## Message overview (v1)

The protocol uses two direction-specific enums. (This keeps parsing strict: the Network process must
never accept Browser-only messages and vice versa.)

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BrowserToNetwork {
  Hello(Hello),

  /// Start a new request.
  RequestStart(RequestStart),

  /// Request body chunk (only when `RequestStart.body` is `BodyStart::Chunked`).
  RequestBodyChunk(BodyChunk),

  /// Marks the end of the request body stream.
  RequestBodyEnd { request_id: RequestId },

  /// Best-effort cancellation.
  Cancel { request_id: RequestId },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum NetworkToBrowser {
  HelloAck(HelloAck),

  /// Response headers/status are ready.
  ResponseStart(ResponseStart),

  /// Response body chunk (only when `ResponseStart.body` is `BodyStart::Chunked`).
  ResponseBodyChunk(BodyChunk),

  /// Marks the end of the response body stream.
  ResponseBodyEnd { request_id: RequestId },

  /// Terminal error for the request (including cancellation).
  ResponseError(ResponseError),
}
```

`NetworkFetchFrame` is the on-wire payload for each frame; it is one of the two enums above,
depending on direction.

---

## Request / response schemas

### Headers representation

Headers are transported as an ordered list of `(name, value)` pairs. Duplicates are allowed.

* Header names must be ASCII, case-insensitive; on the wire they should be normalized to lowercase.
* Header values are UTF‑8 strings (lossy conversion must not be performed silently; invalid UTF‑8 is
  a protocol error).
* `set-cookie` is allowed to appear multiple times and must **not** be combined.

```rust
pub type Header = (String, String);
pub type Headers = Vec<Header>;
```

### Body transport (`BodyStart`)

Bodies use one of two strategies:

1. **Inline**: small bodies are embedded in the `*_Start` message.
2. **Chunked**: larger bodies are sent as a stream of `BodyChunk` frames terminated by `*_BodyEnd`.

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BodyStart {
  /// No body.
  Empty,

  /// Body bytes are carried inline in the *Start* message.
  Inline(Vec<u8>),

  /// Body bytes follow in `BodyChunk` messages until `*_BodyEnd`.
  ///
  /// `len` is the sender's best-effort known length (e.g. Content-Length). It can be `None` for
  /// unknown-length streaming responses.
  Chunked { len: Option<u64> },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BodyChunk {
  pub request_id: RequestId,
  pub data: Vec<u8>,
}
```

Constraints:

* `Inline(Vec<u8>)` length must be `<= INLINE_BODY_MAX`.
* `BodyChunk.data.len()` must be `<= BODY_CHUNK_MAX`.

### RequestStart

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestStart {
  pub request_id: RequestId,

  /// Absolute URL string (no fragments). Only `http`/`https` are valid in v1.
  pub url: String,

  /// ASCII HTTP method (e.g. "GET", "POST"). Case-insensitive on the wire.
  pub method: String,

  /// Request headers after Browser sanitization.
  pub headers: Headers,

  /// Request body transport descriptor.
  pub body: BodyStart,
}
```

### ResponseStart

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResponseStart {
  pub request_id: RequestId,

  /// Final URL that was fetched (after any internal normalization). In v1, Network must not follow
  /// redirects automatically; Browser handles redirect policy explicitly.
  pub final_url: String,

  pub status: u16,
  pub reason: String,
  pub headers: Headers,

  /// Response body transport descriptor.
  pub body: BodyStart,
}
```

### Errors

Errors are terminal for a request ID.

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ResponseErrorKind {
  /// DNS failure, connect error, TLS error, timeout, etc.
  Network,
  /// Protocol or HTTP parsing error inside Network process.
  Http,
  /// Browser canceled the request.
  Canceled,
  /// Network process rejected the request (e.g. unsupported scheme, exceeded limits).
  Rejected,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResponseError {
  pub request_id: RequestId,
  pub kind: ResponseErrorKind,

  /// Human-readable error message for logs/debugging (not stable for programmatic parsing).
  pub message: String,
}
```

---

## Flow: a chunked fetch (sequence diagram)

Example: a `POST` request with a chunked request body, returning a chunked response body.

```
Browser                                                Network
  |                                                      |
  |  Hello{min=1,max=1,max_frame_len=2MiB,...}            |
  |----------------------------------------------------->|
  |                                                      |
  |  HelloAck{version=1,max_frame_len=2MiB,...}           |
  |<-----------------------------------------------------|
  |                                                      |
  |  RequestStart{id=42,url=...,method="POST",            |
  |              body=Chunked{len=Some(120000)}}          |
  |----------------------------------------------------->|
  |  RequestBodyChunk{id=42,data=65536 bytes}             |
  |----------------------------------------------------->|
  |  RequestBodyChunk{id=42,data=54464 bytes}             |
  |----------------------------------------------------->|
  |  RequestBodyEnd{id=42}                                |
  |----------------------------------------------------->|
  |                                                      |
  |  ResponseStart{id=42,status=200,                      |
  |               body=Chunked{len=None}}                 |
  |<-----------------------------------------------------|
  |  ResponseBodyChunk{id=42,data=65536 bytes}            |
  |<-----------------------------------------------------|
  |  ResponseBodyChunk{id=42,data=1234 bytes}             |
  |<-----------------------------------------------------|
  |  ResponseBodyEnd{id=42}                               |
  |<-----------------------------------------------------|
  |                                                      |
```

Notes:

* The Browser may pipeline multiple `RequestStart`s without waiting for responses.
* The Network may start streaming `ResponseBodyChunk`s as soon as headers are known.
* Both sides must validate the per-request state machine (e.g. no body chunks before `*_Start`).

---

## Cancellation semantics

Cancellation is **best-effort** and may race with normal completion.

### Browser behavior

* Browser sends `Cancel { request_id }` once it no longer needs the result.
* Browser must stop sending request body chunks for that `request_id` after `Cancel`.
* Browser must be prepared to receive late `ResponseStart`/`ResponseBodyChunk` frames for a
  canceled request (due to race); it must ignore them.

### Network behavior

Upon receiving `Cancel { request_id }`:

* If the request is still active, Network should abort the underlying HTTP request if possible.
* Network must then send exactly one terminal message for that request:
  * either `ResponseError { kind: Canceled, ... }`
  * or (if it already completed) nothing additional.

### Protocol invariants

For a given `request_id`, exactly one of these must occur:

* Successful completion: `ResponseStart` + (optional chunks) + optional `ResponseBodyEnd`
* Failure: `ResponseError`

After a terminal message (`ResponseBodyEnd` or `ResponseError`), **no further messages** may be sent
for that request ID.

---

## Limits (explicit)

All limits below are **hard caps** enforced at the protocol boundary. Implementations may apply
stricter limits internally.

### Framing

* `MAX_FRAME_LEN = 2_097_152` bytes (2 MiB) payload limit per frame.
* Any frame advertising a larger `length` must terminate the connection.

### URL and method

* `MAX_URL_BYTES = 1_048_576` (1 MiB) UTF‑8 bytes for `RequestStart.url`.
* `MAX_METHOD_BYTES = 32` UTF‑8 bytes for `RequestStart.method`.
* Only `http` and `https` URL schemes are permitted in v1.

### Headers

These match the Fetch core limits (`WebFetchLimits`) so URL/header handling stays consistent:

* `MAX_HEADER_COUNT = 1024` (including duplicates).
* `MAX_TOTAL_HEADER_BYTES = 262_144` (256 KiB), computed as:
  `sum(name.len() + value.len())` for each header entry.

### Body transport

* `INLINE_BODY_MAX = 65_536` bytes.
* `BODY_CHUNK_MAX = 65_536` bytes per chunk.

### Total body size (defense in depth)

Per-request total body size caps should be enforced by whichever side buffers:

* `MAX_REQUEST_BODY_BYTES = 10 MiB`
* `MAX_RESPONSE_BODY_BYTES = 50 MiB`

If streaming is wired end-to-end (Browser does not buffer), then the receiver should still track a
running total and abort on overflow to avoid unbounded memory/disk growth.

---

## Cookie model assumptions

This section is about how cookies interact with **Browser↔Network fetch**. It intentionally does not
specify the Renderer↔Browser cookie API in detail.

### Authority / ownership

* The **Browser process** is the source of truth for cookie state.
* The **Network process** is treated as *stateless with respect to cookies* in v1:
  * it does **not** maintain a cookie jar,
  * it does **not** synthesize `Cookie` headers,
  * it does **not** persist cookie state across requests.

This design limits cookie exposure: the Network process only sees cookies that the Browser elected
to attach to the specific request being executed.

### Sending cookies (`Cookie` request header)

* When the Browser wants cookies attached (e.g. credentials mode includes cookies), it includes a
  `cookie` header entry in `RequestStart.headers`.
* When cookies should not be attached, the Browser must omit the `cookie` header entirely.
* The Network process must forward headers exactly as provided (modulo transport-level normalization
  like HTTP/2 pseudo headers).

### Receiving cookies (`Set-Cookie` response headers)

* The Network process must include any received `set-cookie` headers in `ResponseStart.headers`.
* The Browser consumes `set-cookie` and updates its cookie jar.
* When the Browser forwards response headers to untrusted renderer/JS surfaces, it should apply the
  Fetch/Headers guard behavior (i.e. `Set-Cookie` is a forbidden response header and must not be
  exposed to JS).

### `document.cookie`

`document.cookie` is implemented in the renderer, but it must be backed by Browser-owned cookie
state:

* `document.cookie` **getter**: Browser returns the cookie-string for the active document URL.
  A simple implementation can reuse the same “cookie header value for URL” computation used to
  generate outbound `Cookie` headers.
* `document.cookie` **setter**: Browser treats the setter string as an RFC6265-style cookie-string
  (same format as `Set-Cookie`) and updates the cookie jar for the active document URL.

Current FastRender MVP cookie semantics (important for audits):

* `document.cookie` stores only `name=value` pairs; cookie attributes are ignored.
* Per-document limits are enforced (see `src/js/cookie_jar.rs`):
  * max cookie pairs per document
  * max total cookie-string bytes

Future work (not in v1 protocol):

* RFC6265 attribute semantics (`HttpOnly`, `Secure`, `SameSite`, path/domain scoping),
* partitioned cookies / first-party sets,
* persistent cookie storage managed by the Browser.

---

## Body transport: shared-memory future work (outline)

Chunked IPC is simple but copies bytes at least once per hop. For large responses (images, fonts,
JS bundles), future versions may add a third `BodyStart` mode:

* `SharedMemory { handle, len, mime_hint }`

Where `handle` is an OS-specific transferable handle:

* Linux: `memfd` file descriptor (SCM_RIGHTS)
* Windows: duplicated handle
* macOS: shared memory / mach port

The negotiation `Hello` can advertise support for shared memory and negotiate a maximum segment
size. Until then, v1 must use inline/chunked bodies only.

