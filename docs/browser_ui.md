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
`browser --mem-limit-mb <MB>` or `FASTR_BROWSER_MEM_LIMIT_MB` (see [env-vars.md](env-vars.md)).

The download directory can be configured via `browser --download-dir <path>` or
`FASTR_BROWSER_DOWNLOAD_DIR=<path>`.

Downloaded files are saved into a per-browser download directory resolved in this order:

1. `browser --download-dir <path>`
2. `FASTR_BROWSER_DOWNLOAD_DIR=<path>`
3. The OS downloads directory (via `directories::UserDirs`)
4. The current working directory

To prevent huge in-process pixmap allocations when the window is resized to extreme sizes (or when
running on very high-DPI displays), the browser UI also clamps viewport/DPR based on these env vars
(see [env-vars.md](env-vars.md) for defaults and details):

- `FASTR_BROWSER_MAX_PIXELS`
- `FASTR_BROWSER_MAX_DIM_PX`
- `FASTR_BROWSER_MAX_DPR`
If the windowed UI fails to start due to `wgpu` adapter/device creation issues (common under remote
desktop, VMs, or systems without a working GPU stack), you can force a software adapter and/or
backend:

- `browser --wgpu-fallback` / `FASTR_BROWSER_WGPU_FALLBACK=1`
- `browser --wgpu-backend gl` / `FASTR_BROWSER_WGPU_BACKENDS=gl`

For CI environments without a display/GPU, the `browser` entrypoint provides **test-only** headless
hooks to exercise startup and UI↔worker wiring without creating a window:

- `browser --exit-immediately` / `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`
- `browser --headless-smoke` / `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` (prints `HEADLESS_SMOKE_OK` on success)

See [env-vars.md](env-vars.md) for details.

### GPU / wgpu adapter selection

The windowed `browser` UI uses `wgpu` for presentation. Adapter selection can vary across drivers
and environments (VMs, headless CI, Remote Desktop, etc). The `browser` CLI exposes a few flags to
help with debugging and forcing a specific selection strategy:

- `--power-preference {high,low,none}` — maps to `wgpu::PowerPreference`.
  - `high` prefers a high-performance/discrete GPU (default).
  - `low` prefers an integrated/low-power GPU.
  - `none` leaves the decision up to wgpu/the platform.
- `--force-fallback-adapter` — maps to `RequestAdapterOptions.force_fallback_adapter` (useful for
  software adapters).
- `--wgpu-backends <list>` — restrict the backend set used to create the wgpu instance (comma
  separated), e.g. `--wgpu-backends vulkan,gl`.

Troubleshooting tips:

- If windowed startup fails with a wgpu adapter selection error, try a different backend (for
  example `--wgpu-backends gl`) or `--force-fallback-adapter`.
- If you're in a headless environment without a working display/GPU, use `--headless-smoke` (or
  `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1`) to run a minimal startup smoke test without winit/wgpu.
- Navigate to `about:gpu` to see the selected adapter/backend and the selection options used.

CI note: the main GitHub Actions workflow (`ci.yml`) compiles the `browser` binary with
`--features browser_ui` on Linux/macOS/Windows; Linux additionally runs the headless smoke mode.

Note: by default, when run **without** a URL, the windowed `browser` app tries to restore the
previous session (tabs + per-tab zoom). When run **with** a URL, it opens that URL and does not
restore unless `--restore` is provided.

If no session file exists yet, it falls back to `about:newtab`, which acts as a basic start page
(showing bookmarks + recently visited pages when available). Use `--no-restore` to disable session
restore.

## Appearance

The windowed browser UI exposes a small set of appearance/accessibility knobs (theme, UI scale,
high-contrast, reduced motion) via an in-app **Appearance** popup (gear icon in the toolbar).

These settings are persisted in the browser session file so they survive restarts.

Environment variables like `FASTR_BROWSER_THEME=...` are still supported as overrides (useful for
scripting/CI); see [env-vars.md](env-vars.md).

### Theme mode selection

`FASTR_BROWSER_THEME=system|light|dark` is intended to control the browser chrome theme:

- `system` (default): follow the OS light/dark preference when available.
- `light` / `dark`: force a specific theme.

Interaction with rendered pages:

- The windowed browser UI propagates its resolved theme to rendered pages by default via
  `prefers-color-scheme`.
- Explicit renderer overrides like `FASTR_PREFERS_COLOR_SCHEME=...` take precedence.

### High contrast / reduced motion

- `FASTR_BROWSER_HIGH_CONTRAST=1` is intended to enable a higher-contrast chrome theme and stronger
  focus indicators.
  - Maps to `prefers-contrast` for pages by default unless explicitly overridden via
    `FASTR_PREFERS_CONTRAST=...`.
- `FASTR_BROWSER_REDUCED_MOTION=1` is intended to reduce/disable non-essential UI animations.
  - Maps to `prefers-reduced-motion` for pages by default unless explicitly overridden via
    `FASTR_PREFERS_REDUCED_MOTION=...`.

### UI scale vs page zoom

- **UI scale** (`FASTR_BROWSER_UI_SCALE=<float>`) is intended to scale the browser chrome UI (tabs,
  toolbar, fonts) without changing the page zoom level.
- **Page zoom** is currently implemented as a per-tab setting:
  - shortcuts: Ctrl/Cmd +/-/0, and Ctrl/Cmd + mouse wheel.
  - behaviour: scales the CSS viewport size + DPR (keeps the drawn pixmap size roughly constant
    while making content larger/smaller).

### HUD / debug overlays

- `FASTR_BROWSER_HUD=1` is intended to show an in-app HUD overlay with browser/debug metrics.
- `FASTR_BROWSER_DEBUG_LOG=1` is intended to enable browser/worker debug logging UI (and optionally
  print worker debug lines to stderr).

### Persistence (session file)

The browser persists a lightweight session file for restoring state across restarts.

- Default location: a per-user config directory (via `directories`), e.g.
  `~/.config/fastrender/fastrender_session.json` on Linux.
- Override: `FASTR_BROWSER_SESSION_PATH=/path/to/fastrender_session.json`.

To avoid corrupting the session file, the `browser` process also acquires a lock file for the
session path. If another `browser` process is already running with the same session path, a second
instance will refuse to start. Use `FASTR_BROWSER_SESSION_PATH` to run multiple isolated instances.

The session file format is versioned (currently v2) and includes:

- One or more windows (each with tabs + active tab index)
- Per-tab zoom
- Best-effort window geometry (position/size/maximized) when available
- A crash marker (`did_exit_cleanly`) for detecting unclean exits
- Appearance settings (theme mode, high contrast, reduced motion, UI scale)

The windowed `browser` app uses a background autosave helper (`SessionAutosave`) so session writes
do not block the UI thread:

- **Crash marker:** on startup, the browser immediately persists `did_exit_cleanly=false`. If the
  process is terminated unexpectedly, this marker remains false on disk and the next launch can
  detect the unclean exit (the entrypoint prints a warning when restoring).
- **Background autosave:** while the browser is running, it snapshots the current session and
  schedules a debounced background save on “significant” state changes (for example tab navigations,
  tab/window creation/closure, zoom/appearance changes, and window geometry changes).
- **Clean shutdown:** on a normal exit the final snapshot is written with `did_exit_cleanly=true`.

## Platform polish (window icon, sizing, system theme)

The `browser` window attempts to behave like a “real” native app instead of a prototype:

- **Window icon**: the window icon is loaded from [`assets/app_icon/fastrender.png`](../assets/app_icon/fastrender.png)
  and passed to `winit` via `WindowBuilder::with_window_icon`.
  - Expected behaviour:
    - **Windows/Linux**: icon should appear in the title bar and task switcher / taskbar.
    - **macOS**: the window icon may be ignored by the platform (this is normal); the code path is
      still exercised and should not crash.
- **Default window size**: the window opens at **1200×800** logical pixels, with a minimum size of
  **480×320**, so the chrome remains usable when resized.
- **System theme changes**: when `winit` emits `WindowEvent::ThemeChanged`, the egui visuals are
  updated to match (light/dark). Platform support for live theme change notifications varies.

## Platform-native titlebar integration

The browser UI draws its own chrome (tabs + toolbar) in egui, but relies on `winit` for the native
window and titlebar. The `browser` entrypoint ([`src/bin/browser.rs`](../src/bin/browser.rs))
contains a small amount of platform-specific window configuration so the app feels less “winit
default” and more like a native browser.

### macOS (unified toolbar / traffic lights)

On macOS the browser enables a transparent titlebar + full-size content view via
`winit::platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS}`. This allows the egui chrome to be
rendered *into* the titlebar area (unified toolbar style).

Because the system “traffic light” buttons occupy the top-left of the titlebar, the chrome UI
reserves extra left padding so tabs/toolbar controls don’t end up underneath those buttons.

Manual verification checklist (macOS):

- The chrome background extends behind the titlebar (no extra “empty” titlebar strip).
- Traffic lights remain visible and clickable.
- Tabs/toolbar controls don’t start underneath the traffic lights.

### Windows (best-effort titlebar theming)

On Windows the browser passes a `winit::window::Theme` override to `winit` **only** when the browser
theme is explicitly overridden (for example via `FASTR_BROWSER_THEME=dark`). When no override is set,
the window theme is left as `None` so the native titlebar follows the system light/dark preference.

On supported Windows versions, requesting `Theme::Dark` enables a dark titlebar that better matches
the browser’s dark chrome.

### Linux (X11 + Wayland)

The browser uses the same `Theme` behaviour on Linux (only override when explicitly requested):

- **X11:** winit uses the `_GTK_THEME_VARIANT=dark` hint (best-effort; depends on the window manager).
- **Wayland:** applies to client-side decorations (CSD) where supported by the compositor.

## Usage

- The address bar is an “omnibox”: type either a URL **or** a search query and press Enter.
  - URL inputs are normalized:
    - `example.com` → `https://example.com/`
    - filesystem paths like `/tmp/a.html` → `file://...`
  - Non-URL queries (e.g. `cats`) are treated as searches using the default search engine.
- While typing, the omnibox shows a suggestions dropdown (from history and open tabs).
  - Use ArrowUp/ArrowDown to select a suggestion, Enter to accept, Escape to close the dropdown.
- Right-clicking in the rendered page opens a basic context menu (for example: open link in new tab,
  copy link address, download link, reload).
- Click the downloads icon in the toolbar to open the downloads side panel (shows progress and lets
  you cancel/retry/open/reveal completed downloads).

## Keyboard / mouse shortcuts

| Shortcut | Action |
|---|---|
| Ctrl/Cmd+L | Focus address bar (select all) |
| Ctrl/Cmd+K | Focus address bar (select all) |
| F6 | Focus address bar (select all) |
| Alt+D (Win/Linux) | Focus address bar (select all) |
| Ctrl/Cmd+N | New window |
| Ctrl/Cmd+F | Find in page |
| Ctrl/Cmd+T | New tab |
| Ctrl/Cmd+Shift+T | Reopen last closed tab |
| Ctrl/Cmd+Shift+A | Search tabs / quick switcher |
| Ctrl/Cmd+W | Close active tab (no-op if only one tab) |
| Ctrl/Cmd+F4 | Close active tab (no-op if only one tab) |
| Ctrl/Cmd+Tab | Next tab |
| Ctrl/Cmd+Shift+Tab | Previous tab |
| Ctrl/Cmd+1..9 (9 = last tab) | Activate tab by number |
| Ctrl/Cmd+PageUp | Previous tab |
| Ctrl/Cmd+PageDown | Next tab |
| Alt+Left (Win/Linux) | Back |
| Alt+Right (Win/Linux) | Forward |
| Cmd+[ (macOS) | Back |
| Cmd+] (macOS) | Forward |
| Ctrl/Cmd+R / F5 | Reload |
| Ctrl/Cmd+D | Toggle bookmark for current page |
| Ctrl+H (Win/Linux); Cmd+Y / Cmd+Shift+H (macOS) | Toggle history panel |
| Ctrl/Cmd+Shift+Delete | Open “Clear browsing data” dialog |
| Ctrl/Cmd+Shift+O | Toggle bookmarks manager |
| Ctrl/Cmd+Plus / Ctrl/Cmd+Equals | Zoom in |
| Ctrl/Cmd+Minus | Zoom out |
| Ctrl/Cmd+0 | Reset zoom |
| Ctrl/Cmd + Mouse Wheel | Zoom in/out |
| Middle-click tab | Close tab (no-op if only one tab) |
| Ctrl/Cmd+Click link | Open link in new tab |
| Middle-click link | Open link in new tab |
| Mouse Back / Mouse Forward (buttons 4/5 on Windows/macOS, 8/9 on X11) | Back / Forward |
| PageUp (page focus) | Scroll up |
| PageDown (page focus) | Scroll down |
| ArrowUp (page focus, no element focused) | Scroll up |
| ArrowDown (page focus, no element focused) | Scroll down |
| Space (page focus, no element focused) | Scroll down |
| Shift+Space (page focus, no element focused) | Scroll up |
| Home (page focus, no element focused) | Scroll to top |
| End (page focus, no element focused) | Scroll to bottom |
| Ctrl/Cmd+A (page focus) | Select all text in the focused page `<input>`/`<textarea>` |
| Ctrl/Cmd+C (page focus) | Copy selection from the focused page `<input>`/`<textarea>` to the OS clipboard |
| Ctrl/Cmd+X (page focus) | Cut selection from the focused page `<input>`/`<textarea>` to the OS clipboard |
| Ctrl/Cmd+V (page focus) | Paste OS clipboard text into the focused page `<input>`/`<textarea>` |
| Ctrl+Insert (page focus, Win/Linux) | Copy selection from the focused page `<input>`/`<textarea>` |
| Shift+Insert (page focus, Win/Linux) | Paste OS clipboard text into the focused page `<input>`/`<textarea>` |
| Shift+Delete (page focus, Win/Linux) | Cut selection from the focused page `<input>`/`<textarea>` |

Note: zoom is tracked per-tab and persisted in the browser session file (see `src/ui/session.rs`).

## Menu bar

The windowed `browser` UI includes a browser-style menu bar for discoverability and keyboard parity:

- **File**
- **Edit**
- **View**
- **History**
- **Bookmarks**
- **Window**
- **Help**

Implemented items are wired up to existing browser UI actions (tabs, navigation, reload, zoom,
clipboard, panels).

Some menu entries are still placeholders and remain disabled (for example Undo/Redo).

The menu bar also includes entries for some features that are available elsewhere in the UI, even
if those menu items are currently disabled:

- **New Window**: use <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>N</kbd>.
- **Show Downloads…**: use the downloads button in the toolbar.
- **Bookmark manager…**: use <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd>.
- **Toggle Full Screen**: not implemented yet.

Help/About items open `about:help` / `about:version` in a new tab.

## Bookmarks / History

FastRender’s experimental desktop browser UI supports **bookmarks** and a basic **history** panel.

- **Bookmarking**:
  - Click the **star** button in the toolbar (or press <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>D</kbd>) to
    toggle a bookmark for the current page.
  - Bookmarks appear in the **bookmarks bar** for quick access.
  - Drag bookmarks in the bar to **reorder** them (or use the per-bookmark menu to move left/right).
  - Use the **bookmarks manager** (<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd>) to
    browse/remove bookmarks.
- **History**:
  - Open the history panel with <kbd>Ctrl</kbd>+<kbd>H</kbd> (Win/Linux) or <kbd>Cmd</kbd>+<kbd>Y</kbd>
    / <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd> (macOS).
  - The history UI includes **search** and a **Clear browsing data…** action (with a time range).
- **Persistence**:
  - Bookmarks and history are stored as JSON files under the per-user FastRender config directory
    (for example `~/.config/fastrender/` on Linux).
  - Override the default paths with:
    - `FASTR_BROWSER_BOOKMARKS_PATH`
    - `FASTR_BROWSER_HISTORY_PATH`

## Accessibility / screen readers (chrome)

The browser chrome UI is built with **egui** and, when compiled with `--features browser_ui`, enables
egui’s **AccessKit** integration so screen readers can traverse the chrome widget tree.

Scope note: the rendered page is currently an image; full document accessibility is a separate
future project. Today, screen reader support is intended for chrome-only UI: tabs, toolbar buttons,
address bar, and popups/menus.

Manual verification checklist:

- macOS: VoiceOver can traverse toolbar controls (Back/Forward/Reload/Zoom, tab close, new tab) and
  announces their labels.
- macOS: focusing the address bar (e.g. Cmd+L) is announced as “Address bar” and selection changes
  (select-all) are announced.
- Windows: Narrator can traverse toolbar controls and announces their labels.
- Linux: Orca can traverse toolbar controls (when using X11/Wayland backend supported by your build).

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
  - Chrome UI + shortcut handling: [`src/ui/chrome.rs`](../src/ui/chrome.rs)
    - `chrome_ui` builds the tab strip + toolbar + address bar and returns `ChromeAction` values for
      the front-end to translate into worker messages. The windowed `browser` app calls this helper
      each egui frame.
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

### UI-owned global history semantics
 
In addition to per-tab worker-owned history, the UI maintains:

- `VisitedUrlStore` (in-memory, bounded) used for omnibox suggestions.
- `GlobalHistoryStore` (persisted) used for the History panel (and future internal pages like
  `about:history`).

`GlobalHistoryStore` lives in [`src/ui/global_history.rs`](../src/ui/global_history.rs).

Recording rules (kept explicit + regression-tested):

- Visits are recorded **only** on `WorkerToUi::NavigationCommitted` (not on started/failed).
- Redirects: record the final committed URL (the worker already reports this in
  `NavigationCommitted`).
- Fragments are stripped for history purposes:
  `https://example.com/page#section` → `https://example.com/page`.
- `about:` pages are not recorded (including `about:history` / `about:bookmarks`).
- `file:` URLs are recorded.
- The store is deduped by normalized URL; every committed navigation increments `visit_count` and
  updates `visited_at_ms` (the *last* visit timestamp, Unix epoch milliseconds; including
  back/forward/reload).

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
  - Note: `browser --js` is currently supported only for `--headless-smoke` (a vm-js `BrowserTab`
    smoke test); the windowed UI does not execute author scripts yet.
- **Interaction gaps** (non-JS): the windowed UI forwards pointer/keyboard input to the browser
  worker, which applies basic hit-testing + form interactions. Some interactions are still
  incomplete (e.g. select dropdown UI, rich text editing, complex focus traversal).
- **Limited form support** (non-JS):
  - text input is intentionally minimal
    - basic caret movement + selection are supported for focused `<input>`/`<textarea>` (arrow keys,
      Home/End, Shift+Arrow, Ctrl/Cmd+A)
    - basic clipboard shortcuts (Ctrl/Cmd+C/X/V) are supported for focused `<input>`/`<textarea>`
  - `<select>` support is basic (listbox clicks + dropdown popup selection; keyboard navigation and
    simple typeahead are supported; no multi-select yet)
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
