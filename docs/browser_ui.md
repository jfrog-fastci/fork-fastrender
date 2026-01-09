# Desktop browser UI (experimental)

FastRender has an experimental desktop “browser” binary at [`src/bin/browser.rs`](../src/bin/browser.rs).

This is **feature-gated** so the core renderer can compile without pulling in the heavy GUI stack.

For a higher-level overview of the `browser` binary (current capabilities, env vars, and how to run
it), see [browser.md](browser.md).

## Build / run

The `browser` binary is behind the Cargo feature `browser_ui` (note the underscore) and is **not**
enabled by default.

```bash
# Debug build:
cargo run --features browser_ui --bin browser

# Release build:
cargo run --release --features browser_ui --bin browser
```

If you try to build/run it without the feature, Cargo will refuse because the target has
`required-features = ["browser_ui"]` in [`Cargo.toml`](../Cargo.toml).

When running the browser UI against arbitrary real-world pages, consider using the repo’s resource
limit wrapper (especially on multi-agent hosts):

```bash
scripts/run_limited.sh --as 64G -- cargo run --release --features browser_ui --bin browser
```

The `browser` binary also supports an in-process, best-effort address-space cap via
`FASTR_BROWSER_MEM_LIMIT_MB` (see [env-vars.md](env-vars.md)).

For CI environments without a display/GPU, the `browser` entrypoint provides **test-only** headless
hooks to exercise startup and UI↔worker wiring without creating a window:

- `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` (prints `HEADLESS_SMOKE_OK` on success)

See [env-vars.md](env-vars.md) for details.

## Code layout

- Entry point + winit/egui/wgpu integration: [`src/bin/browser.rs`](../src/bin/browser.rs)
  - Also contains the current in-binary render worker thread (`spawn_default_render_worker`) that
    handles basic navigation/scroll/paint for the windowed UI.
  - Includes a test-only headless smoke mode (see `FASTR_TEST_BROWSER_HEADLESS_SMOKE` in
    [env-vars.md](env-vars.md)).
- Browser UI core (tabs/history model, cancellation helpers, worker wrapper):
  [`src/ui/`](../src/ui/)
  - UI state model (`BrowserAppState`/tabs/chrome): [`src/ui/browser_app.rs`](../src/ui/browser_app.rs)
  - egui chrome widgets (tabs row, nav buttons, address bar): [`src/ui/chrome.rs`](../src/ui/chrome.rs)
  - About pages (`about:blank`, `about:newtab`, `about:error`): [`src/ui/about_pages.rs`](../src/ui/about_pages.rs)
    - These are currently used by the synchronous `BrowserWorker` helper (see below) and the
      `FASTR_TEST_BROWSER_HEADLESS_SMOKE` test mode. The windowed UI's in-binary worker does not yet
      special-case `about:` URLs.
  - Cancellation helpers: [`src/ui/cancel.rs`](../src/ui/cancel.rs)
  - Message protocol types: [`src/ui/messages.rs`](../src/ui/messages.rs)
  - Input coordinate mapping helpers (egui points ↔ viewport CSS px): [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs)
  - Address bar URL normalization: [`src/ui/url.rs`](../src/ui/url.rs)
  - Headless UI worker loop (`spawn_ui_worker`) that implements navigation + scroll + pointer +
    basic non-JS form interactions: [`src/ui/worker.rs`](../src/ui/worker.rs)
    - Exercised by `tests/browser_integration/ui_worker_interaction.rs`.
  - Synchronous “navigate + render a frame” helper (includes `about:*` support): [`src/ui/browser_worker.rs`](../src/ui/browser_worker.rs)
    - Used by the `FASTR_TEST_BROWSER_HEADLESS_SMOKE` test mode.
  - Headless UI worker loop used by scroll-wheel integration tests (including overflow container
    wheel scrolling): [`src/ui/worker_loop.rs`](../src/ui/worker_loop.rs)
    - Exercised by `tests/browser_integration/ui_worker_scroll.rs`.
  - Tab history helpers: [`src/ui/history.rs`](../src/ui/history.rs)
  - Pixmap → egui texture helpers:
    - [`src/ui/pixmap_texture.rs`](../src/ui/pixmap_texture.rs) (CPU conversion path)
    - [`src/ui/wgpu_pixmap_texture.rs`](../src/ui/wgpu_pixmap_texture.rs) (fast wgpu upload path)
- Renderer APIs used/expected to be used by the UI:
  - Public API surface: [`src/api.rs`](../src/api.rs) (`FastRender`, `RenderOptions`,
    `PreparedDocument`, `PreparedPaintOptions`)
  - Progress + cancellation primitives: [`src/render_control.rs`](../src/render_control.rs)
    (`StageHeartbeat`, `RenderDeadline`)

## High-level architecture (current + intended)

The desktop UI is deliberately split into:

- **UI thread**: owns the winit event loop, builds egui widgets, and presents frames via wgpu.
- **Render worker**: runs the “heavy” pipeline (fetch → parse → style → layout → paint) and produces
  a `tiny_skia::Pixmap` for the current viewport.

The worker boundary keeps the UI responsive under slow network/layout and provides a place to add
browser-style behaviors over time:

- keep the UI responsive under slow network/layout,
- route results to the correct tab via `tab_id`.

Cancellation and stale-frame dropping are planned (see below), but are not yet fully wired into the
current worker implementations.

### UI thread vs render worker thread

The browser UI should run rendering on a dedicated large-stack thread:

- Render recursion can be deep on real pages; see
  [`DEFAULT_RENDER_STACK_SIZE`](../src/system.rs) (128 MiB).
- Thread spawn helper: [`spawn_render_worker_thread`](../src/ui/worker.rs).
  - Note: the windowed `browser` app currently spawns its worker directly in
    [`src/bin/browser.rs`](../src/bin/browser.rs); `spawn_render_worker_thread` is a reusable helper
    for other UI/worker wiring.

### Message protocol (channels)

The intended communication model is message-based (std channels) rather than direct calls, so the UI
can remain responsive and explicitly ignore late results.

Current message types live in [`src/ui/messages.rs`](../src/ui/messages.rs):

**UI → worker** (`UiToWorker`) includes requests like:

- `Navigate { tab_id, url, reason }`
- `ViewportChanged { tab_id, viewport_css, dpr }`
- `Scroll { tab_id, delta_css, pointer_css }`
- pointer/key/text events (`PointerDown/Up/Move`, `TextInput`, `KeyAction`)

**Worker → UI** (`WorkerToUi`) includes:

- `FrameReady { tab_id, frame }` — a rendered `tiny_skia::Pixmap` + viewport/scroll metadata
- `NavigationStarted/Committed/Failed { ... }` — URL/title/back-forward state updates
- `Stage { tab_id, stage }` — coarse progress heartbeats forwarded from the renderer
  (`StageHeartbeat` from [`src/render_control.rs`](../src/render_control.rs))
- `ScrollStateUpdated { tab_id, scroll }` / `LoadingState { tab_id, loading }`

Implementation detail: stage listeners are currently **process-global** (see
`GlobalStageListenerGuard` and `swap_stage_listener` in [`src/render_control.rs`](../src/render_control.rs)).
The UI wrapper in [`src/ui/worker.rs`](../src/ui/worker.rs) assumes the worker runs **at most one**
render job at a time; concurrent renders would need per-job routing.

### Cancellation model (generations + cooperative cancel callbacks)

FastRender cancellation is *cooperative*: `RenderDeadline` can carry a `cancel_callback` that is
polled throughout the pipeline (see [`RenderDeadline::check`](../src/render_control.rs)).

The browser UI includes generation-counter cancellation helpers in [`src/ui/cancel.rs`](../src/ui/cancel.rs):

- `CancelGens::bump_nav()` invalidates in-flight **prepare** and **paint** work (new navigation).
- `CancelGens::bump_paint()` invalidates only in-flight **paint** work (e.g. scroll/resize).

Note: the current worker loops are mostly synchronous and do not yet use these helpers; they
document the intended pattern for when cancellation is wired in.

The typical pattern is:

1. Take a `CancelSnapshot` before starting work.
2. Derive a cancel callback from the snapshot.
3. Attach it to the renderer:
   - for full renders / prepares: `RenderOptions.cancel_callback` (and/or `RenderOptions.timeout`)
   - for prepared paints: install a `RenderDeadline` via
     [`render_control::DeadlineGuard`](../src/render_control.rs) around
     `PreparedDocument::paint_with_options` (because `PreparedPaintOptions` is currently view-only
     and does not carry cancellation fields)
4. When results arrive, drop them if the snapshot no longer matches the current generations.

When wired in, this prevents “old” frames from showing up after the user has moved on, and saves
CPU by stopping stale work early.

## Known limitations (as of now)

- **No author JavaScript**: `<script>` does not execute.
- **Limited form support** (non-JS):
  - text input is intentionally minimal (no selection/caret movement beyond append/backspace)
  - many controls are not yet supported (`<select>`, `contenteditable`, file inputs, etc.)
- No persistent browser profile (cookies/storage/devtools/extensions/etc.).

## MSRV + GUI version pinning

This repository is pinned to `rust-version = "1.70"` (MSRV) in [`Cargo.toml`](../Cargo.toml).
The desktop UI stack is therefore pinned to older-but-compatible versions:

- `egui` **0.23**
- `winit` **0.28**
- `wgpu` **0.17**

Do not “cargo update” these casually: newer `egui`/`winit`/`wgpu` releases tend to raise their MSRV.

## Platform prerequisites

### Ubuntu / Debian (Linux)

Building `--features browser_ui` pulls in `winit` (X11 backend) and `wgpu`. On a minimal Linux
install you will likely need additional system development packages.

On CI we rely on the `ubuntu-latest` runner image having these available; to reproduce locally:

```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxrandr-dev libxi-dev libxcursor-dev \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libegl1-mesa-dev libvulkan-dev
```

### macOS

Xcode Command Line Tools are required:

```bash
xcode-select --install
```

### Windows

Use the MSVC toolchain (the default on GitHub Actions’ `windows-latest` runner):

- Install Visual Studio (or “Build Tools for Visual Studio”) with the **Desktop development with
  C++** workload.
- Use the `x86_64-pc-windows-msvc` Rust toolchain.
