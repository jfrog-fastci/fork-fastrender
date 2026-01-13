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

- A “real” web browser engine (no sandboxed multi-process architecture yet, no
  extensions/devtools/service workers, etc.).
- A JavaScript-capable browser (yet): the `browser` binary does not currently execute author
  JavaScript (`<script>` does not run in the GUI today). See [javascript.md](javascript.md) and
  [html_script_processing.md](html_script_processing.md) for the in-tree JS workstream.
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
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke

# “renderer crash shouldn’t take down the browser” smoke test:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-crash-smoke
```

What these validate:

- **`--headless-smoke`**: end-to-end UI↔worker startup and message wiring.
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
- **Pointer/keyboard routing**:
  - link clicking (`<a href=...>`) navigates
  - click to focus and type into basic text inputs / textareas
  - pointer toggles for checkboxes / radios

Startup note:

- When run **without** a URL, the windowed `browser` app tries to restore the previous session
  (windows + tabs + per-tab zoom + best-effort scroll restoration).
- When run **with** a URL, it opens that URL and does not restore unless `--restore` is provided.
- If no session exists yet, it falls back to `about:newtab`, which acts as a basic start page
  (showing bookmarks + recently visited pages when available). Use `--no-restore` to disable session
  restore.

### DOM interaction (non-JS)

FastRender also has a small DOM interaction layer intended to support basic “no-JS” browsing:

- hit-testing + link activation (`<a href=...>`, including same-document `#fragment` scrolling)
- basic form interactions (text inputs, checkboxes, radios, select controls, file inputs, date/time
  inputs; limited keyboard activation via `Enter`/`Space`)
- built-in `about:*` pages (`about:newtab`, `about:blank`, `about:error`, `about:help`,
  `about:version`, `about:gpu`, `about:history`, `about:bookmarks`)

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
  latency, TTFP). See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness).

When running against arbitrary real-world pages, consider using the repo’s resource limit wrapper
(see [browser_ui.md](browser_ui.md)).

## Sandboxing

FastRender is moving toward a multiprocess architecture where untrusted page content runs in a
separate OS-sandboxed renderer process.

Windows sandboxing details (AppContainer + Job Objects + restricted-token fallback) and the
Windows-only debug escape hatch live in [sandboxing.md](sandboxing.md).

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
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

For automated, headless measurements (JSON summary):

```bash
bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
```

See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness) for how the
metrics map to the workstream targets.

### Where to look for logs

The `browser` binary currently logs to stdout/stderr only (run it from a terminal). If the window
opens but nothing renders, check for:

- `wgpu` surface errors printed by [`src/bin/browser.rs`](../src/bin/browser.rs)
- renderer debug output enabled via `FASTR_*` env vars

For panics, use:

```bash
RUST_BACKTRACE=1 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```
