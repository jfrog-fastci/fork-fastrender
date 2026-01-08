# Desktop browser UI (experimental)

FastRender has an experimental desktop “browser” binary at [`src/bin/browser.rs`](../src/bin/browser.rs).

This is **feature-gated** so the core renderer can compile without pulling in the heavy GUI stack.

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

## Code layout

- Entry point + winit/egui/wgpu integration: [`src/bin/browser.rs`](../src/bin/browser.rs)
- Browser-UI scaffolding (tab/history model, cancellation helpers, worker wrapper):
  [`src/ui/`](../src/ui/)
  - Cancellation helpers: [`src/ui/cancel.rs`](../src/ui/cancel.rs)
  - Message protocol types: [`src/ui/messages.rs`](../src/ui/messages.rs)
  - Render worker wrapper + large-stack thread spawn:
    [`src/ui/worker.rs`](../src/ui/worker.rs)
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

Even though the current `browser` binary is still mostly a **scaffold** (it draws a dummy pixmap and
does not yet perform real navigation), the worker boundary is kept so we can:

- keep the UI responsive under slow network/layout,
- cancel work on rapid navigation/scroll,
- drop stale renders, and
- support multiple tabs later.

### UI thread vs render worker thread

The browser UI should run rendering on a dedicated large-stack thread:

- Render recursion can be deep on real pages; see
  [`DEFAULT_RENDER_STACK_SIZE`](../src/system.rs) (128 MiB).
- Thread spawn helper: [`spawn_render_worker_thread`](../src/ui/worker.rs).

### Message protocol (channels)

The intended communication model is message-based (std channels) rather than direct calls, so the UI
can remain responsive and explicitly ignore late results.

Current message types live in [`src/ui/messages.rs`](../src/ui/messages.rs). Today this is minimal:

- `WorkerToUi::Stage { tab_id, stage }` — coarse progress heartbeats forwarded from the renderer
  (`StageHeartbeat` from [`src/render_control.rs`](../src/render_control.rs)).

Implementation detail: stage listeners are currently **process-global** (see
`GlobalStageListenerGuard` and `swap_stage_listener` in [`src/render_control.rs`](../src/render_control.rs)).
The UI wrapper in [`src/ui/worker.rs`](../src/ui/worker.rs) assumes the worker runs **at most one**
render job at a time; concurrent renders would need per-job routing.

### Cancellation model (generations + cooperative cancel callbacks)

FastRender cancellation is *cooperative*: `RenderDeadline` can carry a `cancel_callback` that is
polled throughout the pipeline (see [`RenderDeadline::check`](../src/render_control.rs)).

The browser UI scaffolding uses a generation-counter approach in [`src/ui/cancel.rs`](../src/ui/cancel.rs):

- `CancelGens::bump_nav()` invalidates in-flight **prepare** and **paint** work (new navigation).
- `CancelGens::bump_paint()` invalidates only in-flight **paint** work (e.g. scroll/resize).

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

This prevents “old” frames from showing up after the user has moved on, and saves CPU by stopping
stale work early.

## Known limitations (as of now)

- **No author JavaScript**: `<script>` does not execute.
- The `browser` UI is still an MVP scaffold:
  - back/forward/reload buttons are currently no-op
  - the address bar does not yet trigger navigation
  - the content area currently displays a dummy checkerboard pixmap
- No real hit-testing/input routing yet; form controls are not wired up in the UI.
  Renderer-side DOM mutation helpers exist (e.g. checkbox toggle/text input) in
  [`src/interaction/dom_mutation.rs`](../src/interaction/dom_mutation.rs), but the UI does not drive
  them yet.

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
