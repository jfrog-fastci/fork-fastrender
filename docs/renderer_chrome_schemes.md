# Renderer-chrome internal URL schemes (`chrome://` and `chrome-action:`)

Renderer-chrome is the plan to render the browser UI (“chrome”) using FastRender, in a **trusted**
renderer instance that runs in the **browser process** (not the sandboxed content renderer).

This document defines two *privileged* URL schemes reserved for that trusted chrome renderer:

- `chrome://` — loads built-in UI assets (CSS/JS/images/fonts) from a trusted, allowlisted bundle.
- `chrome-action:` — pseudo-URLs embedded in trusted chrome HTML to request browser actions (new tab,
  close tab, etc).

Note: This `chrome://` scheme is **FastRender-internal**. It is unrelated to external tooling docs
that mention Google Chrome’s `chrome://tracing` trace viewer.

These schemes **must never be interpreted for untrusted web content**. Treat them as a hard
trust-boundary: if untrusted HTML/JS can trigger these, it becomes a browser-escape primitive.

Related context:
- Renderer-chrome overview: [`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md)
- Multiprocess security model: [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)
- Chrome JS bridge (trusted `globalThis.chrome` API): [`docs/chrome_js_bridge.md`](chrome_js_bridge.md)

---

## Trust boundary and current enforcement

FastRender has (or will have) two renderer contexts:

1. **Trusted chrome renderer (browser process)**: renders only repo-owned UI HTML.
2. **Untrusted content renderer (renderer/worker process)**: renders arbitrary web content.

Even before renderer-chrome lands, the codebase already enforces a “content must reject unknown
schemes” rule, which will also reject `chrome://` and `chrome-action:`:

- Scheme allowlist for navigations: [`src/ui/url.rs`](../src/ui/url.rs) (`validate_user_navigation_url_scheme`).
  - This only allows `http`, `https`, `file`, `about`.
- Content worker uses that allowlist for all non-`about:` navigations:
  [`src/ui/render_worker.rs`](../src/ui/render_worker.rs) (navigation prepare path calls
  `validate_user_navigation_url_scheme`).
- Integration test asserts “unsupported schemes fail fast (no error page render)”:
  [`tests/browser_integration/ui_worker_unsupported_scheme.rs`](../tests/browser_integration/ui_worker_unsupported_scheme.rs).
- Integration test asserts an *untrusted* page can’t click-navigate to these schemes either:
  [`tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs`](../tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs).
- Browser CLI start URL validation also uses the same allowlist:
  [`src/bin/browser.rs`](../src/bin/browser.rs) +
  [`tests/browser_integration/browser_cli_start_url_scheme.rs`](../tests/browser_integration/browser_cli_start_url_scheme.rs).
- Defense-in-depth for subresource fetches: [`src/resource.rs`](../src/resource.rs) (scheme classification and
  `ResourcePolicy::allowed_schemes`) treats unknown schemes as `Other` and blocks them by default.
- Web Storage origin classification already treats `chrome://<host>` as a **non-opaque origin**
  (persistent `localStorage`), while treating `about:`/`data:`/`file:` as opaque:
  [`src/js/web_storage.rs`](../src/js/web_storage.rs) (`origin_key_from_document_url`).

**Invariant:** The content renderer must treat `chrome://…` and `chrome-action:…` as unsupported
schemes (no navigation, no fetch, no side effects).

**Do not** “fix” `chrome://` support by adding `chrome` to the global allowlists above. If/when
`chrome://` is implemented, it must be enabled only inside the trusted chrome renderer context.

---

## `chrome://` — trusted chrome assets

### What it is

`chrome://` is a non-network, non-filesystem scheme used by *trusted chrome HTML* to reference UI
assets packaged with the browser (stylesheets, scripts, icons, etc).

Example (illustrative):

```html
<link rel="stylesheet" href="chrome://styles/chrome.css">
<img src="chrome://icons/reload.svg">
```

### Where it is allowed

Only in the **trusted browser-process chrome renderer**.

It must **not** be enabled for:
- web page navigations (typed URL, link clicks, redirects),
- subresource fetching in untrusted documents (`<img>`, `<link>`, `fetch()`, etc),
- `about:` pages as currently implemented (they are rendered through the untrusted worker pipeline).

### Resolution model

When implemented, `chrome://` URLs should be resolved as:

1. Parse the URL as `chrome://<host>/<path>`, where `<host>` is an **asset namespace** (e.g.
   `styles`, `icons`) and `<path>` is the asset path under that namespace.
2. Map `(host, path)` to a **fixed allowlisted asset key**.
3. Serve bytes from a trusted source (typically embedded via `include_bytes!` or a read-only bundle).

Implementation reference (existing pattern): repo-owned UI assets are already embedded via
`include_bytes!` in places like [`src/ui/icons.rs`](../src/ui/icons.rs) (SVGs in
[`assets/browser_icons/`](../assets/browser_icons/)). A future
`chrome://icons/...` mapping would likely reuse these bytes rather than touching the filesystem.

Security requirements for the resolver/fetcher:

- No directory traversal: reject `..`, empty segments, and ambiguous percent-encodings.
- No fallback to network or filesystem on cache miss: unknown assets must fail closed.
- Deterministic MIME types (no sniffing); no redirects; no cookies/auth.

### How it differs from `http(s)` and `file`

- Not a network request: no DNS/TLS/headers/cookies/redirects.
- Not a file request: no host filesystem access; the allowlist is the only source of bytes.
- Intended to be available offline and under aggressive network/file sandboxing.

### Origin / storage semantics

The intended `chrome://` form is host-based (`chrome://<host>/<path>`), so chrome documents have a
stable origin (`chrome://<host>`) that can be used for same-origin checks and Web Storage.

Implementation reference: `localStorage` keys are derived via
[`src/js/web_storage.rs`](../src/js/web_storage.rs) (`origin_key_from_document_url`), which already
treats `chrome://<host>` as a persistent origin (unlike `about:`/`file:` which are treated as
opaque). This only remains safe if untrusted content cannot create chrome documents (enforced by the
navigation/fetch allowlists above).

---

## `chrome-action:` — trusted chrome actions

### What it is

`chrome-action:` is **not a fetchable resource**. It is an internal “action request” encoding used
inside trusted chrome HTML to ask the browser to do something.

Example (illustrative):

```html
<a href="chrome-action:new-tab">New tab</a>
<button data-href="chrome-action:reload">Reload</button>
```

The chrome host is expected to intercept these URLs at the UI-event layer and dispatch them to
browser logic (typically by mapping to a strongly-typed enum such as `ChromeAction` in
[`src/ui/chrome.rs`](../src/ui/chrome.rs)).

### Where it is allowed

Only in **trusted chrome HTML**, rendered in the browser process.

The untrusted content renderer must never interpret this scheme; it should be rejected as an
unsupported navigation scheme (see the enforcement section above).

### Parsing/dispatch invariants

- Parsing must be strict: unknown actions must fail closed (ignore or surface a clear error), and
  must never fall back to “navigate to it anyway”.
- Arguments (if any) must be explicitly parsed/validated; no eval; no shell-like escaping rules.
- Dispatch must be reachable only from the trusted chrome renderer context.

Implementation note: in the untrusted content worker, link resolution helpers (e.g.
[`src/ui/url.rs`](../src/ui/url.rs) (`resolve_link_url`)) intentionally only special-case
`javascript:` and will happily
return absolute `chrome-action:...` URLs. This is safe because the later navigation stage enforces
`validate_user_navigation_url_scheme` and rejects unsupported schemes. In the *trusted* chrome
renderer, you would instead intercept `chrome-action:` before attempting navigation.

---

## Guidance: adding new assets/actions

### Adding a new `chrome://` asset

1. Add the bytes to the trusted asset bundle (embedded or otherwise trusted).
2. Register a stable URL in the `chrome://` allowlist/registry (avoid breaking existing URLs).
3. Assign a deterministic MIME type.
4. Add unit tests for:
   - URL → asset key mapping (including traversal/canonicalization attacks)
   - fetch success for known assets; failure for unknown assets

### Adding a new `chrome-action:` action

1. Add a new strongly-typed action variant (e.g. in [`src/ui/chrome.rs`](../src/ui/chrome.rs) or a dedicated registry).
2. Implement the handler in the chrome host (browser process).
3. Ensure untrusted contexts cannot trigger it (scheme must remain unsupported there).
4. Add unit tests for parsing and dispatch (including invalid/unknown actions).

---

## Testing strategy

**Unit tests (scheme-specific):**

- Parser tests: `chrome://` URL parsing + normalization; `chrome-action:` parsing.
- Fetcher tests: `chrome://` fetch returns correct bytes and MIME type; rejects unknown assets and
  traversal attempts.

**Integration tests (content rejection):**

- [`tests/browser_integration/ui_worker_unsupported_scheme.rs`](../tests/browser_integration/ui_worker_unsupported_scheme.rs) asserts the untrusted content worker
  rejects `chrome://…` and `chrome-action:…` (as well as other unsupported schemes like
  `javascript:`). The expected behavior for untrusted content is:
  - `WorkerToUi::NavigationFailed { .. }`
  - no `WorkerToUi::FrameReady { .. }` for the failed navigation.

  To run just this check:

  ```bash
  # Note: `tests/browser_integration/...` is compiled into the unified integration test binary.
  bash scripts/cargo_agent.sh test --features browser_ui --test integration \
    ui_worker_rejects_unsupported_schemes_without_rendering_error_page
  ```

- [`tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs`](../tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs) asserts an untrusted
  page cannot *click* a link to `chrome-action:` or `chrome://` (same rejection behavior).
