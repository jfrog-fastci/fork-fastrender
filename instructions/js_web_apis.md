# Workstream: JavaScript Web APIs

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

This workstream owns **Web Platform APIs** beyond the core DOM: fetch, URL, timers, storage, encoding, crypto, and other browser APIs.

## The job

Implement the **Web APIs that real websites depend on**. Not obscure APIs. The ones that break pages when missing.

## What counts

A change counts if it lands at least one of:

- **API implementation**: A missing Web API is implemented.
- **Spec compliance**: An API now matches WHATWG/W3C specs.
- **Bug fix**: An API that behaved incorrectly now works.
- **Unblocks sites**: A popular site's script now works due to this API.

## Scope

### Owned by this workstream

**Fetch API (WHATWG Fetch Standard):**
- `fetch()` function
- `Request` class
- `Response` class
- `Headers` class
- `Body` mixin (text, json, blob, arrayBuffer, formData)
- Streaming bodies (ReadableStream)
- CORS and credentials

**URL API (WHATWG URL Standard):**
- `URL` class
- `URLSearchParams` class
- URL parsing and serialization

**Timers (HTML Standard):**
- `setTimeout()`, `clearTimeout()`
- `setInterval()`, `clearInterval()`
- `queueMicrotask()`
- `requestAnimationFrame()`, `cancelAnimationFrame()`
- `requestIdleCallback()` (lower priority)

**Encoding API (Encoding Standard):**
- `TextEncoder`
- `TextDecoder`

**Console API (Console Standard):**
- `console.log()`, `.warn()`, `.error()`, `.info()`, `.debug()`
- `console.dir()`, `.table()`
- `console.time()`, `.timeEnd()`
- `console.assert()`, `.count()`
- Format string substitutions (`%s`, `%d`/`%i`/`%f`, `%o`/`%O`, `%%`, `%c`)
  - In FastRender, `console.*` calls are routed to a host-provided `ConsoleSink` which receives raw
    JS arguments.
  - Default sinks must format using `vm_error_format::format_console_arguments_limited` (bounded,
    deterministic, avoids invoking user-defined `toString` hooks).

**Crypto API (Web Cryptography API):**
- `crypto.getRandomValues()`
- `crypto.randomUUID()`
- `crypto.subtle` (lower priority)

**Storage APIs:**
- `localStorage`
- `sessionStorage`
- `Storage` interface

**History API:**
- `history.pushState()`, `history.replaceState()`
- `history.back()`, `history.forward()`, `history.go()`
- `popstate` event
- `location` object

**Other common APIs:**
- `Blob`, `File`, `FileReader`
- `FormData`
- `AbortController`, `AbortSignal`
- `atob()`, `btoa()`
- `structuredClone()`
- `JSON` (native, already in engine)
- `navigator` object (userAgent, language, etc.)
- `performance.now()`
- `matchMedia()` (media queries from JS)

### NOT owned (see other workstreams)

- DOM APIs (document, element, events) → `js_dom.md`
- JavaScript language features → `js_engine.md`
- Script loading (<script>, modules) → `js_html_integration.md`

For the consolidated WebIDL crate layout and ownership boundaries (what belongs in `vendor/ecma-rs/`
vs `src/js/`), see [`docs/webidl_stack.md`](../docs/webidl_stack.md).

## Priority order (P0 → P1 → P2)

### P0: Critical APIs (pages completely break without these)

1. **Timers**
   - `setTimeout(fn, delay)` — basic timer
   - `clearTimeout(id)` — cancel timer
   - `setInterval(fn, delay)` — repeating timer
   - `clearInterval(id)` — cancel interval
   - Correct timing behavior (not faster than 4ms for nested)
   - Integration with event loop

2. **URL**
   - `new URL(url, base)` — URL parsing
   - URL properties: `href`, `origin`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`
   - URL setters
   - `url.searchParams` → `URLSearchParams`
   - `URLSearchParams`: `get`, `set`, `append`, `delete`, `has`, `entries`, `toString`

3. **Console**
   - `console.log(...args)` — basic logging
   - `console.warn(...)`, `console.error(...)` — log levels
   - Format string support (`%s`, `%d`, `%o`, etc.)
   - Output to browser devtools/stderr

4. **Encoding**
   - `new TextEncoder()` — UTF-8 encoder
   - `encoder.encode(string)` → Uint8Array
   - `new TextDecoder(encoding)` — decoder
   - `decoder.decode(buffer)` → string

### P1: Common APIs (many sites use these)

5. **Fetch**
   - `fetch(url)` — basic GET
   - `fetch(url, { method, headers, body })` — full options
   - `response.ok`, `response.status`, `response.statusText`
   - `response.headers`
   - `response.text()`, `response.json()`, `response.blob()`, `response.arrayBuffer()`
   - Error handling (network errors, aborts)

6. **Request/Response/Headers**
   - `new Request(url, init)`
   - `new Response(body, init)`
   - `new Headers(init)`
   - Headers iteration

7. **Storage**
   - `localStorage.getItem(key)`, `.setItem(key, value)`, `.removeItem(key)`, `.clear()`
   - `sessionStorage` (same interface)
   - `storage` event (cross-tab communication)

8. **History/Location**
   - `location.href`, `.pathname`, `.search`, `.hash`
   - `location.assign()`, `.replace()`, `.reload()`
   - `history.pushState(state, title, url)`
   - `history.replaceState(state, title, url)`
   - `popstate` event

9. **AbortController**
   - `new AbortController()`
   - `controller.signal`
   - `controller.abort()`
   - Integration with fetch

10. **Crypto (basic)**
    - `crypto.getRandomValues(array)`
    - `crypto.randomUUID()`

### P2: Helpful APIs (nice to have)

11. **requestAnimationFrame**
    - `requestAnimationFrame(callback)`
    - `cancelAnimationFrame(id)`
    - Proper timing (vsync-aligned if possible)

12. **Blob/File**
    - `new Blob(parts, options)`
    - `blob.size`, `blob.type`
    - `blob.text()`, `blob.arrayBuffer()`, `blob.slice()`
    - `new File(parts, name, options)`

13. **FormData**
    - `new FormData(form?)`
    - `formData.append()`, `.set()`, `.get()`, `.getAll()`, `.delete()`, `.has()`
    - Integration with fetch body

14. **performance**
    - `performance.now()` — high-resolution time
    - `performance.timing` (navigation timing)

15. **navigator**
    - `navigator.userAgent`
    - `navigator.language`, `navigator.languages`
    - `navigator.onLine`
    - `navigator.clipboard` (lower priority)

16. **matchMedia**
    - `matchMedia(query)` — returns MediaQueryList
    - `mediaQueryList.matches`
    - `mediaQueryList.addEventListener('change', ...)`

### P3: Advanced APIs

17. **Fetch streaming**
    - `response.body` (ReadableStream)
    - Stream reading and piping

18. **IndexedDB** (complex, lower priority)

19. **WebSocket** (network feature)

20. **Worker** (multi-threading, complex)

## Implementation notes

### Architecture

```
src/js/                       — JS host integration
  fetch.rs                    — Fetch API implementation
  url.rs                      — URL API implementation
  time.rs                     — Timer utilities
  vmjs/
    window_fetch.rs           — Fetch bindings for vm-js
    window_url.rs             — URL bindings for vm-js
    window_timers.rs          — Timer bindings for vm-js
    window_text_encoding.rs   — Encoding bindings
    window_crypto.rs          — Crypto bindings
    window_blob.rs            — Blob bindings
    window_form_data.rs       — FormData bindings
```

### Timer integration

Timers must integrate with the HTML event loop:

```rust
// Timers are tasks in the event loop
event_loop.queue_task(Task::Timer { callback, delay });

// The event loop drains timers when their delay has passed
impl EventLoop {
    fn run_until_idle(&mut self) {
        // Process ready timers as tasks
    }
}
```

See `src/js/event_loop.rs` for the event loop implementation.

### Fetch integration

Fetch uses the existing resource loader:

```rust
// src/js/fetch.rs bridges JS fetch() to the Rust resource system
pub async fn fetch_impl(request: Request) -> Result<Response> {
    // Use fastrender::resource for actual fetching
}
```

### Testing

```bash
# Run Web API tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::fetch
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::url

# WPT tests for specific APIs
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom --filter "url"
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom --filter "fetch"
```

## Success criteria

Web APIs are **done** when:
- All P0/P1 APIs are implemented and spec-compliant
- Popular JavaScript libraries (React, Vue, jQuery) can use these APIs
- Fetch can make real HTTP requests and handle responses
- Timers work correctly with expected timing behavior
- Storage persists data across page loads
- History navigation works with `pushState`/`popstate`
