# Desktop browser UI (experimental)

FastRender has an experimental desktop “browser” binary at [`src/bin/browser.rs`](../src/bin/browser.rs).

This is **feature-gated** so the core renderer can compile without pulling in the heavy GUI stack.

For a higher-level overview of the `browser` binary (current capabilities, env vars, and how to run
it), see [browser.md](browser.md).

Note: today, the browser chrome (tabs/address bar/etc) is rendered via **egui**. The long-term
*renderer chrome* goal is to render the chrome UI using FastRender itself; when that lands, chrome
UI pages will use a privileged JS bridge (`globalThis.chrome`) documented in
[`docs/chrome_js_bridge.md`](chrome_js_bridge.md).

For an interim/bootstrap approach, renderer-chrome can also be made interactive **without any
JavaScript**, using trusted HTML/CSS and `chrome-action:` navigations/forms. See:
[`docs/renderer_chrome_non_js.md`](renderer_chrome_non_js.md).

Renderer-chrome also relies on privileged internal URL schemes (`chrome://` assets and
`chrome-action:` actions). These are documented in
[`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md).

For page accessibility status + developer workflow (a11y tree inspection, bounds mapping, and screen
reader testing), see [page_accessibility.md](page_accessibility.md).

## Build / run

The `browser` binary is behind the Cargo feature `browser_ui` (note the underscore) and is **not**
enabled by default.

Always use the repo wrappers (see [`AGENTS.md`](../AGENTS.md)) when building/running the browser UI:
`scripts/cargo_agent.sh` for Cargo invocations and `scripts/run_limited.sh` to apply resource limits.

The easiest entry point is the wrapper-safe `xtask browser` command, which already runs under
`scripts/run_limited.sh` and exposes a few common instrumentation toggles:

```bash
# Release browser with HUD + responsiveness/perf logging enabled:
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release --hud --perf-log about:test-layout-stress
```

To save the JSONL perf log and/or a browser UI trace to disk:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release \
  --perf-log --perf-log-out target/browser_perf.jsonl \
  --trace-out target/browser_trace.json \
  about:test-layout-stress
```

```bash
# Debug build:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser

# Release build:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

### Wayland (optional, Linux)

On Linux, `browser_ui` builds with the **X11** backend only (so minimal/CI hosts don't need Wayland
development packages). To build with both **X11 + Wayland** support, enable `browser_ui_wayland`:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,browser_ui_wayland --bin browser
```

`winit` selects the backend at runtime based on your environment (e.g. `WAYLAND_DISPLAY` / `DISPLAY`).
You can force a specific backend with:

```bash
# Force Wayland:
WINIT_UNIX_BACKEND=wayland timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,browser_ui_wayland --bin browser
```

If you run the `browser` binary without the feature, it will print a short message and exit
(the real implementation is behind the `browser_ui` feature gate; see
[`src/bin/browser.rs`](../src/bin/browser.rs)).

### Audio output (optional)

Audio output backends are opt-in so CI/minimal hosts don't need system audio development packages.

- Default: `NullAudioBackend` (silence), no system deps.
- Real-time audio output: enable `audio_cpal` (may require ALSA dev packages on Linux).

On Ubuntu/Debian, you may need:

```bash
sudo apt-get install libasound2-dev
```

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui,audio_cpal --bin browser
```

### Native dialogs (file/color) (developer note)

The `browser_ui` feature includes an optional dependency on the
[`rfd`](https://crates.io/crates/rfd) crate (pinned for MSRV) for native file/color dialogs, but the
current windowed `browser` app does **not** open native dialogs yet.

Instead:

- `<input type=file>` opens a basic **in-app file picker** popup (a text box where you paste/enter
  one or more local filesystem paths; for `multiple` inputs separate paths with `;`).
- OS drag-and-drop onto `<input type=file>` is also supported.
- `<input type=color>` does not have a picker UI yet.

On Linux, `rfd` is configured to use the `xdg-portal` backend (via Cargo feature selection) so
`--features browser_ui` stays **CI-friendly** and does **not** require GTK development packages
(`libgtk-3-dev`, etc).

If/when native dialogs are wired up (e.g. via `rfd`), note that the portal backend requires an
`xdg-desktop-portal` implementation to be running. If your environment doesn't provide one (common
on minimal/headless setups), dialogs may fail to open at runtime, but the browser UI will still
compile and run (including headless CI smoke tests).

When running the browser UI against arbitrary real-world pages, consider using the repo’s resource
limit wrapper (especially on multi-agent hosts):

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

The `browser` binary also supports an in-process, best-effort address-space cap via
`browser --mem-limit-mb <MB>` or `FASTR_BROWSER_MEM_LIMIT_MB` (see [env-vars.md](env-vars.md)).

The download directory can be configured via `browser --download-dir <path>` or
`FASTR_BROWSER_DOWNLOAD_DIR=<path>`.
Legacy note: some older test setups may still use `FASTR_DOWNLOAD_DIR` as an alias; prefer
`FASTR_BROWSER_DOWNLOAD_DIR` in new code/scripts.

Downloaded files are saved into a per-browser download directory resolved in this order:

1. `browser --download-dir <path>`
2. `FASTR_BROWSER_DOWNLOAD_DIR=<path>`
3. The OS downloads directory (via `directories::UserDirs`)
4. The current working directory

This resolved directory is also the one opened by the Downloads panel’s **Show downloads folder**
action.

To prevent huge in-process pixmap allocations when the window is resized to extreme sizes (or when
running on very high-DPI displays), the browser UI also clamps viewport/DPR based on these env vars
(see [env-vars.md](env-vars.md) for defaults and details):

- `FASTR_BROWSER_MAX_PIXELS`
- `FASTR_BROWSER_MAX_DIM_PX`
- `FASTR_BROWSER_MAX_DPR`

For smoother interactive window drags (resize), the browser UI also supports temporarily reducing
render-worker load by downscaling DPR during resize bursts:

- `FASTR_BROWSER_RESIZE_DPR_SCALE` (defaults to `0.5`; set `1.0` to disable).
If the windowed UI fails to start due to `wgpu` adapter/device creation issues (common under remote
desktop, VMs, or systems without a working GPU stack), you can force a software adapter and/or
backend:

- `browser --force-fallback-adapter` (alias: `--wgpu-fallback`) / `FASTR_BROWSER_WGPU_FALLBACK=1`
- `browser --wgpu-backends gl` / `FASTR_BROWSER_WGPU_BACKENDS=gl`

For CI environments without a display/GPU, the `browser` entrypoint provides **test-only** headless
modes that exercise startup and UI↔worker wiring **without** creating a window or initialising
`winit`/`wgpu`:

- `browser --exit-immediately` / `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1`
  - Parses/apply startup env vars, then exits (used by env parsing tests).
- `browser --headless-smoke` / `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1`
  - Runs an end-to-end smoke test of the real `src/bin/browser.rs` entrypoint + the
    `UiToWorker`/`WorkerToUi` message protocol.
  - On success prints `HEADLESS_SMOKE_OK` to stdout.
  - JS variant: `browser --headless-smoke --js` runs a small vm-js `api::BrowserTab` smoke test and
    on success prints `HEADLESS_VMJS_SMOKE_OK` (in `--headless-smoke` mode, `--js` selects the vm-js
    harness).
- `browser --headless-crash-smoke` / `FASTR_TEST_BROWSER_HEADLESS_CRASH_SMOKE=1`
  - Runs a smoke test that intentionally crashes the renderer worker and validates that the crash is
    contained/observable (future: renderer *process* crash isolation).
  - On success prints `HEADLESS_CRASH_SMOKE_OK` to stdout.

For manual crash-recovery testing, the browser also supports a separate opt-in flag that allows the
address bar / CLI to accept `crash://` URLs:

- `browser --allow-crash-urls` / `FASTR_BROWSER_ALLOW_CRASH_URLS=1`
  - This only allowlists the scheme for typed navigations. The worker crash triggers themselves are
    still disabled unless explicitly enabled (see `FASTR_ENABLE_CRASH_URLS` in [env-vars.md](env-vars.md)).

Run smoke modes under the repo’s resource-limit wrapper:

```bash
# Headless “does it start / is UI↔worker wired up” smoke test:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke

# JS smoke test (vm-js `BrowserTab` execution path):
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke --js

# Headless “renderer crash shouldn’t take down the browser” smoke test:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-crash-smoke
```

See [env-vars.md](env-vars.md) for test-only overrides (session/bookmarks/history JSON injection).

### GPU / wgpu adapter selection

The windowed `browser` UI uses `wgpu` for presentation. Adapter selection can vary across drivers
and environments (VMs, headless CI, Remote Desktop, etc). The `browser` CLI exposes a few flags to
help with debugging and forcing a specific selection strategy:

- `--power-preference {high,low,none}` — maps to `wgpu::PowerPreference`.
  - `high` prefers a high-performance/discrete GPU (default).
  - `low` prefers an integrated/low-power GPU.
  - `none` leaves the decision up to wgpu/the platform.
- `--force-fallback-adapter` (alias: `--wgpu-fallback`) — maps to
  `RequestAdapterOptions.force_fallback_adapter` (useful for software adapters).
- `--wgpu-backends <list>` — restrict the backend set used to create the wgpu instance (comma
  separated), e.g. `--wgpu-backends vulkan,gl`.

Troubleshooting tips:

- If windowed startup fails with a wgpu adapter selection error, try a different backend (for
  example `--wgpu-backends gl`) or `--force-fallback-adapter`.
- If you're in a headless environment without a working display/GPU, use `--headless-smoke` (or
  `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1`) to run a minimal startup smoke test without winit/wgpu.
  `--headless-crash-smoke` is a companion mode intended to validate crash isolation.
- Navigate to `about:gpu` to see the selected adapter/backend and the selection options used.

CI note: the main GitHub Actions workflow (`ci.yml`) compiles the `browser` binary with
`--features browser_ui` on Linux/macOS/Windows; Linux additionally runs the headless smoke mode
(and may run crash-smoke as multiprocess isolation work lands).

Note: startup/session restore:

- When run **without** a URL, the windowed `browser` app tries to restore the previous session
  (windows + tabs + per-tab zoom + best-effort scroll restoration).
  - If the previous run ended unexpectedly (unclean exit) **and the session is restored**, the
    active window shows a crash-recovery infobar/toast (including a **Start new session** option).
  - If repeated unclean exits are detected (crash loop), the browser may skip auto-restoring tabs and
    start with a “safe” `about:newtab` instead. Use `--restore` to force restoring anyway.
- When run **with** a URL, it opens that URL and does not restore tabs unless `--restore` is
  provided.
- `--no-restore` disables tab/session restore even when no URL is provided.
- Even when tabs are not restored (CLI URL or `--no-restore`), the browser may still reuse persisted
  **configuration** from the previous session (appearance/UI scale, home page, menu bar visibility,
  and window geometry) when available.

If no session file exists yet, it falls back to `about:newtab`, which acts as a basic start page
(showing bookmarks + recently visited pages when available).

## Appearance

The windowed browser UI exposes a small set of appearance/accessibility knobs (theme, accent color,
UI scale, high-contrast, reduced motion) via an in-app **Appearance** popup (gear icon in the
toolbar).

These settings are persisted in the browser session file so they survive restarts.

Environment variables like `FASTR_BROWSER_THEME=...` and `FASTR_BROWSER_ACCENT=...` are still supported
as overrides (useful for scripting/CI); see [env-vars.md](env-vars.md).

### Theme mode selection

`FASTR_BROWSER_THEME=system|light|dark` controls the browser chrome theme:

- `system` (default): follow the OS light/dark preference when available.
- `light` / `dark`: force a specific theme.

Interaction with rendered pages:

- The browser UI preference also drives the default `prefers-*` media query surface for page
  rendering:
  - Theme (resolved light/dark, including `system`) → `prefers-color-scheme` (`light`/`dark`).
  - High contrast → `prefers-contrast: more`.
  - Reduced motion → `prefers-reduced-motion: reduce`.
- Explicit renderer overrides (`FASTR_PREFERS_COLOR_SCHEME`, `FASTR_PREFERS_CONTRAST`,
  `FASTR_PREFERS_REDUCED_MOTION`) take precedence.

### Accent color

The browser chrome uses an accent color for links, focus rings, and selection.

- The in-app Appearance popup includes a few accent presets and a custom color picker; the selected
  accent is persisted in the session file.
- `FASTR_BROWSER_ACCENT=<hex>` overrides the accent color (useful for scripting/CI; takes precedence
  over the persisted setting). See [env-vars.md](env-vars.md) for accepted formats.

### High contrast / reduced motion

- `FASTR_BROWSER_HIGH_CONTRAST=1` enables a higher-contrast chrome theme and stronger
  focus indicators.
  - Pages see `prefers-contrast: more` by default unless explicitly overridden via
    `FASTR_PREFERS_CONTRAST=...`.
- `FASTR_BROWSER_REDUCED_MOTION=1` reduces/disables non-essential UI animations.
  - Pages see `prefers-reduced-motion: reduce` by default unless explicitly overridden via
    `FASTR_PREFERS_REDUCED_MOTION=...`.

### UI scale vs page zoom

- **UI scale** (`FASTR_BROWSER_UI_SCALE=<float>`) scales the browser chrome UI (tabs,
  toolbar, fonts) without changing the page zoom level.
- **Page zoom** is currently implemented as a per-tab setting:
  - shortcuts: Ctrl/Cmd +/-/0, and Ctrl/Cmd + mouse wheel.
  - behaviour: scales the CSS viewport size + DPR (keeps the drawn pixmap size roughly constant
    while making content larger/smaller).

### HUD / debug overlays

- HUD overlay:
  - `browser --hud` (or `FASTR_BROWSER_HUD=1`) shows an in-app HUD overlay with browser/debug metrics
    (FPS / frame time, frame queue/backpressure stats, and when enabled: input/resize latency + CPU
    usage summaries).
  - `browser --no-hud` force-disables the HUD (overrides `FASTR_BROWSER_HUD`).
- `FASTR_BROWSER_LOG_SURFACE_CONFIGURE=1` logs `wgpu::Surface::configure` calls to stderr (useful
  when debugging interactive resize performance; swapchain reconfiguration should be coalesced).
- Debug log UI:
  - In **debug** builds it is enabled by default.
  - In **release** builds it is disabled by default:
    - set `FASTR_BROWSER_DEBUG_LOG=1` to enable it at startup, or
    - enable it at runtime via the menu bar: **View → Debug log**.

### Performance / responsiveness

For profiling the *responsiveness* of the windowed UI (frame pacing, jank, and UI↔worker latency),
there is dedicated tooling beyond the renderer’s `FASTR_RENDER_TIMINGS` / tracing knobs:

- `FASTR_PERF_LOG=1` enables **windowed JSONL performance logging**.
  - Intended to make UI regressions measurable (e.g. per-frame time, input/resize→present latency,
    navigation TTFP, and idle CPU usage / busy-loop behavior).
  - Use this when investigating “the UI feels laggy” problems (dropped frames, slow resize, slow
    typing/click feedback), and prefer `--release` builds for realistic numbers.
  - CLI equivalents:
    - `browser --perf-log` emits events to stdout (overrides `FASTR_PERF_LOG` and forces stdout even if `FASTR_PERF_LOG_OUT` is set).
    - `browser --perf-log-out <path>` writes events to a file instead of stdout (creates parent directories; overrides env vars).
  - `FASTR_PERF_LOG_OUT=/path/to/log.jsonl` can be used to write events to a file instead of stdout.
- `ui_perf_smoke` is a **headless** UI responsiveness harness.
  - Use it for quick local checks and for CI-style regression tests where you can’t (or don’t want
    to) open a real window/GPU-backed swapchain.
- `browser_perf_log_summary` summarizes a captured perf log into p50/p95/max numbers.
- `scripts/capture_browser_perf_log.sh` wraps an interactive windowed run (under `run_limited`) and
  tees the stdout JSONL stream to a file. Pass `--summary` to run `browser_perf_log_summary`
  automatically (human summary to stderr; JSON output suppressed so stdout stays JSONL-only).
- `scripts/profile_browser_samply.sh` records an interactive Linux CPU profile (Samply) for the
  windowed browser UI.

See [perf-logging.md#browser-responsiveness](perf-logging.md#browser-responsiveness) (and the
“Measuring browser responsiveness” section below) for full details.

### Persistence (session file)

The browser persists a lightweight session file for restoring state across restarts.

This file acts as both:

- **Session restore** (tabs/windows), and
- **Persisted configuration** (appearance/UI scale, menu bar visibility, home page, window geometry).

Even when the browser starts with *fresh* tabs (for example because you launched with a CLI URL or
`--no-restore`), it may still reuse these persisted configuration fields from the last session.

- Default location: a per-user config directory (via `directories`), e.g.
  `~/.config/fastrender/fastrender_session.json` on Linux.
- Override:
  - CLI: `browser --session-path /path/to/fastrender_session.json`
  - Env: `FASTR_BROWSER_SESSION_PATH=/path/to/fastrender_session.json` (CLI takes precedence)
- Fallback (if the OS config directory cannot be determined): `./fastrender_session.json`.

Sidecar files:

- **Lock file:** to avoid corrupting the session file, the `browser` process also acquires a lock
  file for the session path. If another `browser` process is already running with the same session
  path, a second instance will refuse to start. Use `--session-path` (or `FASTR_BROWSER_SESSION_PATH`)
  to run multiple isolated instances.
- **Backup file (`*.bak`):** the browser retains a last-known-good backup of the session file
  (for example `fastrender_session.json.bak` next to `fastrender_session.json`). If the primary
  session file is corrupted/unparseable, the backup can be used to recover.
  - This backup is updated on overwrite when the existing session parses successfully; it is intended
    as a recovery path for corruption or manual edits.

The session file format is versioned (currently v2) and includes:

- One or more windows (each with tabs + active tab index)
- Per-tab zoom
- Best-effort per-tab scroll restoration
- Pinned tabs and tab groups (when used)
- Per-window menu bar visibility (`show_menu_bar`)
- The configured home page URL
- Best-effort window geometry (position/size/maximized) when available
  - When a window is maximized, the persisted **width/height** represent the last *normal* (restored)
    size so “unmaximize” returns to the expected size.
  - When a window is minimized, the browser avoids writing meaningless `0×0` window sizes (it keeps
    the last known non-zero geometry).
- A crash marker (`did_exit_cleanly`) + crash-loop streak (`unclean_exit_streak`) for detecting
  unclean exits and breaking restore crash loops
- Appearance settings (theme mode, accent color, high contrast, reduced motion, UI scale)

The windowed `browser` app uses a background autosave helper (`SessionAutosave` in
[`src/ui/session_autosave.rs`](../src/ui/session_autosave.rs), wired up by the entrypoint in
[`src/bin/browser.rs`](../src/bin/browser.rs)) so session writes do not block the UI thread:

- **Crash marker / crash-loop breaker:** on startup, the browser immediately persists
  `did_exit_cleanly=false` and increments `unclean_exit_streak`. If the process is terminated
  unexpectedly, these values remain on disk so the next launch can detect the unclean exit.
  - UX (when restoring an unclean session): the active window shows a crash-recovery infobar/toast
    (including a **Start new session** option that discards restored tabs across all windows).
  - Crash-loop breaker: after a threshold number of consecutive unclean exits, the browser may skip
    auto-restoring tabs and start with a safe new tab instead (use `--restore` to force restoring
    anyway).
- **Background autosave:** while the browser is running, it snapshots the current session and
  schedules a debounced background save on “significant” state changes (for example tab navigations,
  tab/window creation/closure, zoom/appearance changes, and window geometry changes).
- **Clean shutdown:** on a normal exit the final snapshot is written with `did_exit_cleanly=true` and
  `unclean_exit_streak=0`.

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
  - Alt+Enter (Win/Linux) / Option+Enter (macOS) opens the resolved navigation in a **new tab**.
- While typing, the omnibox shows a suggestions dropdown (from open tabs, recently closed tabs,
  bookmarks, history, built-in `about:` pages, and remote search suggestions when available).
  - Use ArrowUp/ArrowDown to select a suggestion, Enter to accept, Escape to close the dropdown.
  - Remote suggestions (when available) are fetched asynchronously using DuckDuckGo’s autocomplete
    endpoint.
- Tabs:
  - Drag tabs in the tab strip to **reorder** them.
  - Right-click a tab to open a tab context menu (reload/duplicate, pin/unpin, tab grouping, close
    other tabs, close tabs to the right).
  - Drag a tab out of the tab strip to **detach** it into a new window.
- Drag a link from page content and drop it onto the address bar to navigate (Chrome-like UX).
- Hovering a link shows its destination URL in a small bottom-left overlay bubble (modern browser
  style).
- Right-clicking in the rendered page opens a basic context menu (context-sensitive; can include:
  open link/image in a new tab, copy link/image address, download link/image, reload, and basic
  clipboard/editing actions like copy/cut/paste/select all).
- Click the downloads icon in the toolbar to open the downloads side panel (shows progress and lets
  you cancel/retry/open/reveal completed downloads).

### Downloads

- Open the downloads panel via the toolbar downloads icon, <kbd>Ctrl</kbd>+<kbd>J</kbd> (Win/Linux),
  <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>J</kbd> (macOS), or **Window → Show Downloads…**.
- For completed downloads:
  - **Open** launches the file using the OS default application.
  - **Show in Folder** reveals the file in your OS file manager.
- Use **Clear completed** (panel header) to remove completed entries from the list.
- **Show downloads folder** opens the currently configured download directory (as resolved from
  `--download-dir` / `FASTR_BROWSER_DOWNLOAD_DIR` / OS Downloads / working directory).

## Keyboard / mouse shortcuts

| Shortcut | Action |
|---|---|
| Ctrl/Cmd+L | Focus address bar (select all) |
| Ctrl/Cmd+K | Focus address bar (select all) |
| F6 | Focus address bar (select all) |
| Alt+D (Win/Linux) | Focus address bar (select all) |
| Alt+Enter (address bar, Win/Linux); Option+Enter (address bar, macOS) | Open omnibox input in a new tab |
| Ctrl/Cmd+N | New window |
| F11 (Win/Linux); Ctrl+Cmd+F (macOS) | Toggle full screen |
| Ctrl/Cmd+F | Find in page |
| Ctrl+J (Win/Linux); Cmd+Shift+J (macOS) | Toggle downloads panel |
| Ctrl/Cmd+T | New tab |
| Ctrl/Cmd+Shift+T | Reopen last closed tab |
| Ctrl/Cmd+Shift+A | Search tabs / quick switcher |
| Ctrl/Cmd+W | Close active tab (no-op if only one tab) |
| Ctrl/Cmd+F4 | Close active tab (no-op if only one tab) |
| Ctrl+Tab | Next tab (Cmd+Tab is reserved by the macOS app switcher) |
| Ctrl+Shift+Tab | Previous tab (Cmd+Shift+Tab is reserved by the macOS app switcher) |
| Ctrl/Cmd+1..9 (9 = last tab) | Activate tab by number |
| Ctrl/Cmd+PageUp | Previous tab |
| Ctrl/Cmd+PageDown | Next tab |
| Alt+Left (Win/Linux) | Back |
| Alt+Right (Win/Linux) | Forward |
| Cmd+[ (macOS) | Back |
| Cmd+] (macOS) | Forward |
| Ctrl/Cmd+R / F5 | Reload |
| Ctrl/Cmd+S | Save page (reserved; not implemented yet) |
| Ctrl/Cmd+P | Print page (reserved; not implemented yet) |
| Esc (while loading) | Stop loading |
| Alt+Home (Win/Linux); Cmd+Shift+H (macOS) | Home page |
| Ctrl+D (Win/Linux); Cmd+D (macOS) | Toggle bookmark for current page |
| Ctrl+H (Win/Linux); Cmd+Y (macOS) | Toggle history panel |
| Ctrl/Cmd+Shift+Delete | Open “Clear browsing data” dialog |
| Ctrl+Shift+O (Win/Linux); Cmd+Shift+O (macOS) | Toggle bookmarks manager |
| Ctrl/Cmd+Shift+B | Toggle bookmarks bar |
| Ctrl/Cmd+Plus / Ctrl/Cmd+Equals | Zoom in |
| Ctrl/Cmd+Minus | Zoom out |
| Ctrl/Cmd+0 | Reset zoom |
| Ctrl/Cmd + Mouse Wheel | Zoom in/out |
| Middle-click tab | Close tab (no-op if only one tab) |
| Ctrl/Cmd+Click link | Open link in new tab |
| Middle-click link | Open link in new tab |
| Mouse Back / Mouse Forward (buttons 4/5 on Windows/macOS, 8/9 on X11) | Back / Forward |
| Shift+F10 (focused element); Apps/Menu key (page focus, Win/Linux) | Open context menu |
| PageUp (page focus) | Scroll up |
| PageDown (page focus) | Scroll down |
| ArrowUp (page focus, no element focused) | Scroll up |
| ArrowDown (page focus, no element focused) | Scroll down |
| Space (page focus, no element focused) | Scroll down |
| Shift+Space (page focus, no element focused) | Scroll up |
| Home (page focus, no element focused) | Scroll to top |
| End (page focus, no element focused) | Scroll to bottom |
| Ctrl/Cmd+Z (page focus) | Undo in the focused page `<input>`/`<textarea>` |
| Ctrl+Shift+Z (page focus, Win/Linux); Cmd+Shift+Z (page focus, macOS); Ctrl+Y (page focus, Win/Linux) | Redo in the focused page `<input>`/`<textarea>` |
| Ctrl/Cmd+A (page focus) | Select all text in the focused page `<input>`/`<textarea>` |
| Ctrl/Cmd+C (page focus) | Copy selection from the focused page `<input>`/`<textarea>` to the OS clipboard |
| Ctrl/Cmd+X (page focus) | Cut selection from the focused page `<input>`/`<textarea>` to the OS clipboard |
| Ctrl/Cmd+V (page focus) | Paste OS clipboard text into the focused page `<input>`/`<textarea>` |
| Ctrl+Insert (page focus, Win/Linux) | Copy selection from the focused page `<input>`/`<textarea>` |
| Shift+Insert (page focus, Win/Linux) | Paste OS clipboard text into the focused page `<input>`/`<textarea>` |
| Shift+Delete (page focus, Win/Linux) | Cut selection from the focused page `<input>`/`<textarea>` |

Notes:

- Zoom is tracked per-tab and persisted in the browser session file (see `src/ui/session.rs`).
- Ctrl/Cmd+S and Ctrl/Cmd+P are reserved by browser chrome: they currently show a “not implemented
  yet” toast and are not forwarded to the rendered page.
- macOS: <kbd>Cmd</kbd>+<kbd>Tab</kbd> is reserved by the OS (app switcher) and generally won’t reach
  the app. Tab cycling is expected to work via <kbd>Ctrl</kbd>+<kbd>Tab</kbd> /
  <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Tab</kbd>.
- Windows/Linux: some keyboard layouts expose **AltGr** as <kbd>Ctrl</kbd>+<kbd>Alt</kbd>. The
  shortcut mapper intentionally ignores Ctrl+Alt+… combinations to avoid breaking text entry (see
  `src/ui/shortcuts.rs`).
- Shift+F10 opens the context menu for the currently focused surface:
  - page focus → page context menu
  - focused tab group chip → tab group context menu (rename/color/ungroup)

For a cross-platform **quick manual smoke matrix** (shortcuts + UX parity), see
[`docs/chrome_test_matrix.md`](chrome_test_matrix.md).

For a cross-platform, end-to-end **manual regression checklist** (address bar, tabs, downloads,
menus, session restore, accessibility), see
[`docs/browser_chrome_manual_test_matrix.md`](browser_chrome_manual_test_matrix.md).

## Menu bar

The windowed `browser` UI can optionally show a browser-style **in-window** menu bar for
discoverability and keyboard parity:

- Default:
  - **macOS:** hidden (native apps typically use the system menu bar)
  - **Other platforms:** shown
- Toggle at runtime via the toolbar hamburger menu: **Menu → Window → “Show menu bar”**.
- The setting is persisted in the browser session file so it survives restarts.
  - CI override: `FASTR_BROWSER_SHOW_MENU_BAR=0|1` (takes precedence for the current process, but does
    **not** update the persisted session preference).

When hidden, the menu bar does not reserve any vertical space; keyboard shortcuts continue to work.

- **File**
- **Edit**
- **View**
- **History**
- **Bookmarks**
- **Window**
- **Help**

Implemented items are wired up to existing browser UI actions (tabs, navigation, reload, zoom,
clipboard, panels, find-in-page).

Some menu entries are still placeholders and remain disabled (for example **File → Save Page…** and
**File → Print…**).

Enabled items (as of now):

- **File:** New Tab, Close Tab, Quit (Save Page… / Print… are present but disabled placeholders)
- **Edit:** Undo/Redo (chrome text inputs), Cut/Copy/Paste, Select All, Find in Page
- **View:** Reload, Zoom In/Out/Reset, Debug log toggle, Toggle Full Screen
- **History:** Back/Forward, Reopen Closed Tab, toggle History panel
- **Bookmarks:** Bookmark This Page / Remove Bookmark, toggle Bookmarks panel, Bookmark manager…
- **Window:** New Window, Show Downloads…
- **Help:** Help, About FastRender

Help/About items open `about:help` / `about:version` in a new tab.

## Bookmarks / History

FastRender’s experimental desktop browser UI supports **bookmarks** and a basic **history** panel.

- **Bookmarking**:
  - Click the **star** button in the toolbar (or press <kbd>Ctrl</kbd>+<kbd>D</kbd> on Win/Linux, or
    <kbd>Cmd</kbd>+<kbd>D</kbd> on macOS) to toggle a bookmark for the current page.
  - Bookmarks appear in the **bookmarks bar** for quick access.
  - Drag bookmarks in the bar to **reorder** them (or use the per-bookmark menu to move left/right).
  - Use the **bookmarks manager** (<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd> on Win/Linux, or
    <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd> on macOS) to browse/remove bookmarks.
- **History**:
  - Open the history panel with <kbd>Ctrl</kbd>+<kbd>H</kbd> (Win/Linux) or <kbd>Cmd</kbd>+<kbd>Y</kbd>
    (macOS).
  - The history UI includes **search** and a **Clear browsing data…** action (with a time range).
- **Persistence**:
  - Bookmarks and history are stored as JSON files under the per-user FastRender config directory
    (for example `~/.config/fastrender/` on Linux).
    - Fallback (if the OS config directory cannot be determined): `./fastrender_bookmarks.json` and
      `./fastrender_history.json`.
  - Override the default paths with:
    - `FASTR_BROWSER_BOOKMARKS_PATH`
    - `FASTR_BROWSER_HISTORY_PATH`

## Accessibility

The windowed `browser` UI exposes accessibility information via **AccessKit** so platform screen
readers (VoiceOver/Narrator/Orca) can traverse both the browser chrome and (experimental) page
content.

Note: the page is still *visually* rendered as a pixel buffer (pixmap); the page content
accessibility subtree is a separate semantic tree. Injecting that page subtree into the OS-facing
AccessKit tree is still in progress (see “Current limitations” below).

Accessibility sources:

- **Chrome widgets (egui):** tabs, toolbar buttons, address bar, menus/panels/popups are exposed via
  egui-winit + AccessKit when compiled with `--features browser_ui`.
- **Page content (renderer tree):** page content accessibility can be exposed by injecting an
  AccessKit subtree derived from the renderer’s accessibility tree (`AccessibilityNode`, via
  `src/accessibility.rs`). The render worker can emit a live `WorkerToUi::PageAccessibility` snapshot
  (semantic tree + best-effort bounds in viewport-local CSS pixels), stored in
  `ui::browser_app::PageAccessibilitySnapshot`. Per-element OS-facing page content exposure is still
  in progress (see [page_accessibility.md](page_accessibility.md)).

When wiring up a page subtree, AccessKit node IDs are expected to be derived from `(tab_id,
dom_node_id)` (see `ui::page_accesskit_ids`) so OS action requests can be routed back to the correct
tab + DOM node.

For background and developer workflow:

- Renderer page semantics (`dump_a11y`, bounds mapping): [page_accessibility.md](page_accessibility.md).
- AccessKit plumbing + debugging guide (updates, id stability, coordinate systems):
  [chrome_accessibility.md](chrome_accessibility.md).

Debugging tip: to inspect the **egui-produced** AccessKit update (chrome widgets) without running a
screen reader, use the `dump_accesskit` CLI (requires `--features browser_ui`).

Note: `dump_accesskit` does not run the browser worker, so it does **not** include any
worker-produced page accessibility snapshot (or any injected page subtree); use the real windowed
`browser` + a platform accessibility inspector to debug page nodes.

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- --help
```

Current limitations (MVP / in-progress):

- **Page subtree injection is not complete:** depending on the build/runtime configuration, the OS
  accessibility tree may contain only the egui chrome widgets + a single labeled page region (pixmap),
  without per-element page semantics.
- **Action support is evolving:** chrome widgets support focus/activate via egui. The worker also
  supports additional page actions such as **scroll into view**, **set value** (basic form controls),
  and **set text selection** (text inputs), but full parity with platform/AT actions is still in
  progress.
- **Bounds/geometry may be missing or approximate** for page nodes once exposed, which can affect
  hit-testing and “click this element” style commands.
- **Selection/value reporting may be partial** for some controls (e.g. caret/selection state or
  text values may be missing/incomplete).
- **Tree updates are coarse:** page accessibility updates are currently best-effort snapshots (not
  JS-driven live region updates) and may lag behind visual updates on complex pages.

Manual testing checklist (smoke):

- **macOS (VoiceOver):**
  - Enable VoiceOver (Cmd+F5).
  - Verify you can traverse chrome controls and the address bar is announced as “Address bar”
    (e.g. after Cmd+L).
  - Navigate to a simple page (e.g. `about:test-form`) and verify the page region is discoverable
    and has a meaningful label (currently: “Web page content (rendered image)”).
  - If your build includes injected page semantics, verify VoiceOver can traverse basic document
    content (headings/links/form controls) and trigger focus/activate on at least one control.
- **Windows (Narrator):**
  - Enable Narrator (Ctrl+Win+Enter).
  - Verify basic traversal/announcement works for chrome controls and the page region.
  - If your build includes injected page semantics, verify focus/activate works on at least one page control.
- **Linux (Orca):**
  - Enable Orca (often Super+Alt+S).
  - Verify basic traversal/announcement works for chrome controls and the page region (backend
    support depends on your `winit`/X11/Wayland environment).
  - If your build includes injected page semantics, verify focus/activate works on at least one page control.

If you need a lower-level view than screen reader announcements, use a platform accessibility
inspector (e.g. macOS Accessibility Inspector, Windows Inspect.exe, Linux Accerciser) to confirm
the chrome widget nodes and the page region/subtree structure.

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
  - Renders a date/time picker popup for `<input type=date|time|datetime-local|month|week>`.
    - Workers request this via `WorkerToUi::{DateTimePickerOpened,DateTimePickerClosed}`; the UI
      responds with `UiToWorker::{DateTimePickerChoose,DateTimePickerCancel}`.
  - Renders a file picker popup for `<input type=file>`.
    - Workers request this via `WorkerToUi::{FilePickerOpened,FilePickerClosed}`; the UI responds
      with `UiToWorker::{FilePickerChoose,FilePickerCancel}`.
  - Includes a test-only headless smoke mode (see `FASTR_TEST_BROWSER_HEADLESS_SMOKE` in
    [env-vars.md](env-vars.md)).
- Browser UI core (tabs/history model, cancellation helpers, worker wrapper):
  [`src/ui/`](../src/ui/)
  - UI state model (`BrowserAppState`/tabs/chrome): [`src/ui/browser_app.rs`](../src/ui/browser_app.rs)
  - Chrome UI + shortcut handling: [`src/ui/chrome.rs`](../src/ui/chrome.rs)
    - `chrome_ui` builds the tab strip + toolbar + address bar and returns `ChromeAction` values for
      the front-end to translate into worker messages. The windowed `browser` app calls this helper
      each egui frame.
- About pages (`about:blank`, `about:newtab`, `about:settings`, `about:error`, `about:help`,
  `about:version`, `about:gpu`, `about:processes`, `about:history`, `about:bookmarks`,
  `about:test-scroll`, `about:test-heavy`, `about:test-layout-stress`, `about:test-form`):
  [`src/ui/about_pages.rs`](../src/ui/about_pages.rs)
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

Multiprocess note (roadmap): today the “worker” is an in-process thread, but the long-term plan is to
move the content renderer (and eventually JS) into a separate sandboxed process for crash/security
isolation. The intended seam is the `UiToWorker`/`WorkerToUi` protocol
([`src/ui/messages.rs`](../src/ui/messages.rs)) and the worker spawn helpers in
[`src/ui/render_worker.rs`](../src/ui/render_worker.rs) (`spawn_browser_worker` /
`spawn_browser_ui_worker`). The headless smoke modes (`--headless-smoke` / `--headless-crash-smoke`)
are intended to remain stable as that swap happens.

For OS sandboxing details (including the Windows AppContainer + Job Object design and renderer
debug escape hatches), see [sandboxing.md](sandboxing.md).

Windows debugging tip: set `FASTR_LOG_SANDBOX=1` for verbose AppContainer/restricted-token spawn
logs.
If you need the sandboxed child to inherit the full parent environment on Windows (disabling the
default environment sanitization / `TEMP`/`TMP` override), set `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1`
(debug only).

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
- `Tick { tab_id, delta }` — periodic wake-up used to advance time-based effects and drive the tab
  event loop (CSS animations/transitions, animated images, JS timers/`requestAnimationFrame`/`requestIdleCallback`, etc).
  Front-ends should schedule ticks for the active/visible tab using the `RenderedFrame.next_tick`
  hint reported on `WorkerToUi::FrameReady`.
  - `delta` is the time elapsed since the previous tick delivered for this tab (front-ends can
    derive it from wall-clock time, or inject a fixed `delta` in deterministic harnesses).
  - Tick is a wake-up signal (no absolute timestamp); time-aware subsystems must query their own
    clocks rather than inferring time from tick cadence (see
    [`docs/media_clocking.md`](media_clocking.md)).
  - Implementation note: the canonical UI worker advances CSS animation time using the `delta`
    provided in `Tick` (with a default requested cadence of `DEFAULT_TICK_INTERVAL` in
    [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)). If ticks are delayed/suppressed,
    animations will pause and then jump when ticks resume. Do not treat UI ticks as a master clock
    for media playback; see [`docs/media_clocking.md`](media_clocking.md).
  - Workers may coalesce back-to-back ticks by summing their deltas when overloaded.
- `ViewportChanged { tab_id, viewport_css, dpr }`
- `Scroll { tab_id, delta_css, pointer_css }`
- pointer/key/text events (`PointerDown/Up/Move`, `TextInput`, `KeyAction`)
- clipboard actions (`Copy`, `Cut`, `Paste`, `SelectAll`) for the focused page `<input>`/`<textarea>`
- find-in-page actions (`FindQuery`, `FindNext`, `FindPrev`, `FindStop`)
- `DropFiles { tab_id, pos_css, paths }` — OS file drop (used to populate `<input type=file>` controls)
- `SelectDropdownChoose { tab_id, select_node_id, option_node_id }` — user selected an option from
  a dropdown popup (sent after `WorkerToUi::SelectDropdownOpened`)
- `SelectDropdownCancel { tab_id }` — user dismissed a dropdown popup (Escape/click-away)
- `ContextMenuRequest { tab_id, pos_css }` — request hit-test/context for a page context menu
  invocation; the worker responds with `WorkerToUi::ContextMenu`.
- `DateTimePickerChoose { tab_id, input_node_id, value }` / `DateTimePickerCancel { tab_id }` —
  user interaction with a date/time picker popup for `<input type=date|time|datetime-local|month|week>`.
- `FilePickerChoose { tab_id, input_node_id, paths }` / `FilePickerCancel { tab_id }` — user
  interaction with a file picker popup for `<input type=file>`.
- Downloads: `SetDownloadDirectory`, `StartDownload`, `CancelDownload`

Coordinate convention: `pos_css` / `pointer_css` fields are **viewport-relative CSS pixels** (origin
at the top-left of the viewport). They must **not** include the current scroll offset; worker loops
add `scroll_state.viewport` when converting to page coordinates for hit-testing.

**Worker → UI** (`WorkerToUi`) includes:

- `FrameReady { tab_id, frame }` — a rendered `tiny_skia::Pixmap` + viewport/scroll metadata
- `HoverChanged { tab_id, hovered_url, cursor }` — hovered link URL + cursor updates (drives the
  status/URL bubble and the OS cursor icon).
- `RequestWakeAfter { tab_id, after, reason }` — request that the UI wake up after `after` and
  deliver a `UiToWorker::Tick` for this tab (often with `delta=Duration::ZERO`). This is primarily
  intended for tickless media/video frame deadlines so the UI can sleep in `ControlFlow::WaitUntil`
  without relying on a fixed 16ms tick.
- `Warning { tab_id, text }` — non-fatal, user-facing warnings (e.g. viewport/DPR clamping to avoid
  huge pixmap allocations); the windowed `browser` UI surfaces this as:
  - a small warning badge in the address bar, and
  - a transient warning toast overlay for the active tab.
- `OpenSelectDropdown { tab_id, select_node_id, control }` — legacy dropdown popup request for a
  `<select>` control (cursor-anchored; kept for back-compat with older UIs).
- `SelectDropdownOpened { tab_id, select_node_id, control, anchor_css }` — request the UI open a
  dropdown popup for a `<select>` control, with an explicit `anchor_css` in **viewport-local CSS
  pixels** so the popup can be positioned relative to the rendered frame.
- `SelectDropdownClosed { tab_id }` — close/dismiss any open dropdown popup for the tab
- `DateTimePickerOpened { tab_id, input_node_id, kind, value, anchor_css }` /
  `DateTimePickerClosed { tab_id }` — open/close a date/time picker popup for an `<input>` control.
- `FilePickerOpened { tab_id, input_node_id, multiple, accept, anchor_css }` /
  `FilePickerClosed { tab_id }` — open/close a file picker popup for an `<input type=file>` control.
- `ContextMenu { ... }` — response to `UiToWorker::ContextMenuRequest` (link/image under cursor +
  copy/cut/paste/select-all affordances + whether the page prevented the default menu).
- `NavigationStarted/Committed/Failed { ... }` — URL/title/back-forward state updates
- `Stage { tab_id, stage }` — coarse progress heartbeats forwarded from the renderer
  (`StageHeartbeat` from [`src/render_control.rs`](../src/render_control.rs))
  - Can be surfaced by chrome UIs while loading (e.g. [`src/ui/chrome.rs`](../src/ui/chrome.rs)).
- `ScrollStateUpdated { tab_id, scroll }` / `LoadingState { tab_id, loading }`
  - `ScrollStateUpdated` is emitted when the worker's scroll model changes, but it is **not**
    guaranteed to be produced for every navigation or every scroll input:
    - some navigations update scroll state only via the next `FrameReady` (no standalone scroll
      message),
    - clamped/no-op scroll requests may schedule a repaint without changing scroll state (so no
      scroll update),
    - scroll updates may be forwarded **before** the corresponding `FrameReady` so UIs can
      async-scroll the last painted texture while waiting for repaint.
  - `FrameReady.frame.scroll_state` is the canonical "painted" scroll state; UIs should not assume
    any specific ordering between `ScrollStateUpdated` and `FrameReady`.
- `FindResult { tab_id, query, case_sensitive, match_count, active_match_index }` — find-in-page
  match count + active match updates.
- `SetClipboardText { tab_id, text }` — request the UI update the OS clipboard (copy/cut).
- Downloads: `DownloadStarted` / `DownloadProgress` / `DownloadFinished` (used to drive the downloads
  panel UI).

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

#### Tick loop (`RenderedFrame.next_tick` / `UiToWorker::Tick`)

FastRender’s browser UI worker is **tick-driven**: it does not busy-poll “document time” on its own.
Front-ends drive time-based behavior by sending periodic
[`UiToWorker::Tick`](../src/ui/messages.rs) messages.

- The worker reports a per-frame scheduling hint: `RenderedFrame.next_tick: Option<Duration>` (on
  [`WorkerToUi::FrameReady`](../src/ui/messages.rs)).
  - `None` means the worker does not currently need ticks for the tab.
- While `next_tick` is `Some(delay)`, front-ends should send `Tick { tab_id, delta }` for the
  active/visible tab after approximately that delay.
  - The windowed `browser` app does this with a small scheduler in
    [`src/bin/browser.rs`](../src/bin/browser.rs), using `ControlFlow::WaitUntil` so it can animate
    without busy-polling the winit event loop.
  - Ticks are typically paused when the window is minimized/occluded/unfocused or during resize
    bursts. In these cases the browser resets the tick schedule, so time-based effects may appear to
    pause while the window is not actively being driven.
- A tick is the worker’s chance to run a bounded slice of time-based work (CSS
  animations/transitions, animated images, JS timers/`requestAnimationFrame`/`requestIdleCallback`, etc) and schedule a
  repaint if the page becomes dirty.
- `Tick` is a wake-up signal (no absolute timestamp). The UI provides a best-effort `delta` duration
  (elapsed since the previous tick for the tab) so the worker can advance CSS animation time;
  deterministic harnesses can inject a fixed `delta`. Time-aware subsystems (especially media) must
  still query their own clocks rather than inferring time from tick cadence; see
  [`docs/media_clocking.md`](media_clocking.md).
  - The canonical worker loop typically requests ~`DEFAULT_TICK_INTERVAL` by setting
    `RenderedFrame.next_tick` accordingly (see [`src/ui/render_worker.rs`](../src/ui/render_worker.rs)).
- Workers may coalesce back-to-back ticks by summing their deltas when overloaded.

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

### Built-in `about:` pages

FastRender includes several internal `about:` pages implemented in
[`src/ui/about_pages.rs`](../src/ui/about_pages.rs).

See [`docs/about_pages.md`](about_pages.md) for the canonical list and per-page expectations (notably
`about:settings` for runtime paths and `about:processes` for multiprocess/chrome debugging).

Quick note: `about:processes` is a *multiprocess/process-assignment placeholder* page — today it
shows a best-effort snapshot of open tabs (and a derived Site column), and will eventually show
real renderer/network process assignment as multiprocess work lands.

### Test-only `about:test-*` pages

For deterministic, offline repros (no network), the worker supports a few `about:` pages defined in
[`src/ui/about_pages.rs`](../src/ui/about_pages.rs):

- `about:test-scroll` — a simple tall page for scroll/viewport behavior.
- `about:test-heavy` — a large DOM intended to make cancellation/timeout behavior observable.
- `about:test-layout-stress` — a layout stress test page (intentionally heavy/degenerate layout).
- `about:test-form` — a minimal form for interaction/input testing.

These are used by the browser UI integration tests, but are also handy for manual debugging in the
windowed app.

### Worker debug logs

The worker can emit best-effort structured debug lines via `WorkerToUi::DebugLog { tab_id, line }`.
Front-ends are encouraged to print these to stderr while developing new protocol behavior.

### Measuring browser responsiveness (frame times / latency)

For machine-readable UI responsiveness metrics (frame times during scroll/resize, input latency, and
navigation TTFP), enable JSONL perf logging:

```bash
# Convenience wrapper: runs under run_limited, passes `browser --perf-log`, and tees stdout JSONL to a file.
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl

# Capture + summarize (runs `browser_perf_log_summary` after the browser exits):
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --summary --url about:test-layout-stress --out target/browser_perf.jsonl
```

Manual invocation (write perf JSONL directly to a file):

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
  --perf-log-out target/browser_perf.jsonl about:test-layout-stress
```

For automated/headless runs, use the `ui_perf_smoke` harness:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
```

See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness) for
details and metric mapping.

## Known limitations (as of now)

- **JavaScript execution is experimental:** the windowed UI worker maintains a
  JS-capable [`api::BrowserTab`](../src/api/browser_tab.rs) (vm-js executor) alongside the rendered
  `BrowserDocument`, runs bounded JS pumps (post-navigation, after DOM event dispatch, and on
  `Tick`), and best-effort syncs the JS tab’s `dom2` snapshot into the renderer DOM before painting.
  This is still incomplete (many Web APIs missing, lots of web-compat gaps). See
  [runtime_stacks.md](runtime_stacks.md) and [live_rendering_loop.md](live_rendering_loop.md).
  - CLI note: windowed mode currently runs author JS by default (no stable CLI toggle to disable it
    yet). `browser --js` is currently only meaningful in `--headless-smoke` mode, where
    `browser --headless-smoke --js` selects a vm-js `BrowserTab` smoke test.
  - Repaints are currently “whole frame” rerenders (no incremental damaged-rect compositor yet).
- **Interaction gaps:** the windowed UI forwards pointer/keyboard input to the browser
  worker, which applies basic hit-testing + form interactions. Some interactions are still
  incomplete (e.g. rich text editing, complex focus traversal).
  - Debugging tip: set `FASTR_LOG_INTERACTION_INVALIDATION=1` to log (stderr) whether each frame was paint-only vs needed a restyle/relayout due to interaction state changes (useful when dogfooding hover/focus performance).
- **Limited form support:**
  - text input is intentionally minimal
    - basic caret movement + selection are supported for focused `<input>`/`<textarea>` (arrow keys,
      Home/End, Shift+Arrow, Ctrl/Cmd+A)
    - Undo/Redo are supported for focused `<input>`/`<textarea>` (Ctrl/Cmd+Z; Ctrl+Shift+Z/Ctrl+Y)
    - basic clipboard shortcuts (Ctrl/Cmd+C/X/V) are supported for focused `<input>`/`<textarea>`
  - `<select>` support is basic (listbox clicks + dropdown popup selection; keyboard navigation and
    simple typeahead are supported; no multi-select yet)
  - `<input type=file>`:
    - Clicking the control opens a basic **in-app file picker** popup (enter a path; for `multiple`
      inputs separate paths with `;`).
    - OS drag-and-drop onto the control is supported (multi-file drops are coalesced to support
      `multiple`).
    - Native OS file chooser dialogs are not wired up yet.
  - many controls are not yet supported (`contenteditable`, etc.)
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

To reproduce locally:

```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config \
  libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxrandr-dev libxi-dev libxcursor-dev \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libegl1-mesa-dev libvulkan-dev
```

If you also enable real-time audio output via `--features audio_cpal`, you'll typically need the
ALSA development headers:

```bash
sudo apt-get install -y libasound2-dev
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
