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

Enveloped HTTP request (`FetchHttpRequest`, with method/headers/body):

```json
{
  "id": 42,
  "request": {
    "FetchHttpRequest": {
      "req": {
        "fetch": {
          "url": "https://example.com/api",
          "destination": "Fetch",
          "referrer_url": "https://example.com/",
          "client_origin": { "scheme": "https", "host": "example.com", "port": 443 },
          "referrer_policy": "StrictOriginWhenCrossOrigin",
          "credentials_mode": "Include"
        },
        "method": "POST",
        "redirect": "Follow",
        "headers": [
          ["content-type", "application/json"],
          ["accept", "application/json"]
        ],
        "body_b64": "eyJmb28iOiJiYXIifQ=="
      }
    }
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

Chunked fetch body chunk:

```json
{
  "FetchBodyChunk": {
    "id": 42,
    "bytes_b64": "AAECAwQF"
  }
}
```

Chunked fetch response end:

```json
{
  "FetchEnd": {
    "id": 42
  }
}
```

Fetch error (for a fetch-like request):

```json
{
  "FetchErr": {
    "id": 42,
    "err": {
      "message": "fetch failed for https://example.com/: DNS error",
      "content_type": null,
      "status": null,
      "final_url": null,
      "etag": null,
      "last_modified": null
    }
  }
}
```

Inline fetch success (single-frame body):

```json
{
  "Response": {
    "id": 42,
    "response": {
      "Fetched": {
        "Ok": {
          "bytes_b64": "SGVsbG8sIHdvcmxkIQ==",
          "content_type": "text/plain",
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
        }
      }
    }
  }
}
```

CookieHeaderValue response (cookie getter):

```json
{
  "Response": {
    "id": 7,
    "response": {
      "MaybeString": {
        "Ok": "a=b; c=d"
      }
    }
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

### Connection lifetime

This is a **long-lived** connection:

- One `Hello`/`HelloAck` handshake at connect time.
- Many request/response exchanges.
- On any protocol violation, the receiver closes the connection; the client must reconnect and redo
  the hello handshake.

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

The wire format is ID-addressed and therefore *could* support multiplexing, but the current
implementation is **strictly sequential**:

- The client (`IpcResourceFetcher`) uses a mutex around the connection and issues a request only when
  it can synchronously read that request’s full response.
- The server (`IpcFetchServer`) processes requests in a simple loop and writes the response frames
  inline during that loop (including all chunk frames for large bodies).

**Protocol rule (v1 / implemented behavior):** on a single connection, there is at most **one**
request “in flight” at a time. A client must not send request `N+1` until it has fully consumed the
response for request `N` (including `FetchEnd` for chunked bodies).

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

#### Semantics of `IpcFetchRequest` fields (as implemented by `HttpFetcher`)

These fields are **security-sensitive** because they influence:

- which cookies are attached,
- which browser-like request headers are synthesized (`Origin`, `Referer`, `Sec-Fetch-*`, …), and
- whether CORS checks are enforced on the response.

Key points:

- `destination` drives “fetch mode” behavior:
  - destinations with `sec_fetch_mode() == "cors"` (notably `Fetch`, `Font`, and the `*Cors` variants)
    trigger response-side CORS checks in `HttpFetcher` (`enforce_cors_on_network_response`).
  - destinations with `sec_fetch_mode() == "no-cors"` skip those checks (browser-like behavior for
    passive subresources).
- `credentials_mode` controls cookie inclusion (`cookies_allowed_for_request`):
  - `Include`: always attach cookies (if cookie state is available).
  - `Omit`: never attach cookies.
  - `SameOrigin`: attach cookies only when `client_origin` is present and same-origin with `url`.
- `client_origin` is the strongest input for request-site classification and CORS enforcement. If it
  is missing, `HttpFetcher` may **skip** response-side CORS checks for `cors`-mode destinations to
  avoid over-blocking navigations without a known origin.

Security note for multiprocess designs:

- If the **sender is untrusted** (e.g. a compromised renderer), the receiver must not treat fields
  like `destination` and `client_origin` as authoritative security policy inputs. A malicious sender
  could otherwise downgrade `destination` to a `no-cors` mode to bypass CORS checks.
  - In a hardened browser architecture, the browser process should supply/validate these fields
    (e.g. via a SiteLock / trusted initiator origin), or the protocol should be redesigned so the
    sender cannot choose a weaker policy than the browser intended.

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

### `FetchHttpRequest` header semantics (security boundary)

`IpcRequest::FetchHttpRequest` is the only request type that carries **user-specified** HTTP headers
(`IpcHttpRequest.headers`). This is a security-sensitive surface: a compromised renderer must not be
able to spoof privileged headers like `Cookie`, `Host`, `Origin`, `Referer`, or `Sec-Fetch-*`.

Protocol/implementation expectations:

- `headers` is an ordered list of `(name, value)` pairs and may contain duplicates.
- The network process must:
  - validate header tokens and reject NUL/CR/LF in names/values (already enforced by
    `validate_ipc_request`),
  - drop/ignore **forbidden Fetch request headers** (including `Cookie`, `Host`, `Origin`,
    `Referer`, `User-Agent`, and any `Sec-*`/`Proxy-*` headers), and
  - synthesize/overwrite security-relevant headers based on `IpcFetchRequest` metadata (destination,
    origin/referrer, credentials mode) and its own policy.
- In-tree reference: `HttpFetcher` enforces this when executing `fetch_http_request` via:
  - `merge_user_request_headers(...)` and
  - `fetch_http_request_header_forbidden(...)`
  in `src/resource.rs`.

### Request → response mapping (what peers should accept)

The protocol is “RPC-like”: each request results in exactly one logical response for the same
request `id`, but fetch-like requests may stream the body over multiple frames.

Notes:

- Some response “error” shapes exist in the schema but are not emitted by the in-tree server today
  (the server tends to treat non-fetch RPCs as infallible/best-effort).
- For `Option<T>` fields, JSON uses `null` for `None`.

| Request (`IpcRequest`) | Response shape(s) | Notes |
|---|---|---|
| `Fetch*` / `FetchHttpRequest` / `FetchPartial*` | **Either** `Response{id, IpcResponse::Fetched(Ok(IpcFetchedResource{bytes_b64,..}))}` **or** `FetchStart/FetchBodyChunk*/FetchEnd` **or** `FetchErr{id, IpcError}` | “Fetch-like”: may use chunked body when `bytes.len() > IPC_INLINE_LIMIT_BYTES`. For `FetchPartial*`, the in-tree client treats a `206 Partial Content` response as valid only when `Content-Range` starts at 0. |
| `RequestHeaderValue{..}` | `Response{id, IpcResponse::MaybeString(Ok(<string-or-null>))}` | `Ok(null)` means “unknown / cannot determine header value”. Used for cache `Vary` keying. |
| `CookieHeaderValue{..}` | `Response{id, IpcResponse::MaybeString(Ok(<string-or-null>))}` | `Ok(Some("a=b; c=d"))` means the `Cookie` header value that would be sent. `Ok(Some(""))` means “cookie support enabled, but there are no matching cookies”. `Ok(null)` means cookie state is not exposed or the URL was invalid/unparseable. |
| `StoreCookieFromDocument{..}` | `Response{id, IpcResponse::Unit(Ok(()))}` or `Response{id, IpcResponse::Unit(Err(IpcError))}` | In-tree server always returns `Ok(())` (best-effort). |
| `ReadCacheArtifact*{..}` | `Response{id, IpcResponse::MaybeFetched(Ok(<resource-or-null>))}` | Returned body is always inline base64 (no chunked stream for cache artifacts). In-tree server always returns `Ok(...)`. |
| `WriteCacheArtifact*{..}` | `Response{id, IpcResponse::Unit(Ok(()))}` or `Response{id, IpcResponse::Unit(Err(IpcError))}` | Base64 decode errors are surfaced as `Err(IpcError)` (in-tree server). |
| `RemoveCacheArtifact*{..}` | `Response{id, IpcResponse::Unit(Ok(()))}` or `Response{id, IpcResponse::Unit(Err(IpcError))}` | In-tree server always returns `Ok(())` (best-effort). |

### Validation rules (server-side, hard failures)

The server treats incoming requests as attacker-controlled input and validates before executing.
Validation is performed by `validate_ipc_request()` in `src/resource/ipc_fetcher.rs` and includes:

- URL byte length caps:
  - `http`/`https` URLs: `IPC_MAX_URL_BYTES` (8 KiB)
  - other schemes: `IPC_MAX_NON_HTTP_URL_BYTES` (1 MiB)
- URL byte restrictions:
  - NUL bytes are always rejected
  - CR/LF are rejected for `http`/`https` URLs (avoid header injection / parsing ambiguity)
- HTTP method:
  - non-empty, <= `IPC_MAX_METHOD_BYTES`
  - must parse as a valid `http::Method`
  - no NUL/CR/LF
- Headers:
  - count <= `IPC_MAX_HEADER_COUNT`
  - name/value <= `IPC_MAX_HEADER_NAME_BYTES` / `IPC_MAX_HEADER_VALUE_BYTES`
  - name must parse as a valid `http::header::HeaderName`
  - name/value must not contain NUL/CR/LF
- Request body (`body_b64`):
  - conservative upper bound estimate before decode (avoid allocating huge attacker strings)
  - decoded bytes <= `IPC_MAX_REQUEST_BODY_BYTES`
- `document.cookie` setter string is capped (`MAX_COOKIE_BYTES = 4096`).

Validation failures are treated as **protocol violations**; the server closes the connection (see
“Error reporting vs protocol violations” below).

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

  // Fetch error (used for any fetch-like request; may be sent instead of FetchStart/FetchEnd).
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

Note: this chunking is a **message framing** strategy only. The in-tree network process fetcher
returns a fully-buffered `Vec<u8>` and then splits it into chunks for IPC; it does not stream bytes
incrementally from the remote origin.

### Per-request state machine (server → client)

For a single request `id`, the server sends exactly one of the following sequences:

1. **Non-fetch RPC response** (cookies, cache artifacts, etc):

   ```
   Response(id, IpcResponse::<...>)
   ```

2. **Fetch success, inline body** (`bytes.len() <= IPC_INLINE_LIMIT_BYTES`):

   ```
   Response(id, IpcResponse::Fetched(Ok(IpcFetchedResource{ bytes_b64, ... })))
   ```

3. **Fetch success, chunked body** (`bytes.len() > IPC_INLINE_LIMIT_BYTES`):

   ```
   FetchStart(id, meta, total_len)
   FetchBodyChunk(id, bytes_b64)   // 0..N chunks
   FetchEnd(id)
   ```

4. **Fetch error** (any fetch-like request failure):

   ```
   FetchErr(id, IpcError)
   ```

Any deviation (wrong message type for the current state, wrong `id`, invalid base64, chunk overflow,
etc.) is a **protocol violation** and must terminate the connection.

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
either:

- **For fetch-like requests** (`Fetch*` / `FetchHttpRequest` / `FetchPartial*`): as
  `NetworkToBrowser::FetchErr { id, err: IpcError }`.
  - This is used regardless of whether the body would have been inline or chunked.
  - `FetchErr` may appear as the first response frame for a request (common), and the client also
    accepts it during a chunked stream as an abort signal (defense in depth).
- **For non-fetch RPCs** (`CookieHeaderValue`, `StoreCookieFromDocument`, cache artifact ops, etc.):
  as `NetworkToBrowser::Response { id, response: <... IpcResult::Err(IpcError) ...> }`.

Note: `IpcResponse::Fetched(IpcResult::Err(_))` exists in the schema and the client accepts it, but
the current server implementation uses `FetchErr` for fetch failures.

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
- Add an explicit per-request timeout / deadline budget field so the server can enforce timeouts
  without relying solely on the HTTP client’s global timeout configuration.

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

- **Getter**: `IpcRequest::CookieHeaderValue { url }` → `IpcResponse::MaybeString(Ok(Option<String>))`
  - `Ok(Some("a=b; c=d"))` means cookies are supported and that cookie header value would be sent.
  - `Ok(Some(""))` means cookies are supported but there are no matching cookies.
  - `Ok(None)` (`null` in JSON) means the underlying fetcher does not expose cookie state *or* the
    URL was invalid/unparseable. The in-tree client (`IpcResourceFetcher`) treats `null` defensively
    as an empty cookie string for deterministic `document.cookie`.
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

---

## Operational notes (non-normative, but matches in-tree behavior)

### Socket timeouts

The renderer-side `IpcResourceFetcher` configures the underlying stream with a read/write timeout
(`IPC_IO_TIMEOUT`, currently 30 seconds) to avoid deadlocking the renderer forever if the network
process becomes unresponsive mid-request.

This is not a wire-level protocol feature, but other implementations should set conservative
timeouts and treat timeout errors like connection loss (reconnect + redo the hello handshake).

### Concurrency

Because v1 is strictly sequential on a single connection, clients that need parallelism should open
multiple independent connections, each with its own `Hello`/`HelloAck` handshake and independent
request ID space.

### JSON integer precision

All integer fields (`id`, `status`, `total_len`, `max_bytes`, …) are serialized as **JSON numbers**.

Implementations must decode these values as *integers* (not floats) and should be careful with
languages/parsers that only provide IEEE-754 doubles for JSON numbers (notably JavaScript). For such
languages, use a JSON library that supports 64-bit integers / big integers.

---

## Related protocols (do not confuse)

FastRender has multiple IPC / IPC-like protocols in-tree. This document is specifically for the
JSON `ipc_fetcher` protocol (`src/resource/ipc_fetcher.rs` + `src/ipc/network_service.rs`).

Other network-related protocols you may encounter:

- **Planned browser ↔ network schema**: `src/ipc/protocol/network.rs`
  - Has an explicit `Cancel { request_id }` message and `expected_fds()` planning for FD-backed body
    transfer.
  - Uses `#[serde(deny_unknown_fields)]` at the top-level message enums.
- **Prototype `network` subprocess protocol**: `src/network_process/ipc.rs`
  - Different framing (`u32_be` length prefix + JSON) and a smaller request surface.
- **Binary network transport** (requests/responses/events): `src/net/transport.rs`
  - Explicit per-field limits and streaming/event types for WebSockets/downloads; not the JSON
    `ipc_fetcher` protocol.
