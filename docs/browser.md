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
- basic form interactions (text inputs, checkboxes, radios; limited keyboard activation via
  `Enter`/`Space`)

These interactions are currently exercised by the headless UI workers used by integration tests;
the windowed `browser` app now uses the same worker-thread wiring, so link clicking and basic form
interactions work in the GUI as well. See [browser_ui.md](browser_ui.md) for implementation details
and current status.

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
