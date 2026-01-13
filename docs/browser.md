# Desktop browser (`browser` binary)

FastRender includes an experimental cross-platform desktop “browser” app: a small windowed shell
(chrome) that hosts the renderer and displays the resulting framebuffer.

Code lives in:

- Entry point: [`src/bin/browser.rs`](../src/bin/browser.rs)
- Shared browser UI helpers: [`src/ui/`](../src/ui/)

## What this is / is not

**This is:**

- A cross-platform *desktop* app (Linux/macOS/Windows) built on `winit + wgpu + egui`.
- A host for the FastRender HTML/CSS renderer, displaying a rendered `tiny_skia::Pixmap` inside the
  window.
- An experimental foundation for an interactive surface over the renderer (no JS required).

**This is not:**

- A “real” web browser engine (still missing many pieces: sandboxed multiprocess architecture,
  extensions/devtools/service workers, etc.).
  - Multiprocess isolation (renderer/network separation, sandboxing) is tracked in
    [`instructions/multiprocess_security.md`](../instructions/multiprocess_security.md) and
    [`docs/network_process.md`](network_process.md) and should be treated as security-critical and
    still-evolving.
- A fully JavaScript-capable browser: author JS execution in the windowed UI is still experimental
  and incomplete. The UI worker currently maintains a JS-capable `api::BrowserTab` and best-effort
  syncs its `dom2` snapshot into the rendered document before painting, but many Web APIs and
  web-compat behaviors are still missing. See [javascript.md](javascript.md),
  [html_script_processing.md](html_script_processing.md), and [runtime_stacks.md](runtime_stacks.md)
  for context on the JS stacks and containers.
  - CLI note: JavaScript is currently enabled by default in the windowed UI (there is no stable CLI
    toggle to disable it yet). `browser --js` is currently only meaningful in `--headless-smoke`
    mode, where `browser --headless-smoke --js` selects a vm-js `BrowserTab` smoke test.
- A renderer-chrome browser UI (yet): the `browser` chrome is currently rendered via egui. The
  renderer-chrome workstream aims to render the chrome UI using FastRender; trusted chrome pages
  would then use the privileged JS bridge documented in [chrome_js_bridge.md](chrome_js_bridge.md).
  Privileged internal URL schemes used by renderer-chrome (`chrome://` assets, `chrome-action:`
  actions) are documented in [renderer_chrome_schemes.md](renderer_chrome_schemes.md).

## Build / run

The `browser` binary is feature-gated behind the Cargo feature `browser_ui` so the core renderer
can compile without pulling in the GUI stack.

For build/run commands, platform prerequisites, and MSRV constraints, see [browser_ui.md](browser_ui.md).

### Headless smoke / crash-smoke (CI + multiprocess seam)

The `browser` entrypoint also has **headless** smoke modes intended for CI and quick validation on
hosts without a working display/GPU (they do not create a window or initialise `winit`/`wgpu`):

```bash
# Basic “is UI↔worker wired up” smoke test:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke

# JS smoke test (vm-js `BrowserTab` execution path):
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke --js

# “renderer crash shouldn’t take down the browser” smoke test:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-crash-smoke
```

What these validate:

- **`--headless-smoke`**: end-to-end UI↔worker startup and message wiring.
  - With `--js`: runs a vm-js `api::BrowserTab` smoke test instead (prints `HEADLESS_VMJS_SMOKE_OK` on
    success).
- **`--headless-crash-smoke`**: crash isolation (today: worker thread crash; future: renderer process crash).

In the intended multiprocess architecture, a renderer crash should **not** take down the whole
browser process: the chrome (and other tabs) should stay responsive, and the affected tab should be
marked as crashed and show a deterministic “tab crashed” overlay (typically with a Reload action).
See [site_isolation.md](site_isolation.md) for the design context.

These smoke modes are intended to remain stable as the renderer moves out-of-process. The key seam
is the `UiToWorker`/`WorkerToUi` message protocol in [`src/ui/messages.rs`](../src/ui/messages.rs) and
the worker spawn helpers (`spawn_browser_worker` / `spawn_browser_ui_worker`) in
[`src/ui/render_worker.rs`](../src/ui/render_worker.rs).

## Current capabilities (MVP)

The `browser` UI is intentionally minimal, but the core chrome/navigation loop is now wired up
end-to-end:

- **Tabs**: create/close/switch tabs.
- **Windows**: open multiple windows (<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>N</kbd>).
- **Menu bar**: browser-style menu bar (File/Edit/View/History/Bookmarks/Window/Help) for discoverability.
- **Accessibility (chrome)**: the egui-based chrome UI exposes widget semantics to OS assistive tech
  via AccessKit (VoiceOver/Narrator/Orca). See [chrome_accessibility.md](chrome_accessibility.md).
- **Accessibility (page content)**: the renderer can compute a page accessibility tree
  (roles/names/states) as JSON via `dump_a11y` (see [page_accessibility.md](page_accessibility.md)).
  The render worker also emits a live `WorkerToUi::PageAccessibility` snapshot (tree + best-effort
  bounds) that the UI stores for future OS-facing subtree injection. Wiring per-element page content
  into the OS-facing AccessKit tree is still in progress; today the windowed UI typically exposes the
  rendered page primarily as a single labeled region (the pixmap).
- **Navigation**:
  - address bar URL entry (press Enter to navigate; user input is normalized, e.g. `example.com`
    → `https://example.com/`, filesystem paths → `file://...`)
  - per-tab history with back/forward/reload
  - loading + error status in the chrome
- **Bookmarks**: bookmark the current page (star button / Ctrl+D on Win/Linux, Cmd+D on macOS), bookmarks bar, bookmarks side panel, bookmarks manager.
- **History**: global history panel with search + “Clear browsing data” (time range).
- **Find in page**: Ctrl/Cmd+F opens a find bar and highlights matches in the current tab.
- **Downloads**: “Download link/image” from the page context menu; view progress/cancel/retry/open from the downloads side panel.
- **Scrolling**: mouse wheel / trackpad scroll updates the viewport scroll offset and repaints.
- **JavaScript (experimental)**: the windowed UI worker runs a JS-capable tab runtime (vm-js), so
  `<script>` can run during navigation and trigger repaints via DOM mutations; time-based updates use
  the same tick loop described in [browser_ui.md](browser_ui.md).
  - Note: there is currently no stable CLI toggle to disable JS in windowed mode; `browser --js` is
    used for the `--headless-smoke` vm-js smoke test path only.
- **Pointer/keyboard routing**:
  - link clicking (`<a href=...>`) navigates
  - click to focus and type into basic text inputs / textareas
  - pointer toggles for checkboxes / radios

Startup note:

- When run **without** a URL, the windowed `browser` app tries to restore the previous session
  (windows + tabs + per-tab zoom + best-effort scroll restoration).
- If the previous run ended unexpectedly (unclean exit) **and the session is restored**, the UI shows
  a crash-recovery infobar/toast on startup (including a **Start new session** option).
  - If repeated unclean exits are detected (crash loop), the browser may skip auto-restoring tabs and
    start with a “safe” `about:newtab` instead. Use `--restore` to force restoring anyway.
- When run **with** a URL, it opens that URL and does not restore tabs unless `--restore` is provided.
  - Even when tabs are not restored (CLI URL or `--no-restore`), the browser may still reuse persisted
    **configuration** from the previous session (appearance/UI scale, home page, menu bar visibility,
    and window geometry) when available.
- If the primary session file is corrupted/unparseable, the browser can fall back to a retained
  last-known-good backup (same filename with a `.bak` suffix, e.g. `fastrender_session.json.bak`).
- If no session exists yet, it falls back to `about:newtab`, which acts as a basic start page
  (showing bookmarks + recently visited pages when available). Use `--no-restore` to disable tab
  restore.

### DOM interaction (host-driven; works without JS)

FastRender also has a small DOM interaction layer intended to support basic browsing even without
JS (and still used heavily by the browser UI for hit-testing and form interactions):

- hit-testing + link activation (`<a href=...>`, including same-document `#fragment` scrolling)
- basic form interactions (text inputs, checkboxes, radios, select controls, file inputs, date/time
  inputs; limited keyboard activation via `Enter`/`Space`)
- built-in `about:*` pages (see [about_pages.md](about_pages.md)): `about:newtab`, `about:settings`,
  `about:blank`, `about:error`, `about:help`, `about:version`, `about:gpu`, `about:processes`,
  `about:history`, `about:bookmarks`
  - `about:processes` is a multiprocess/process-assignment debugging page: today it shows a
    best-effort open-tabs snapshot and a derived Site column; future work will surface real
    renderer/network process assignment.

These interactions are exercised by the headless UI worker integration tests; the windowed `browser`
app uses the same `UiToWorker`/`WorkerToUi` message protocol via the browser UI worker thread
([`spawn_browser_ui_worker`](../src/ui/render_worker.rs), a wrapper around
`spawn_browser_worker`), so link clicking and basic form interactions work in the GUI as well.

### Still incomplete (non-exhaustive)

- select dropdown UI is basic (keyboard navigation and simple typeahead are supported; no multi-select yet)
- richer text editing + selection/caret movement
- full focus traversal + keyboard activation parity

See [browser_ui.md](browser_ui.md) for implementation details and current status.
## Environment variables / resource limits

Browser-related environment variables live in [env-vars.md](env-vars.md) (see “Browser UI (`browser`
binary)”). Notably:

- `FASTR_BROWSER_MEM_LIMIT_MB=<MiB>` – best-effort address-space (virtual memory) limit for the
  `browser` process. This is applied at process start (and may be unsupported on some platforms).
- `FASTR_BROWSER_MAX_PIXELS`, `FASTR_BROWSER_MAX_DIM_PX`, `FASTR_BROWSER_MAX_DPR` – hard safety
  limits for viewport/DPR to prevent huge in-process pixmap allocations when the window is resized
  to extreme dimensions or when the display has a very high DPI scale.
- `FASTR_BROWSER_WGPU_FALLBACK=1` / `browser --force-fallback-adapter` (alias: `--wgpu-fallback`) – force
  `wgpu` to use a fallback (software) adapter when selecting a GPU for the windowed UI.
- `FASTR_BROWSER_WGPU_BACKENDS=...` / `browser --wgpu-backends ...` (alias: `--wgpu-backend`) – restrict
  the `wgpu` backend set (for example `gl`) when the default backend selection fails.
- `FASTR_PERF_LOG=1` – enable JSONL responsiveness logging for the windowed UI (frame times, input
  latency, TTFP, CPU usage). See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness).
- `FASTR_PERF_LOG_OUT=/path/to/log.jsonl` – optional output path for `FASTR_PERF_LOG` events (when unset,
  logs are written to stdout so they can be piped/tee'd).

When running against arbitrary real-world pages, consider using the repo’s resource limit wrapper
(see [browser_ui.md](browser_ui.md)).

## Sandboxing

FastRender is moving toward a multiprocess architecture where untrusted page content runs in a
separate OS-sandboxed renderer process.

Windows sandboxing details (AppContainer + Job Objects + restricted-token fallback) and macOS
Seatbelt notes (profiles + debugging) live in [sandboxing.md](sandboxing.md).

Debugging note: you can temporarily disable the renderer OS sandbox with
`FASTR_DISABLE_RENDERER_SANDBOX=1` (platform aliases: `FASTR_WINDOWS_RENDERER_SANDBOX=off`,
`FASTR_RENDERER_SANDBOX=off`, `FASTR_MACOS_RENDERER_SANDBOX=off`). This is **insecure** and prints a warning to stderr; see
[sandboxing.md](sandboxing.md) for details.

To debug Windows sandbox spawn failures, set `FASTR_LOG_SANDBOX=1` for verbose sandbox logs.
If you need the sandboxed child to inherit the full parent environment on Windows (disabling the
default environment sanitization / `TEMP`/`TMP` override), set `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1`
(debug only).

## Implementation notes

For implementation details (code layout, message protocol, cancellation, platform prerequisites),
see [browser_ui.md](browser_ui.md).

## Debugging tips

### Renderer debugging knobs

Most renderer debug knobs are environment variables; the canonical list is
[env-vars.md](env-vars.md). A few commonly useful ones while developing the browser UI:

- `FASTR_RENDER_TIMINGS=1` – log per-stage timings to stderr.
- `FASTR_TRACE_OUT=/tmp/trace.json` – write Chrome trace events for a render.
- `FASTR_TRACE_MAX_EVENTS=<N>` – cap the number of trace events retained per render (default 200000).
- `FASTR_PAINT_BACKEND=display_list|legacy` – switch paint backend.

### Browser responsiveness tooling

To capture machine-readable browser responsiveness metrics:

```bash
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl

# Wrapper-friendly helper (sets env vars for you):
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release --hud \
  --perf-log --perf-log-out target/browser_perf.jsonl \
  about:test-layout-stress

# Manual invocation:
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- about:test-layout-stress
```

Summarize a captured log with:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- \
  --input target/browser_perf.jsonl
```

(`timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --summary ...` runs the summary tool automatically after the
browser exits.)

For automated, headless measurements (JSON summary):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
```

For interactive CPU attribution while reproducing jank (Linux), use the Samply wrapper:

```bash
timeout -k 10 600 bash scripts/profile_browser_samply.sh --url about:test-layout-stress
```

See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness) for how the
metrics map to the workstream targets.

### Where to look for logs

Most `browser` output is written to stdout/stderr (run it from a terminal). The main exceptions are
the optional file outputs:

- `FASTR_PERF_LOG_OUT=/path/to/log.jsonl` (responsiveness JSONL)
- `FASTR_BROWSER_TRACE_OUT=/path/to/trace.json` (Perfetto/Chrome trace)

If the window opens but nothing renders, check for:

- `wgpu` surface errors printed by [`src/bin/browser.rs`](../src/bin/browser.rs)
- renderer debug output enabled via `FASTR_*` env vars

For panics, use:

```bash
RUST_BACKTRACE=1 timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```
