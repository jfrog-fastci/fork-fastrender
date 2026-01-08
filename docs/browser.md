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
- The start of an interactive surface (scroll + hit-testing + basic form interactions) without
  requiring JavaScript.

**This is not:**

- A “real” web browser engine (no multi-process architecture, no extensions/devtools/service
  workers, etc.).
- A JavaScript-capable browser: there is currently **no author JS engine** and `<script>` does not
  execute. (See [docs/javascript.md](javascript.md) for the separate JS workstream.)

## Build / run

The `browser` binary is feature-gated behind `browser_ui` so the core renderer can compile without
pulling in the GUI stack.

```bash
# Debug build:
cargo run --features browser_ui --bin browser

# Release build:
cargo run --release --features browser_ui --bin browser
```

If you try to run it without `--features browser_ui`, `Cargo.toml` will refuse because the binary
has `required-features = ["browser_ui"]`. See:

- [`Cargo.toml`](../Cargo.toml)
- Platform prerequisites + MSRV constraints: [browser_ui.md](browser_ui.md)

## UI overview (current)

The UI is intentionally minimal and currently acts as a **scaffold**:

- **Top chrome bar** (egui `TopBottomPanel` in [`src/bin/browser.rs`](../src/bin/browser.rs)):
  - back/forward/reload buttons (currently no-op)
  - address bar text field (currently does not trigger navigation)
- **Content area**:
  - currently displays a dummy checkerboard pixmap
  - clicks in the page area print the local click position to stdout (in egui “points”, treated as
    CSS px)

Tabs and loading/error status are not yet wired into the UI, but there is supporting scaffolding in
[`src/ui/history.rs`](../src/ui/history.rs) and stage heartbeat plumbing (see below).

## Architecture

### Threading model

The intended (and partially implemented) split is:

- **UI thread**
  - owns the winit event loop (`EventLoop::run`)
  - builds egui widgets (chrome + page view)
  - presents via wgpu (`wgpu::Surface`)
- **Render worker thread**
  - runs the “heavy” pipeline: fetch → parse → style → layout → paint
  - is spawned with a large stack (128 MiB) because real pages can recurse deeply during DOM/style/
    layout:
    - [`src/system.rs`](../src/system.rs) (`DEFAULT_RENDER_STACK_SIZE`)
    - [`src/ui/worker.rs`](../src/ui/worker.rs) (`spawn_render_worker_thread`)

Even when the browser is not fully asynchronous yet, keeping a worker thread boundary makes the UI
responsive and sets us up for:

- cancellation (scrolling/navigating shouldn’t queue unbounded work),
- dropping stale renders,
- multi-tab rendering.

### Why a message protocol

Communication between UI and worker is intended to be message-based (channels), rather than direct
function calls, so the UI can:

- stay responsive under slow network fetches / expensive layout,
- keep an explicit “current request”/generation id, and
- ignore late results.

#### Worker → UI messages (current)

The message enum is defined in [`src/ui/messages.rs`](../src/ui/messages.rs):

- `WorkerToUi::Stage { tab_id: TabId, stage: StageHeartbeat }`

`StageHeartbeat` is a coarse progress marker emitted from the renderer (see
[`src/render_control.rs`](../src/render_control.rs)). It covers phases like:

- `dom_parse`, `css_inline`, `css_parse`, `cascade`, `box_tree`, `layout`, `paint_build`,
  `paint_rasterize`

Stage heartbeats are forwarded by [`src/ui/worker.rs`](../src/ui/worker.rs) via
`render_control::set_stage_listener`.

Important limitation (current code): the stage listener is **process-global**, so the worker wrapper
assumes only one render job runs at a time. (This is called out in the `StageListenerGuard` docs in
[`src/ui/worker.rs`](../src/ui/worker.rs).)

## Rendering pipeline integration details

### Prepare vs paint

FastRender supports a “prepare then paint” split:

- Prepare: parse DOM + inline CSS + cascade + layout into a [`PreparedDocument`](../src/api.rs)
- Paint: rasterize the prepared document multiple times with different scroll offsets / viewport
  size via `PreparedDocument::paint_with_options`

The browser UI worker wrapper exposes this shape in [`src/ui/worker.rs`](../src/ui/worker.rs):

- `RenderWorker::prepare_html(...) -> PreparedDocument`
- `RenderWorker::paint_prepared(...) -> Pixmap`

### Cancellation and stale frame dropping

The renderer supports *cooperative* cancellation via a cancel callback stored in
[`RenderDeadline`](../src/render_control.rs) (see `RenderOptions.cancel_callback` in
[`src/api.rs`](../src/api.rs)).

The browser UI side has a small helper for generating cancel callbacks:

- [`src/ui/cancel.rs`](../src/ui/cancel.rs) (`CancelGens`)

The intended pattern:

1. UI increments a generation counter on navigation or scroll.
2. UI submits render work tagged with the generation snapshot.
3. Worker installs a deadline with a cancel callback derived from the snapshot.
4. When a render finishes, UI compares the returned generation with the current one:
   - if stale, drop the frame;
   - if current, update the displayed pixmap.

This avoids showing “old” frames after rapid scroll/nav and also avoids wasting CPU on work the user
no longer cares about.

## Coordinate systems and scaling

FastRender layout is done in **CSS pixels**, but painting produces a **device-pixel** pixmap:

- CSS viewport size: `(width_css_px, height_css_px)`
- Device scale factor: `device_pixel_ratio` (DPR)
- Pixmap size: `(width_css_px * DPR, height_css_px * DPR)`

The paint code does this explicitly (see `Painter::with_resources_scaled` in
[`src/paint/painter.rs`](../src/paint/painter.rs)).

In the desktop UI:

- winit gives a window **scale factor** (physical pixels per logical pixel).
- egui uses “points” (logical pixels). We treat **egui points as CSS px**.

Mapping summary for hit-testing:

- pointer position from egui: `(x_points, y_points)` → **CSS px**
- document-space CSS px: `(x_points + scroll_x, y_points + scroll_y)`
- pixmap device pixel coords: `(x_points * DPR, y_points * DPR)` (relative to the viewport)

If your page texture looks blurry, it usually means the pixmap dimensions don’t match the window’s
physical resolution (e.g., rendering at CSS px but displaying scaled up). The UI should render at
CSS size with `device_pixel_ratio = window.scale_factor()` so the pixmap is full-resolution.

## Interaction model (MVP, non-JS)

The desktop browser is aiming for a very small “interaction” surface without JS:

- clicking links (navigate to `<a href=...>`)
- focus + basic text input (`<input>` / `<textarea>`)
- toggling `<input type=checkbox>` / `<input type=radio>`

Renderer-side DOM mutation helpers already exist (and are expected to be driven by UI hit-testing):

- [`src/interaction/dom_mutation.rs`](../src/interaction/dom_mutation.rs)
  - `toggle_checkbox`
  - `activate_radio`
  - `append_text_to_input` / `backspace_input`
  - `append_text_to_textarea` / `backspace_textarea`
  - focus/hover/active state via `data-fastr-*` attributes

### Known unsupported controls (as of now)

The UI does not yet wire real hit-testing or input routing. In addition, even after wiring, expect
many controls to be unsupported initially, including:

- `<select>` (menus/listboxes)
- `<input type=range>` / `<input type=color>` / `<input type=file>`
- contenteditable, selection/caret movement, rich clipboard integration
- any DOM event model (because no JS)

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
RUST_BACKTRACE=1 cargo run --features browser_ui --bin browser
```

