# Browser ↔ Network fetch IPC protocol (JSON, length-prefixed frames)

This document describes the **fetch IPC protocol** used by FastRender’s network service boundary.

In-tree, this protocol is implemented by:

- Client: `crate::resource::ipc_fetcher::IpcResourceFetcher` (`src/resource/ipc_fetcher.rs`)
- Server: `crate::ipc::network_service::IpcFetchServer` (`src/ipc/network_service.rs`)
- Test network-process binary: `src/bin/network_process.rs`

Terminology:

- **Client**: the process asking for network operations (today this is typically the renderer; in a
  split browser/renderer architecture this can also be the browser).
- **Server / Network process**: the process that executes network I/O (HTTP fetch) and owns network
  state.

Threat model:

- Treat both endpoints as potentially compromised; **every decode must be bounded** and must reject
  malformed payloads **before allocating**.

This doc is normative for the `ipc_fetcher` protocol. It is **not**:

- the browser↔renderer IPC protocol (`src/ipc/protocol/renderer.rs`), or
- the in-tree “planned” browser↔network schema (`src/ipc/protocol/network.rs`).

---

## Transport & framing

The protocol runs over a **reliable ordered byte stream** (today: `TcpStream`; long-term: Unix
domain sockets / named pipes).

### Frame format

Each message is a frame:

```
u32_le length
u8[length] json_payload
```

- `length` is the byte length of `json_payload` (not including the 4-byte prefix).
- `length == 0` is invalid and must be treated as a protocol violation.
- The receiver must reject oversized frames **before allocating**:
  - inbound frames (client → server): `IPC_MAX_INBOUND_FRAME_BYTES`
  - outbound frames (server → client): `IPC_MAX_OUTBOUND_FRAME_BYTES`

Constants (as implemented in `src/resource/ipc_fetcher.rs`):

- `IPC_MAX_INBOUND_FRAME_BYTES = 8 MiB`
- `IPC_MAX_OUTBOUND_FRAME_BYTES = 80 MiB`

For the purposes of this document, these are the effective **max frame lengths**:

- `MAX_FRAME_LEN` (client → server) = `IPC_MAX_INBOUND_FRAME_BYTES` (8 MiB)
- `MAX_FRAME_LEN` (server → client) = `IPC_MAX_OUTBOUND_FRAME_BYTES` (80 MiB)

### Payload encoding (JSON)

Each frame payload is a single UTF‑8 JSON value serialized via `serde_json`.

Important for implementers in other languages:

- Rust `enum`s use serde’s **externally tagged** default representation:
  - struct-like variants encode as `{"VariantName": {"field": ...}}`
  - unit variants encode as `"VariantName"`
- `IpcRequest` and `IpcResponse` are `#[serde(deny_unknown_fields)]` (unknown fields must fail
  closed).

### Example JSON payloads (no length prefix shown)

Hello request (unenveloped):

```json
{"Hello":{"token":"<redacted>"}}
```

Hello response (unenveloped unit variant):

```json
"HelloAck"
```

Enveloped fetch request:

```json
{
  "id": 42,
  "request": {
    "Fetch": { "url": "https://example.com/" }
  }
}
```

Chunked fetch response start:

```json
{
  "FetchStart": {
    "id": 42,
    "meta": {
      "content_type": "text/html",
      "nosniff": false,
      "content_encoding": null,
      "status": 200,
      "etag": null,
      "last_modified": null,
      "access_control_allow_origin": null,
      "timing_allow_origin": null,
      "vary": null,
      "response_referrer_policy": null,
      "access_control_allow_credentials": false,
      "final_url": "https://example.com/",
      "cache_policy": null,
      "response_headers": null
    },
    "total_len": 2000000
  }
}
```

---

## Connection setup (Hello handshake)

Immediately after connecting, the client must send an **unenveloped** hello frame:

```rust
pub enum IpcRequest {
  Hello { token: String },
  /* ... other requests ... */
}
```

The server replies with an **unenveloped** hello acknowledgement:

```rust
pub enum IpcResponse {
  HelloAck,
  /* ... other responses ... */
}
```

### Auth token

The token is an out-of-band secret (typically provided to both sides via
`FASTR_NETWORK_AUTH_TOKEN` / `IPC_AUTH_TOKEN_ENV`). If the token is wrong, the server closes the
connection without sending an error (minimize information leakage).

Hard cap: `IPC_MAX_AUTH_TOKEN_BYTES = 1024`.

### Version negotiation (current status)

The current implementation does **not** negotiate a protocol version; client and server must be
deployed in lockstep.

Future work: extend `Hello` to include a `protocol_version` or a supported version range so upgrades
can be rolled out safely.

---

## Request IDs and (future) multiplexing

After the hello handshake, every request is sent in an envelope:

```rust
pub struct BrowserToNetwork {
  pub id: u64,
  pub request: IpcRequest,
}
```

Despite the name (`BrowserToNetwork`), this is the **client → server** request envelope.

### Request ID rules

- IDs are `u64`.
- Client must allocate IDs; in-tree `IpcResourceFetcher` uses a monotonically increasing counter
  starting at `1`.
- IDs must not be reused on the same connection (avoids “stale response” confusion).
- For correctness/safety, implementations should treat `id = 0` as reserved (even though this is not
  currently enforced by `validate_ipc_request`).

### Multiplexing

The wire format is ID-addressed and therefore *can* support multiplexing, but the current client
implementation serializes requests with a mutex around the connection and expects responses to be in
order.

If a future implementation allows concurrent in-flight requests on one connection, the invariant is:

- Responses must carry the same `id` as the request they correspond to.
- A client must ignore any response with an unexpected `id`.

---

## Message schemas (as implemented)

### Client → server: `IpcRequest`

Defined in `src/resource/ipc_fetcher.rs`:

```rust
pub enum IpcRequest {
  Hello { token: String },

  // Fetch primitives.
  Fetch { url: String },
  FetchWithRequest { req: IpcFetchRequest },
  FetchWithRequestAndValidation { req: IpcFetchRequest, etag: Option<String>, last_modified: Option<String> },
  FetchHttpRequest { req: IpcHttpRequest },
  FetchPartialWithContext { kind: FetchContextKind, url: String, max_bytes: u64 },
  FetchPartialWithRequest { req: IpcFetchRequest, max_bytes: u64 },

  // Cookie access.
  CookieHeaderValue { url: String },
  StoreCookieFromDocument { url: String, cookie_string: String },

  // Cache plumbing (disk cache / artifacts).
  RequestHeaderValue { req: IpcFetchRequest, header_name: String },
  ReadCacheArtifact { kind: FetchContextKind, url: String, artifact: CacheArtifactKind },
  ReadCacheArtifactWithRequest { req: IpcFetchRequest, artifact: CacheArtifactKind },
  WriteCacheArtifact { kind: FetchContextKind, url: String, artifact: CacheArtifactKind, bytes_b64: String, source: Option<IpcCacheSourceMetadata> },
  WriteCacheArtifactWithRequest { req: IpcFetchRequest, artifact: CacheArtifactKind, bytes_b64: String, source: Option<IpcCacheSourceMetadata> },
  RemoveCacheArtifact { kind: FetchContextKind, url: String, artifact: CacheArtifactKind },
  RemoveCacheArtifactWithRequest { req: IpcFetchRequest, artifact: CacheArtifactKind },
}
```

`IpcFetchRequest` captures the request context needed for browser-ish Fetch semantics:

```rust
pub struct IpcFetchRequest {
  pub url: String,
  pub destination: FetchDestination,
  pub referrer_url: Option<String>,
  pub client_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub credentials_mode: FetchCredentialsMode,
}
```

`IpcHttpRequest` is the “JS fetch/XHR shaped” request:

```rust
pub struct IpcHttpRequest {
  pub fetch: IpcFetchRequest,
  pub method: String,
  pub redirect: web_fetch::RequestRedirect,
  pub headers: Vec<(String, String)>,
  pub body_b64: Option<String>,
}
```

Request body note:

- `body_b64` is base64-encoded and hard-limited: decoded bytes must be `<= IPC_MAX_REQUEST_BODY_BYTES`
  (1 MiB). Larger uploads need a future chunked request-body extension.

### Server → client: `NetworkToBrowser`

The server replies with one of:

```rust
pub enum NetworkToBrowser {
  // Single-frame response (most RPCs and small fetch bodies).
  Response { id: u64, response: IpcResponse },

  // Chunked fetch response stream for large bodies.
  FetchStart { id: u64, meta: IpcFetchedResourceMeta, total_len: usize },
  FetchBodyChunk { id: u64, bytes_b64: String },
  FetchEnd { id: u64 },

  // Chunked fetch error (sent instead of FetchStart/FetchEnd).
  FetchErr { id: u64, err: IpcError },
}
```

Single-frame responses use:

```rust
pub enum IpcResponse {
  HelloAck,
  Fetched(IpcResult<IpcFetchedResource>),
  MaybeFetched(IpcResult<Option<IpcFetchedResource>>),
  MaybeString(IpcResult<Option<String>>),
  Unit(IpcResult<()>),
}

pub enum IpcResult<T> {
  Ok(T),
  Err(IpcError),
}
```

### Response payload structs (as serialized)

Fetch responses carry either a full `IpcFetchedResource` (inline body) or an `IpcFetchedResourceMeta`
followed by chunk bytes (chunked body):

```rust
pub struct IpcFetchedResourceMeta {
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub response_referrer_policy: Option<ReferrerPolicy>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<IpcHttpCachePolicy>,
  pub response_headers: Option<Vec<(String, String)>>,
}

pub struct IpcFetchedResource {
  /// Base64 (standard alphabet, padded) of the entire response body.
  pub bytes_b64: String,
  pub content_type: Option<String>,
  pub nosniff: bool,
  pub content_encoding: Option<String>,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  pub access_control_allow_origin: Option<String>,
  pub timing_allow_origin: Option<String>,
  pub vary: Option<String>,
  pub response_referrer_policy: Option<ReferrerPolicy>,
  pub access_control_allow_credentials: bool,
  pub final_url: Option<String>,
  pub cache_policy: Option<IpcHttpCachePolicy>,
  pub response_headers: Option<Vec<(String, String)>>,
}

pub struct IpcError {
  pub message: String,
  pub content_type: Option<String>,
  pub status: Option<u16>,
  pub final_url: Option<String>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
}

pub struct IpcHttpCachePolicy {
  pub max_age: Option<u64>,
  pub s_maxage: Option<u64>,
  pub no_cache: bool,
  pub no_store: bool,
  pub must_revalidate: bool,
  pub expires_epoch_secs: Option<u64>,
  pub date_epoch_secs: Option<u64>,
  pub age: Option<u64>,
  pub stale_if_error: Option<u64>,
  pub stale_while_revalidate: Option<u64>,
  pub last_modified_epoch_secs: Option<u64>,
}
```

Notes:

- Base64 uses the standard padded alphabet (`base64::engine::general_purpose::STANDARD`); decoders must
  reject non-multiple-of-4 lengths and enforce the per-chunk/per-body decoded byte caps.
- `response_headers` is an ordered list of `(name, value)` pairs and may include duplicates (e.g.
  multiple `set-cookie` values).
- Optional fields are currently serialized as explicit `null` values (the structs do not use
  `skip_serializing_if = "Option::is_none"`).

---

## Body transport strategy (inline vs chunked)

Fetch responses can be returned in two ways:

### 1) Inline body (single frame)

If `res.bytes.len() <= IPC_INLINE_LIMIT_BYTES` (1 MiB), the server sends:

- `NetworkToBrowser::Response { id, response: IpcResponse::Fetched(IpcResult::Ok(IpcFetchedResource{ bytes_b64, ... })) }`

Where:

- `IpcFetchedResource.bytes_b64` is the base64 of the entire response body.

### 2) Chunked body stream (multiple frames)

If `res.bytes.len() > IPC_INLINE_LIMIT_BYTES`, the server sends:

1. `FetchStart { id, meta, total_len }`
2. `0..N` `FetchBodyChunk { id, bytes_b64 }`
3. `FetchEnd { id }`

Constraints:

- `total_len` is the exact byte length of the response body.
- Each chunk’s decoded length must be `<= IPC_CHUNK_MAX_BYTES` (64 KiB).
- Client must enforce `total_len <= IPC_MAX_BODY_BYTES` (50 MiB) and reject overflow / extra bytes.

### Shared-memory / FD-backed future work

This protocol currently base64-encodes bodies (simple but copy-heavy). Future work options:

- Transfer bodies out-of-band via FD passing (pipe/memfd) on Unix sockets.
- Transfer bodies via shared memory (memfd/MapViewOfFile/etc) plus a small control message carrying
  `{ id, len, mime_hint }`.

Both approaches keep per-message control frames small and avoid base64 overhead.

---

## Flow: fetch with chunked response body (sequence diagram)

Example: client issues an `IpcRequest::FetchHttpRequest` whose response body exceeds
`IPC_INLINE_LIMIT_BYTES`, so the server streams it.

```
Client (renderer/browser)                              Network process
  |                                                      |
  |  frame: IpcRequest::Hello { token }                  |
  |----------------------------------------------------->|
  |  frame: IpcResponse::HelloAck                        |
  |<-----------------------------------------------------|
  |                                                      |
  |  frame: BrowserToNetwork {                            |
  |           id: 42,                                     |
  |           request: IpcRequest::FetchHttpRequest{...}  |
  |         }                                             |
  |----------------------------------------------------->|
  |                                                      |
  |  frame: NetworkToBrowser::FetchStart { id:42, total_len: 2000000, ... } |
  |<-----------------------------------------------------|
  |  frame: NetworkToBrowser::FetchBodyChunk { id:42, bytes_b64: \"...\" }  |
  |<-----------------------------------------------------|
  |  frame: NetworkToBrowser::FetchBodyChunk { id:42, bytes_b64: \"...\" }  |
  |<-----------------------------------------------------|
  |  frame: NetworkToBrowser::FetchEnd { id:42 }          |
  |<-----------------------------------------------------|
```

---

## Error reporting vs protocol violations

There are two classes of failures:

### 1) Structured per-request errors (normal)

Network/fetch failures (DNS, TLS, HTTP errors surfaced as `Error`, etc.) are returned as an `IpcError`
inside:

- `NetworkToBrowser::Response { .. IpcResponse::Fetched(IpcResult::Err(IpcError)) .. }` (inline path),
  or
- `NetworkToBrowser::FetchErr { id, err: IpcError }` (chunked path).

These are *request-scoped* and do not imply the connection is unusable.

### 2) Protocol violations (fatal to the connection)

Malformed frames or messages (bad JSON, invalid lengths, `id` mismatch, invalid chunk sizes, etc.)
are treated as **fatal** and the receiver closes the connection.

Examples that trigger a hard close in the in-tree client implementation:

- response `id` does not match the request `id`
- `FetchBodyChunk` decoded length exceeds `IPC_CHUNK_MAX_BYTES`
- chunk stream sends more bytes than `FetchStart.total_len`

Examples that trigger a hard close in the in-tree server (`IpcFetchServer`):

- invalid JSON
- request fails `validate_ipc_request` (oversize URL/headers/body, invalid header tokens, etc.)
- `Hello` is sent after the initial handshake

---

## Cancellation semantics (current)

This protocol does **not** define an explicit `Cancel` message.

Current best-effort cancellation behavior:

- The client may drop/close the connection to abort waiting for a response.
- The server will observe EOF and exit its request loop for that connection.
- There is no guarantee that an in-flight HTTP request is aborted; the server uses a blocking fetcher.

Future work:

- Add `Cancel { id }` so the client can explicitly cancel an in-flight request on a long-lived
  connection.
- Add request-body streaming so uploads can be canceled mid-stream.

---

## Explicit size limits (auditable)

All values below are **hard caps** in the current implementation.

### Frame-level caps

| Direction | Constant | Value |
|---|---:|---:|
| client → server | `IPC_MAX_INBOUND_FRAME_BYTES` | 8 MiB |
| server → client | `IPC_MAX_OUTBOUND_FRAME_BYTES` | 80 MiB |

### Request field caps

| Field | Constant | Value |
|---|---:|---:|
| HTTP(S) URL length | `IPC_MAX_URL_BYTES` | 8 KiB |
| non-HTTP(S) URL length (`data:`, `file:`, etc) | `IPC_MAX_NON_HTTP_URL_BYTES` | 1 MiB |
| header count | `IPC_MAX_HEADER_COUNT` | 256 |
| header name bytes | `IPC_MAX_HEADER_NAME_BYTES` | 1024 |
| header value bytes | `IPC_MAX_HEADER_VALUE_BYTES` | 1024 |
| method bytes | `IPC_MAX_METHOD_BYTES` | 64 |
| decoded request body bytes (`IpcHttpRequest.body_b64`) | `IPC_MAX_REQUEST_BODY_BYTES` | 1 MiB |
| `document.cookie` setter string bytes | `MAX_COOKIE_BYTES` | 4096 |
| auth token bytes | `IPC_MAX_AUTH_TOKEN_BYTES` | 1024 |

### Response body caps (defense in depth)

| Purpose | Constant | Value |
|---|---:|---:|
| inline body threshold (switch to chunked) | `IPC_INLINE_LIMIT_BYTES` | 1 MiB |
| max decoded bytes per chunk | `IPC_CHUNK_MAX_BYTES` | 64 KiB |
| max total chunked body bytes (client-side) | `IPC_MAX_BODY_BYTES` | 50 MiB |

Notes:

- The outbound frame cap is high because base64 expands bodies. Chunking keeps typical frames small
  even when the cap is generous.

---

## Cookie jar model (assumptions)

Current model (as implemented by `IpcFetchServer` running an in-process `HttpFetcher`):

- The **network process owns the cookie jar** (it is inside `HttpFetcher`).
- Cookies are persisted in-memory for the lifetime of the network process and are reused across
  requests (and typically across connections, since `HttpFetcher` clones share the underlying jar).

### `Set-Cookie` handling

- When `HttpFetcher` receives `Set-Cookie` headers, it updates the cookie jar.
- The updated cookie jar affects future requests’ `Cookie` header synthesis (subject to
  `FetchCredentialsMode` and request origin metadata).

### `document.cookie` handling

The renderer’s `document.cookie` plumbing uses two IPC calls:

- **Getter**: `IpcRequest::CookieHeaderValue { url }` → `IpcResponse::MaybeString(Ok(Some(cookie_header)))`
- **Setter**: `IpcRequest::StoreCookieFromDocument { url, cookie_string }` → `IpcResponse::Unit(...)`

Semantics:

- `cookie_string` is treated as an RFC6265-style cookie string. The network process stores it in the
  cookie jar (attributes are interpreted by the HTTP client; renderer-side `CookieJar` also applies
  additional deterministic limits).
- `cookie_string` is capped to 4096 bytes by `validate_ipc_request`.

MVP caveats (important for audits):

- The renderer-side `document.cookie` implementation (`src/js/cookie_jar.rs`) stores only
  `name=value` pairs and ignores cookie attributes (Path/Domain/SameSite/HttpOnly/etc). It also uses
  deterministic bounds (max cookie count and max string bytes).
- Because the getter path is backed by `CookieHeaderValue` (\"cookies that would be sent\"), the
  current behavior may expose cookies that a full browser would hide from `document.cookie` (notably
  `HttpOnly`). Treat this as an MVP limitation, not a security guarantee.

Security note:

- The client must **not** expose `Set-Cookie` response headers directly to untrusted JS; `Set-Cookie`
  is a forbidden response header in Fetch.
