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

- A “real” web browser engine (no multi-process architecture, no extensions/devtools/service
  workers, etc.).
- A JavaScript-capable browser: there is currently **no author JS engine** and `<script>` does not
  execute. (See [docs/javascript.md](javascript.md) for the separate JS workstream.)

## Build / run

The `browser` binary is feature-gated behind the Cargo feature `browser_ui` so the core renderer
can compile without pulling in the GUI stack.

For build/run commands, platform prerequisites, and MSRV constraints, see [browser_ui.md](browser_ui.md).

## Current capabilities (MVP)

The `browser` UI is intentionally minimal, but the core chrome/navigation loop is now wired up
end-to-end:

- **Tabs**: create/close/switch tabs.
- **Navigation**:
  - address bar URL entry (press Enter to navigate; user input is normalized, e.g. `example.com`
    → `https://example.com/`, filesystem paths → `file://...`)
  - per-tab history with back/forward/reload
  - loading + error status in the chrome
- **Scrolling**: mouse wheel / trackpad scroll updates the viewport scroll offset and repaints.
- **Page input routing**: pointer + keyboard events over the page area are forwarded to the render
  worker.

### DOM interaction (non-JS)

FastRender also has a small DOM interaction layer intended to support basic “no-JS” browsing:

- hit-testing + link activation (`<a href=...>`, including same-document `#fragment` scrolling)
- basic form interactions (text inputs, checkboxes, radios, select controls; limited keyboard
  activation via `Enter`/`Space`)
- built-in `about:*` pages (`about:newtab`, `about:blank`, `about:error`)

These interactions are exercised by the headless UI worker integration tests; the windowed `browser`
app uses the same worker-thread wiring, so link clicking and basic form interactions work in the GUI
as well.

See [browser_ui.md](browser_ui.md) for implementation details and current status.

## Environment variables / resource limits

Browser-related environment variables live in [env-vars.md](env-vars.md) (see “Browser UI (`browser`
binary)”). Notably:

- `FASTR_BROWSER_MEM_LIMIT_MB=<MiB>` – best-effort address-space (virtual memory) limit for the
  `browser` process. This is applied at process start (and may be unsupported on some platforms).

When running against arbitrary real-world pages, consider using the repo’s resource limit wrapper
(see [browser_ui.md](browser_ui.md)).

## Implementation notes

For implementation details (code layout, message protocol, cancellation, platform prerequisites),
see [browser_ui.md](browser_ui.md).

## Debugging tips

### Renderer debugging knobs

Most renderer debug knobs are environment variables; the canonical list is
[env-vars.md](env-vars.md). A few commonly useful ones while developing the browser UI:

- `FASTR_RENDER_TIMINGS=1` – log per-stage timings to stderr.
- `FASTR_TRACE_OUT=/tmp/trace.json` – write Chrome trace events for a render.
- `FASTR_PAINT_BACKEND=display_list|legacy` – switch paint backend.

### Where to look for logs

The `browser` binary currently logs to stdout/stderr only (run it from a terminal). If the window
opens but nothing renders, check for:

- `wgpu` surface errors printed by [`src/bin/browser.rs`](../src/bin/browser.rs)
- renderer debug output enabled via `FASTR_*` env vars

For panics, use:

```bash
RUST_BACKTRACE=1 scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --features browser_ui --bin browser
```
