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

The `browser` UI is intentionally minimal, but it is now wired up end-to-end:

- **Tabs**: create/close/switch tabs.
- **Navigation**:
  - address bar URL entry (press Enter to navigate; user input is normalized, e.g. `example.com`
    → `https://example.com/`, filesystem paths → `file://...`)
  - per-tab history with back/forward/reload
  - loading + error status in the chrome
- **Scrolling**: mouse wheel / trackpad scroll updates the viewport scroll offset and repaints.
- **Pointer/keyboard routing**: the window UI forwards pointer and keyboard events to the render
  worker (see `UiToWorker` in [`src/ui/messages.rs`](../src/ui/messages.rs)).

Basic hit-testing/link navigation and non-JS form interactions are implemented in the headless UI
worker loop under [`src/ui/worker.rs`](../src/ui/worker.rs) (and exercised by integration tests),
including:

- clicking links (hit-test fragments → navigate)
- basic focus + text input for `<input>` / `<textarea>`
- checkbox/radio toggling

Wiring these interactions into the windowed `browser` app is ongoing. See [browser_ui.md](browser_ui.md)
for implementation details.

## Environment variables / resource limits

Browser-related environment variables live in [env-vars.md](env-vars.md). Notably:

- `FASTR_BROWSER_MEM_LIMIT_MB=<MiB>` – best-effort address-space (virtual memory) limit for the
  `browser` process. This is applied at process start (and may be unsupported on some platforms).
- **Test-only headless hooks** (used by integration tests / CI):
  - `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` – exit successfully before creating a window.
  - `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` – run a minimal end-to-end UI↔worker headless smoke test and
    print `HEADLESS_SMOKE_OK` on success (no winit/wgpu init).

When running against arbitrary real-world pages, consider using the repo’s resource limit wrapper
(see [browser_ui.md](browser_ui.md)).

## Implementation notes

For implementation details (code layout, message protocol, cancellation, platform prerequisites),
see [browser_ui.md](browser_ui.md).
