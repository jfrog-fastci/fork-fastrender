# Renderer-chrome internal URL schemes (`chrome://`, `chrome-action:`, and `chrome-dialog:`)

Renderer-chrome is the plan to render the browser UI (“chrome”) using FastRender, in a **trusted**
renderer instance that runs in the **browser process** (not the sandboxed content renderer).

This document defines three *privileged* URL schemes reserved for that trusted chrome renderer:

- `chrome://` — loads built-in UI assets (CSS/JS/images/fonts) from a trusted, allowlisted bundle.
- `chrome-action:` — pseudo-URLs embedded in trusted chrome HTML to request browser actions (new tab,
  close tab, etc).
- `chrome-dialog:` — pseudo-URLs embedded in trusted chrome HTML to report modal/dialog "button
  result" actions (accept/cancel) back to the chrome host.

Note: This `chrome://` scheme is **FastRender-internal**. It is unrelated to external tooling docs
that mention Google Chrome’s `chrome://tracing` trace viewer.

These schemes **must never be interpreted for untrusted web content**. Treat them as a hard
trust-boundary: if untrusted HTML/JS can trigger these, it becomes a browser-escape primitive.

Related context:
- Renderer-chrome overview: [`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md)
- Multiprocess security model: [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md)
- Chrome JS bridge (trusted `globalThis.chrome` API): [`docs/chrome_js_bridge.md`](chrome_js_bridge.md)
- No-JS chrome interaction roadmap (`chrome-action:` links/forms): [`docs/renderer_chrome_non_js.md`](renderer_chrome_non_js.md)

---

## Trust boundary and current enforcement

FastRender has (or will have) two renderer contexts:

1. **Trusted chrome renderer (browser process)**: renders only repo-owned UI HTML.
2. **Untrusted content renderer (renderer/worker process)**: renders arbitrary web content.

Even before renderer-chrome lands, the codebase already enforces a “content must reject unknown
schemes” rule, which will also reject `chrome://`, `chrome-action:`, and `chrome-dialog:`:

- Scheme allowlist for navigations: [`src/ui/url.rs`](../src/ui/url.rs) (`validate_user_navigation_url_scheme`).
  - This only allows `http`, `https`, `file`, `about`.
  - Unit test ensures privileged `chrome://` / `chrome-action:` / `chrome-dialog:` remain rejected:
    `user_navigation_scheme_validation_rejects_privileged_renderer_chrome_schemes` in
    [`src/ui/url.rs`](../src/ui/url.rs).
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

**Invariant (untrusted content):** Untrusted web content must treat `chrome://…`,
`chrome-action:…`, and `chrome-dialog:…` as unsupported schemes (no navigation, no fetch, no side
effects).

**Important exception (internal `about:` pages):** The UI worker installs an origin-gated fetcher
(`AboutPagesCompositeFetcher` in [`src/ui/about_pages_fetcher.rs`](../src/ui/about_pages_fetcher.rs))
that allows `about:` pages to load a small allowlisted set of shared `chrome://` assets. Today this
is used for the shared `about:` page stylesheet:

- `chrome://styles/about.css` (see `ABOUT_SHARED_CSS_URL` in [`src/ui/about_pages.rs`](../src/ui/about_pages.rs))

This does **not** mean `chrome://` is generally enabled for untrusted documents: non-`about:`
origins are rejected, and unknown `chrome://` assets fail closed.

**Do not** “fix” `chrome://` support by adding `chrome` to the global allowlists above. If/when
`chrome://` is implemented, it must be enabled only inside the trusted chrome renderer context.

Likewise, **do not** add `chrome-action` or `chrome-dialog` to navigation allowlists.
`chrome-action:` / `chrome-dialog:` are not fetchable schemes and must be intercepted/handled only
inside the trusted chrome renderer context.

This also includes **subresource fetching** allowlists/policies (e.g. `ResourcePolicy::allowed_schemes`
in [`src/resource.rs`](../src/resource.rs)): `chrome://` support should be implemented as a
trusted-only fetch path, not as a globally-allowed URL scheme.

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

### Dynamic chrome assets: tab favicons

Renderer-chrome also uses a small set of **dynamic** (in-memory) `chrome://` resources that are
generated by the browser UI at runtime.

Currently supported:

- `chrome://favicon/<tab_id>` — tab favicon PNG for the given tab id.

Notes / invariants:

- Canonical form is **no extension**: `chrome://favicon/123`
  - The legacy form `chrome://favicons/123.png` is still accepted by the fetcher for backwards
    compatibility, but should not be used by new chrome HTML.
- MIME type: `image/png`
- Missing favicon entries return a tiny 1×1 transparent PNG (to avoid a broken-image icon in chrome
  UI while a page is still loading a favicon).
- This is served by [`ChromeDynamicAssetFetcher`](../src/ui/chrome_dynamic_asset_fetcher.rs), which is
  expected to wrap the static allowlisted bundle fetcher (`ChromeAssetsFetcher`) in trusted chrome
  contexts.

### Where it is allowed

Only in the **trusted browser-process chrome renderer**.

It must **not** be enabled for:
- web page navigations (typed URL, link clicks, redirects),
- subresource fetching in untrusted documents (`<img>`, `<link>`, `fetch()`, etc),
- navigations to `chrome://...` (even from internal pages; `chrome://` is not a user-facing URL scheme).

Current implementation note: internal `about:` pages are allowed to load a small allowlisted subset
of `chrome://` assets (currently just the shared stylesheet `chrome://styles/about.css`). This is
enforced by origin checks in `AboutPagesCompositeFetcher` and is intended purely for offline UI
styling, not for general `chrome://` support.

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
- Reject or ignore query/fragment deterministically (prefer rejecting `?` / `#` entirely). Asset
  identity should be based only on the allowlisted `(host, path)` pair.
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

The vm-js `Window` realm also uses a chrome-aware origin serializer for `document.origin` /
`location.origin`: [`src/js/vmjs/window_realm.rs`](../src/js/vmjs/window_realm.rs)
(`serialized_origin_for_document_url`). It treats trusted `chrome://<host>` documents as a tuple
origin so internal pages can safely use same-document APIs like `history.pushState` without
collapsing all internal pages into the opaque `"null"` origin bucket.

Secure-context note (JS): the current `isSecureContext` computation in
[`src/js/vmjs/window_realm.rs`](../src/js/vmjs/window_realm.rs) (`is_secure_context_for_document_url`)
treats only HTTPS (and HTTP localhost) as secure, so `chrome://` pages are currently *not* secure
contexts. If renderer-chrome pages need secure-context-only APIs in the future, this policy should
be revisited (but only for **trusted** chrome pages).

---

## `chrome-action:` — trusted chrome actions

### What it is

`chrome-action:` is **not a fetchable resource**. It is an internal “action request” encoding used
inside trusted chrome HTML to ask the browser to do something.

This is separate from the privileged JS bridge (`globalThis.chrome`, see
[`docs/chrome_js_bridge.md`](chrome_js_bridge.md)):

- `chrome-action:` is useful for simple, declarative “command links/buttons” (e.g. `<a href=...>`),
  even on pages that want minimal JS.
- The JS bridge is for richer chrome UI logic (stateful UI, async operations, etc).

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
- Prefer a narrow grammar such as `chrome-action:<action>` (no `//` authority form, no redirects).
- Arguments (if any) must be explicitly parsed/validated; no eval; no shell-like escaping rules.
- Dispatch must be reachable only from the trusted chrome renderer context.

Implementation note: in the untrusted content worker, link resolution helpers (e.g.
[`src/ui/url.rs`](../src/ui/url.rs) (`resolve_link_url`)) intentionally only special-case
`javascript:` and will happily
return absolute `chrome-action:...` URLs. This is safe because the later navigation stage enforces
`validate_user_navigation_url_scheme` and rejects unsupported schemes. In the *trusted* chrome
renderer, you would instead intercept `chrome-action:` before attempting navigation.

---

## `chrome-dialog:` — trusted dialog result actions

### What it is

`chrome-dialog:` is **not a fetchable resource**. It is a narrow internal encoding used by trusted
chrome HTML for modal dialogs (alert/confirm/prompt) to report a button result back to the chrome
host.

Canonical examples:

```text
chrome-dialog:accept
chrome-dialog:cancel
```

Dialog submissions may include a query string payload (for example, prompt text submitted via
`method=get`), but the action name itself must remain unambiguous and strongly typed:

```text
chrome-dialog:accept?value=hello
```

### Where it is allowed

Only in **trusted chrome HTML**, rendered in the browser process.

Untrusted content must treat `chrome-dialog:` as an unsupported scheme (same trust boundary as
`chrome://` and `chrome-action:`).

### Parsing/dispatch invariants

- Parsing must be strict: accept only known actions (`accept`, `cancel`); fail closed otherwise.
- Prefer the opaque form `chrome-dialog:<action>` and reject `chrome-dialog://...` authority forms.
- Dispatch must be reachable only from the trusted chrome renderer context.

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

- Parser tests: `chrome://` URL parsing + normalization; `chrome-action:` parsing; `chrome-dialog:`
  parsing.
- Fetcher tests: `chrome://` fetch returns correct bytes and MIME type; rejects unknown assets and
  traversal attempts.

**Integration tests (content rejection):**

- [`tests/browser_integration/ui_worker_unsupported_scheme.rs`](../tests/browser_integration/ui_worker_unsupported_scheme.rs) asserts the untrusted content worker
  rejects `chrome://…`, `chrome-action:…`, and `chrome-dialog:…` (as well as other unsupported schemes like
  `javascript:`). The expected behavior for untrusted content is:
  - `WorkerToUi::NavigationFailed { .. }`
  - no `WorkerToUi::FrameReady { .. }` for the failed navigation.

  To run just this check:

  ```bash
  # Note: `tests/browser_integration/...` is compiled into the unified integration test binary.
  timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --features browser_ui --test integration \
    ui_worker_rejects_unsupported_schemes_without_rendering_error_page
  ```

- [`tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs`](../tests/browser_integration/ui_worker_untrusted_chrome_schemes.rs) asserts an untrusted
  page cannot *click* a link to `chrome-action:`, `chrome-dialog:`, or `chrome://` (same rejection
  behavior).
