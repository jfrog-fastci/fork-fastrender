# Desktop browser UI (experimental)

FastRender has an experimental desktop “browser” binary at [`src/bin/browser.rs`](../src/bin/browser.rs).

This is **feature-gated** so the core renderer can compile without pulling in the heavy GUI stack.

For a higher-level overview of the `browser` binary (current capabilities, env vars, and how to run
it), see [browser.md](browser.md).

## Build / run

The `browser` binary is behind the Cargo feature `browser_ui` (note the underscore) and is **not**
enabled by default.

Always use the repo wrappers (see [`AGENTS.md`](../AGENTS.md)) when building/running the browser UI:
`scripts/cargo_agent.sh` for Cargo invocations and `scripts/run_limited.sh` to apply resource limits.

```bash
# Debug build:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser

# Release build:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

### Wayland (optional, Linux)

On Linux, `browser_ui` builds with the **X11** backend only (so minimal/CI hosts don't need Wayland
development packages). To build with both **X11 + Wayland** support, enable `browser_ui_wayland`:

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,browser_ui_wayland --bin browser
```

`winit` selects the backend at runtime based on your environment (e.g. `WAYLAND_DISPLAY` / `DISPLAY`).
You can force a specific backend with:

```bash
# Force Wayland:
WINIT_UNIX_BACKEND=wayland bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,browser_ui_wayland --bin browser
```

If you run the `browser` binary without the feature, it will print a short message and exit
(the real implementation is behind the `browser_ui` feature gate; see
[`src/bin/browser.rs`](../src/bin/browser.rs)).

When running the browser UI against arbitrary real-world pages, consider using the repo’s resource
limit wrapper (especially on multi-agent hosts):

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

The `browser` binary also supports an in-process, best-effort address-space cap via
`FASTR_BROWSER_MEM_LIMIT_MB` (see [env-vars.md](env-vars.md)).

To prevent huge in-process pixmap allocations when the window is resized to extreme sizes (or when
running on very high-DPI displays), the browser UI also clamps viewport/DPR based on these env vars
(see [env-vars.md](env-vars.md) for defaults and details):

- `FASTR_BROWSER_MAX_PIXELS`
- `FASTR_BROWSER_MAX_DIM_PX`
- `FASTR_BROWSER_MAX_DPR`

For CI environments without a display/GPU, the `browser` entrypoint provides **test-only** headless
hooks to exercise startup and UI↔worker wiring without creating a window:

- `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` (prints `HEADLESS_SMOKE_OK` on success)

See [env-vars.md](env-vars.md) for details.

CI note: the main GitHub Actions workflow (`ci.yml`) compiles the `browser` binary with
`--features browser_ui` on Linux/macOS/Windows; Linux additionally runs the headless smoke mode.

Note: the windowed `browser` app currently starts by navigating to `about:newtab`.

## Keyboard / mouse shortcuts

| Shortcut | Action |
|---|---|
| Ctrl/Cmd+L | Focus address bar (select all) |
| Ctrl/Cmd+T | New tab |
| Ctrl/Cmd+W | Close active tab (no-op if only one tab) |
| Ctrl/Cmd+Tab | Next tab |
| Ctrl/Cmd+Shift+Tab | Previous tab |
| Alt+Left | Back |
| Alt+Right | Forward |
| Ctrl/Cmd+R / F5 | Reload |
| Mouse Back / Mouse Forward (buttons 8/9) | Back / Forward |

## Code layout

- Entry point + winit/egui/wgpu integration: [`src/bin/browser.rs`](../src/bin/browser.rs)
  - Spawns the production browser worker thread via
    [`spawn_browser_ui_worker`](../src/ui/render_worker.rs) (large stack; std::io-friendly wrapper),
    which handles navigation/history, scrolling, and DOM interaction and produces `WorkerToUi`
    updates.
    - The worker owns navigation history; the windowed chrome sends `UiToWorker::{GoBack,GoForward,Reload}`.
  - Renders a small egui popup for `<select>` dropdowns. Workers can request a popup via:
    - `WorkerToUi::OpenSelectDropdown` (legacy cursor-anchored message)
    - `WorkerToUi::SelectDropdownOpened` (preferred; includes an explicit `anchor_css` rect)
    - The current windowed UI handles both; when `SelectDropdownOpened` is available it uses
      `anchor_css` to position the popup relative to the rendered frame (instead of anchoring to the
      cursor).
  - Includes a test-only headless smoke mode (see `FASTR_TEST_BROWSER_HEADLESS_SMOKE` in
    [env-vars.md](env-vars.md)).
- Browser UI core (tabs/history model, cancellation helpers, worker wrapper):
  [`src/ui/`](../src/ui/)
  - UI state model (`BrowserAppState`/tabs/chrome): [`src/ui/browser_app.rs`](../src/ui/browser_app.rs)
  - Chrome action types + a reusable egui chrome UI helper: [`src/ui/chrome.rs`](../src/ui/chrome.rs)
    - The windowed `browser` app currently renders its chrome widgets inline in
      [`src/bin/browser.rs`](../src/bin/browser.rs) (see `App::render_chrome_ui`), but reuses the
      `ChromeAction` type.
  - About pages (`about:blank`, `about:newtab`, `about:error`): [`src/ui/about_pages.rs`](../src/ui/about_pages.rs)
    - Used by the canonical UI render worker runtime ([`src/ui/render_worker.rs`](../src/ui/render_worker.rs)).
  - Cancellation helpers: [`src/ui/cancel.rs`](../src/ui/cancel.rs)
  - Message protocol types: [`src/ui/messages.rs`](../src/ui/messages.rs)
  - Input coordinate mapping helpers (egui points ↔ viewport CSS px): [`src/ui/input_mapping.rs`](../src/ui/input_mapping.rs)
  - Address bar URL normalization: [`src/ui/url.rs`](../src/ui/url.rs)
  - Canonical UI render worker runtime (navigation/history + interaction + cancellation):
    [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)
    - `spawn_browser_worker` is the main worker runtime; the windowed `browser` app spawns it via
      `spawn_browser_ui_worker` (std::io-friendly wrapper).
    - `spawn_ui_worker` / `spawn_ui_worker_with_factory` are headless entrypoints used by browser
      integration tests (same runtime; no parallel worker loop).
  - Render-thread utilities (stage heartbeat forwarding, large-stack thread builder):
    [`src/ui/worker.rs`](../src/ui/worker.rs)
  - Tab history helpers: [`src/ui/history.rs`](../src/ui/history.rs)
  - Pixmap → egui texture helpers: [`src/ui/wgpu_pixmap_texture.rs`](../src/ui/wgpu_pixmap_texture.rs)
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

The codebase includes cancellation helpers (generation counters + cooperative cancel callbacks), but
not all worker implementations are fully cancellation-aware yet.

### UI thread vs render worker thread

The browser UI should run rendering on a dedicated large-stack thread:

- Render recursion can be deep on real pages; see
  [`DEFAULT_RENDER_STACK_SIZE`](../src/system.rs) (128 MiB).
- The windowed `browser` app spawns its worker via
  [`spawn_browser_ui_worker`](../src/ui/render_worker.rs) (wrapper around `spawn_browser_worker`),
  which uses `std::thread::Builder` and [`DEFAULT_RENDER_STACK_SIZE`](../src/system.rs) to configure
  the stack size.
- Headless UI worker loops used by integration tests (`spawn_ui_worker`, etc) use the same large-stack
  configuration (see [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)).

### Message protocol (channels)

The intended communication model is message-based (std channels) rather than direct calls, so the UI
can remain responsive and explicitly ignore late results.

Current message types live in [`src/ui/messages.rs`](../src/ui/messages.rs):

**UI → worker** (`UiToWorker`) includes requests like:

- `CreateTab { tab_id, initial_url, cancel }`
  - Creates a new tab on the worker side.
  - If `initial_url` is `None`, the tab is created in an “empty” state and will not produce any
    navigation/frame messages until the UI sends an explicit `Navigate`.
    - UIs that want a default page (e.g. `about:newtab`) should provide it explicitly via
      `initial_url: Some("about:newtab".to_string())`.
- `NewTab { tab_id, initial_url }` — optional alias for `CreateTab` (kept for protocol flexibility).
- `CloseTab { tab_id }`
- `SetActiveTab { tab_id }`
- `Navigate { tab_id, url, reason }`
- History actions (`GoBack { tab_id }`, `GoForward { tab_id }`, `Reload { tab_id }`)
- `Tick { tab_id }` — periodic “event loop slice” / repaint driver (used by JS-capable workers)
- `ViewportChanged { tab_id, viewport_css, dpr }`
- `Scroll { tab_id, delta_css, pointer_css }`
- pointer/key/text events (`PointerDown/Up/Move`, `TextInput`, `KeyAction`)
- `SelectDropdownChoose { tab_id, select_node_id, option_node_id }` — user selected an option from
  a dropdown popup (sent after `WorkerToUi::SelectDropdownOpened`)
- `SelectDropdownCancel { tab_id }` — user dismissed a dropdown popup (Escape/click-away)

Coordinate convention: `pos_css` / `pointer_css` fields are **viewport-relative CSS pixels** (origin
at the top-left of the viewport). They must **not** include the current scroll offset; worker loops
add `scroll_state.viewport` when converting to page coordinates for hit-testing.

**Worker → UI** (`WorkerToUi`) includes:

- `FrameReady { tab_id, frame }` — a rendered `tiny_skia::Pixmap` + viewport/scroll metadata
- `Warning { tab_id, text }` — non-fatal, user-facing warnings (e.g. viewport/DPR clamping to avoid
  huge pixmap allocations); the windowed `browser` UI currently surfaces this as a small warning
  badge in the chrome.
- `OpenSelectDropdown { tab_id, select_node_id, control }` — legacy dropdown popup request for a
  `<select>` control (cursor-anchored; kept for back-compat with older UIs).
- `SelectDropdownOpened { tab_id, select_node_id, control, anchor_css }` — request the UI open a
  dropdown popup for a `<select>` control, with an explicit `anchor_css` in **viewport-local CSS
  pixels** so the popup can be positioned relative to the rendered frame.
- `SelectDropdownClosed { tab_id }` — close/dismiss any open dropdown popup for the tab
- `NavigationStarted/Committed/Failed { ... }` — URL/title/back-forward state updates
- `Stage { tab_id, stage }` — coarse progress heartbeats forwarded from the renderer
  (`StageHeartbeat` from [`src/render_control.rs`](../src/render_control.rs))
  - Can be surfaced by chrome UIs while loading (e.g. [`src/ui/chrome.rs`](../src/ui/chrome.rs)).
- `ScrollStateUpdated { tab_id, scroll }` / `LoadingState { tab_id, loading }`

Note: not all worker implementations emit every message variant. For example, the windowed `browser`
app's worker thread (`spawn_browser_worker` via `spawn_browser_ui_worker`) emits `FrameReady`, select
dropdown open messages (`OpenSelectDropdown`/`SelectDropdownOpened`), and navigation/scroll/loading
events. Stage heartbeats are only sent when a stage listener is installed by the worker.
The canonical browser UI worker loop installs a listener for the duration of each navigation prepare
job and for each paint (including scroll/hover-driven repaints), tagging forwarded heartbeats with
the current `tab_id`.

Implementation detail: stage listeners are stored in a **thread-local stack** (see
`push_stage_listener` / `StageListenerGuard` in [`src/render_control.rs`](../src/render_control.rs)).
`record_stage` invokes the top-of-stack thread-local listener first, then (optionally) a
process-global listener (`swap_stage_listener` / `GlobalStageListenerGuard`, mainly used by tests).

Browser worker implementations typically install a stage listener for the duration of a specific
job (navigation/tick/etc). Multiple worker threads can render concurrently without clobbering each
other's stage forwarding, but overlapping render jobs on the *same* thread would still require
per-job routing (e.g. tagging stage messages with a job identifier).

### Worker-owned history semantics

Navigation/history state is **owned by the worker** (per-tab [`TabHistory`](../src/ui/history.rs)):

- The UI **must not** attempt to compute history state client-side (e.g. by pushing its own URL
  stack or “guessing” the next URL for back/forward).
- The UI sends history actions (`UiToWorker::{GoBack,GoForward,Reload}`) without providing a URL.
- The worker sends `WorkerToUi::{NavigationCommitted,NavigationFailed}` including:
  - the current URL/title,
  - `can_go_back` / `can_go_forward` affordances.
- Scroll restoration is also worker-owned: the worker persists scroll offsets in history entries
  (`HistoryEntry.{scroll_x,scroll_y}`) and restores them on back/forward/reload.

Implementation notes (canonical worker loop in [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)):

- When starting a new navigation, the worker may push a **provisional** history entry immediately.
  If that navigation is superseded before it commits, the entry is typically updated in-place
  (`TabHistory::replace_current_url`) to avoid leaving cancelled URLs in the back/forward list.
- Redirects are committed by updating the current entry’s URL in-place
  (`TabHistory::commit_navigation`) rather than creating a new entry.

### Cancellation model (generations + cooperative cancel callbacks)

FastRender cancellation is *cooperative*: `RenderDeadline` can carry a `cancel_callback` that is
polled throughout the pipeline (see [`RenderDeadline::check`](../src/render_control.rs)).

The browser UI includes generation-counter cancellation helpers in [`src/ui/cancel.rs`](../src/ui/cancel.rs):

- `CancelGens::bump_nav()` invalidates in-flight **prepare** and **paint** work (new navigation).
- `CancelGens::bump_paint()` invalidates only in-flight **paint** work (e.g. scroll/resize).

When building a UI front-end, bump gens **before sending** the corresponding `UiToWorker` message so
in-flight work can be cancelled even while the worker thread is busy:

| UI action | Cancel gen to bump |
|---|---|
| `Navigate`, `GoBack`, `GoForward`, `Reload` | `bump_nav()` |
| `ViewportChanged`, `Scroll`, input events, `RequestRepaint` | `bump_paint()` |

Note: the canonical browser UI worker loop (`spawn_browser_worker` / `spawn_ui_worker*`) uses these
helpers to cancel stale prepares/paints. The browser integration suite serializes tests that depend
on deterministic cancellation timing (see `stage_listener_test_lock`).

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

## Debugging tips (UI/worker)

### Stage heartbeat logging

The renderer emits coarse “where are we?” stage heartbeats (`StageHeartbeat` in
[`src/render_control.rs`](../src/render_control.rs)). The canonical browser worker installs a
per-thread stage listener and forwards these as `WorkerToUi::Stage { tab_id, stage }`.

Tips:

- The windowed UI already surfaces a condensed stage string in its chrome (e.g. `Loading… layout`).
- When debugging hangs/blank frames, it’s often useful to **log every stage message** received on
  the UI thread (including `tab_id`) to see where time is spent.
- If you implement a custom worker loop, make sure you install a stage listener around both
  “prepare” and “paint” work; the lightweight wrapper in [`src/ui/worker.rs`](../src/ui/worker.rs)
  (`RenderWorker`) shows the minimal pattern.

### Built-in `about:test-*` pages

For deterministic, offline repros (no network), the worker supports a few `about:` pages defined in
[`src/ui/about_pages.rs`](../src/ui/about_pages.rs):

- `about:test-scroll` — a simple tall page for scroll/viewport behavior.
- `about:test-heavy` — a large DOM intended to make cancellation/timeout behavior observable.
- `about:test-form` — a minimal form for interaction/input testing.

These are used by the browser UI integration tests, but are also handy for manual debugging in the
windowed app.

### Worker debug logs

The worker can emit best-effort structured debug lines via `WorkerToUi::DebugLog { tab_id, line }`.
Front-ends are encouraged to print these to stderr while developing new protocol behavior.

## Known limitations (as of now)

- **No author JavaScript in the browser UI yet**: `<script>` does not run in the windowed `browser`
  app today. (See [javascript.md](javascript.md) and
  [html_script_processing.md](html_script_processing.md) for the in-tree JS workstream.)
- **Interaction gaps** (non-JS): the windowed UI forwards pointer/keyboard input to the browser
  worker, which applies basic hit-testing + form interactions. Some interactions are still
  incomplete (e.g. select dropdown UI, rich text editing, complex focus traversal).
- **Limited form support** (non-JS):
  - text input is intentionally minimal (no selection/caret movement beyond append/backspace)
  - `<select>` support is basic (listbox clicks + dropdown popup selection; no typeahead/multi-select yet)
  - many controls are not yet supported (`contenteditable`, file inputs, etc.)
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

For Wayland builds (`--features browser_ui,browser_ui_wayland`) you also need the Wayland
development headers:

```bash
sudo apt-get install -y libwayland-dev
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
