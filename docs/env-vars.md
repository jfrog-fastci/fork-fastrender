# Runtime environment variables (`FASTR_*`)

FastRender has many internal debug/profiling toggles controlled via environment variables. These are intentionally lightweight (read at runtime) and primarily used by:

- `pageset_progress`
- `fetch_pages`
- `prefetch_assets`
- `render_pages`
- `fetch_and_render`
- `inspect_frag`

Pageset wrappers (`bash scripts/cargo_agent.sh xtask pageset`, `scripts/pageset.sh`, and the profiling scripts) enable the `disk_cache` cargo feature by default to reuse cached assets; set `DISK_CACHE=0` when invoking the scripts to opt out and force fresh fetches.

The rendering pipeline parses the environment once into a typed [`RuntimeToggles`](../src/debug/runtime.rs) structure. Library callers can override these values without mutating the process environment by constructing a `RuntimeToggles` instance and supplying it via `FastRender::builder().runtime_toggles(...)` or `RenderOptions::with_runtime_toggles(...)`.

Pageset/profiling runners typically invoke FastRender in `--release` mode, so `FASTR_*` toggles are the primary way to run controlled compatibility experiments (A/B against Chrome/pageset fixtures) without rebuilding.

## Renderer sandboxing

Renderer sandboxing (multiprocess security boundary) is documented in:

- [renderer_sandbox.md](renderer_sandbox.md) (entrypoint)
- [sandboxing.md](sandboxing.md) (cross-platform overview)
- [security/sandbox.md](security/sandbox.md) (Linux-focused design notes)

Platform-specific debug escape hatches are listed alongside their `FASTR_*` entries below (for
example `FASTR_DISABLE_RENDERER_SANDBOX` on Windows).

## Pageset disk cache tuning

These are parsed by the pageset CLI binaries (`prefetch_assets`, `render_pages`, `pageset_progress`, `fetch_and_render`) and wire into `DiskCacheConfig` when built with the `disk_cache` cargo feature.

- `FASTR_DISK_CACHE_MAX_BYTES=<bytes>` – on-disk subresource cache size limit (0 disables eviction; default 512MB).
- `FASTR_DISK_CACHE_MAX_AGE_SECS=<secs>` – cap cached entry age (0 disables age-based expiry; default 7 days).
- `FASTR_DISK_CACHE_LOCK_STALE_SECS=<secs>` – treat disk-cache `.lock` files older than this as stale and remove them (default 8 seconds).
- `FASTR_DISK_CACHE_ALLOW_NO_STORE=0|1` – allow persisting `Cache-Control: no-store` responses in the disk cache (default disabled).

CLI flag equivalents: `--disk-cache-max-bytes`, `--disk-cache-max-age-secs`, `--disk-cache-lock-stale-secs`, `--disk-cache-allow-no-store`.

`FASTR_DISK_CACHE_ALLOW_NO_STORE=1` can improve pageset determinism and offline behavior because
some critical CSS/font endpoints return `Cache-Control: no-store` even when the bytes are stable.
When enabled, FastRender will still treat these entries as always-stale (it will normally try a
network refresh), but cached bytes remain available as a fallback (and can be served immediately
under render deadlines when configured to serve stale responses).

When `no-store` persistence is enabled, FastRender also persists transient HTTP error responses
(notably 429/5xx) as always-stale entries so warm-cache pageset runs avoid repeatedly hammering
blocked endpoints. Non-deadline fetches still attempt a refresh.

## Commonly useful

- `FASTR_RENDER_TIMINGS=1` – print per-stage timings during rendering (parse/cascade/box_tree/layout/paint).
- `FASTR_LOG_INTERACTION_INVALIDATION=0|1` – log interaction invalidation decisions in `BrowserDocument` renders (used by renderer-chrome dogfooding).
  - Emits one line per `BrowserDocument::render_frame_with_deadlines_and_interaction_state` call with:
    - path: `paint_only` / `restyle_reuse_layout` / `restyle_relayout` / `full_prepare`
    - prev/new CSS + paint hashes and whether the layout fingerprint matched.
- `FASTR_FULL_PAGE=1` – expand output to the full document content size (instead of the viewport).
- `FASTR_USE_BUNDLED_FONTS=1` – disable system font discovery and use the bundled fixtures (default in CI).
- `FASTR_JS_CONSOLE_STDERR=0|1` – print JavaScript `console.*` output to stderr (opt-in; default off). Useful for local debugging when render diagnostics collection is disabled.
- `FASTR_WEB_FONT_WAIT_MS=<ms>` – wait up to `<ms>` for pending `@font-face` web font loads (notably `font-display: swap`) before layout/paint so offline renders use the intended web fonts.
  - Default: `0` in the core renderer (no extra wait; renders the pre-swap state).
  - Fixture tooling (`render_fixtures`, `xtask page-loop`) may set a small non-zero default; set this explicitly to override.
- `FASTR_BUNDLE_EMOJI_FONT=0|1` – explicitly enable/disable the bundled emoji font fixture (on by default in bundled mode/CI).
- `FASTR_FETCH_LINK_CSS=0` – skip fetching linked stylesheets from `<link>` elements (defaults to on; does not affect `@import` loads).
- `FASTR_FETCH_PRELOAD_STYLESHEETS=0|1` – control whether `<link rel=preload as=style>` entries are treated as stylesheet candidates (defaults to on).
- `FASTR_FETCH_MODULEPRELOAD_STYLESHEETS=0|1` – opt into treating `<link rel=modulepreload as=style>` as stylesheet candidates (defaults to off).
- `FASTR_FETCH_ALTERNATE_STYLESHEETS=0|1` – allow skipping `<link rel="alternate stylesheet">` entries when disabled (defaults to on).
- `FASTR_FETCH_ENFORCE_CORS=0|false|no|off` – opt out of browser-like CORS checks (`Access-Control-Allow-Origin`) for cross-origin web fonts and `<img crossorigin>` images (enabled by default).
- `FASTR_PAINT_BACKEND=display_list|legacy` – select the paint pipeline (defaults to `display_list`). Use `legacy` to force the immediate painter.
- `FASTR_DISABLE_RENDERER_SANDBOX=0|1` – debug escape hatch: disable the renderer OS sandbox (INSECURE).
  - Any non-empty value **other than** `0`/`false`/`no`/`off` disables sandboxing (e.g. `1`).
  - Windows alias: `FASTR_WINDOWS_RENDERER_SANDBOX=off` (`off`/`0`/`false`/`no` disable).
  - macOS alias: `FASTR_MACOS_RENDERER_SANDBOX=off`.
  - When set, FastRender logs a warning to stderr so insecure runs are not silent.
  - Windows note: disabling token/AppContainer sandboxing does **not** remove all guardrails: the
    spawn helper still uses the handle-inheritance allowlist and still attempts to apply a Job
    Object (kill-on-close, active-process cap). If job assignment fails (e.g. nested jobs are
    disallowed by the parent job), it may run jobless and prints a warning.
- `FASTR_ALLOW_UNSANDBOXED_RENDERER=0|1` – **Windows-only**: opt in to running the renderer without the full Windows sandbox when required primitives are missing or sandbox startup fails.
  - Default: disabled (sandbox failures return an error; no silent downgrade).
  - `crates/win-sandbox`: used by `RendererSandboxMode::new_default()` to avoid silently disabling
    sandboxing on unsupported hosts.
  - `crates/win-sandbox`: also used by `win_sandbox::renderer::RendererSandbox::new_default()`; when
    enabled it may allow the helper to run with AppContainer disabled and/or continue jobless if job
    assignment fails.
  - `fastrender::sandbox::windows::spawn_sandboxed(...)`: when set to `1`, Windows sandbox spawning
    may fall back to weaker sandboxing (restricted token) or an unsandboxed spawn.
- `FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts|off` – **macOS-only**: override the Seatbelt renderer profile used by `src/sandbox/macos.rs`.
  - `pure-computation` is the strict default (aliases: `pure`, `strict`).
  - `system-fonts` enables the relaxed system-font allowlist profile (aliases: `fonts`, `relaxed`).
- `FASTR_MACOS_USE_SANDBOX_EXEC=0|1` – **macOS-only**: opt into wrapping supported subprocess spawns
  in `/usr/bin/sandbox-exec` (debug/legacy; deprecated by Apple). Used by
  `fastrender::sandbox::macos_spawn::*` helpers and by `fastrender::sandbox::spawn::configure_renderer_command(...)`.
  - Ignored when sandboxing is disabled via `FASTR_DISABLE_RENDERER_SANDBOX=1`, `FASTR_RENDERER_SANDBOX=off`, or `FASTR_MACOS_RENDERER_SANDBOX=off`.
- `FASTR_DISABLE_WIN_MITIGATIONS=1` – **Windows-only**: disable Win32 *process mitigation policies* applied at process creation (Win32k lockdown, dynamic code prohibition, etc).
  - Any value disables (the check is presence-based, not `0/1` parsing).
  - This disables the optional `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` layer when spawning sandboxed processes.
  - This does **not** disable AppContainer / restricted-token sandboxing, job-object limits, or handle allowlisting.
  - Intended for debugging and compatibility with older/unusual Windows configurations.
- `FASTR_LOG_SANDBOX=0|1` – **Windows-only**: enable verbose Windows sandbox spawn logging (useful when debugging AppContainer/restricted-token failures).
  - In debug builds, sandbox spawn debug logs are enabled by default; set this in release builds.
- `FASTR_RENDERER_JOB_MEM_LIMIT_MB=<MiB>` – **Windows-only**: apply a best-effort renderer Job object committed-memory cap.
  - Parsed by `crates/win-sandbox` (`win_sandbox::renderer::RendererSandbox::new_default()` / `RendererSandboxBuilder`) and the `win-sandbox` probe tool.
  - `0`, empty, or unset disables. Accepts `_` separators (e.g. `1_024`).
  - Sets `JOB_OBJECT_LIMIT_JOB_MEMORY` (`JOBOBJECT_EXTENDED_LIMIT_INFORMATION::JobMemoryLimit`) in bytes.
  - Semantics: caps total *committed memory* across all processes in the job (not RSS; not per-process).
  - Note: the production Windows sandbox launcher in `src/sandbox/windows.rs` does **not** currently read this env var.
- `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1` – **Windows-only**: opt into inheriting the full parent environment when spawning sandboxed renderer children.
  - By default `src/sandbox/windows.rs` builds a sanitized environment block to avoid leaking secrets
    and overrides `TEMP`/`TMP` to a sandbox-accessible temp directory.
    - In AppContainer mode this is typically `GetAppContainerFolderPath(AppContainerSid)\Temp`
      (fallback: `C:\Windows\Temp`).
  - This is intended for local debugging only.
- `FASTR_PERF_SMOKE_PAGESET_GUARDRAILS_MANIFEST=/path/to/pageset_guardrails.json` – override the guardrails manifest consumed by the `perf_smoke` binary for the `--suite pageset-guardrails` suite. `FASTR_PERF_SMOKE_PAGESET_TIMEOUT_MANIFEST` is accepted as a legacy alias.

## Renderer sandboxing (macOS)

These env vars are consumed by the multiprocess renderer sandbox entrypoints (i.e. processes that
call [`fastrender::sandbox::apply_macos_sandbox_from_env`](../src/sandbox/mod.rs) during startup).

- `FASTR_RENDERER_SANDBOX=strict|relaxed|off` – control the macOS Seatbelt sandbox mode for renderer processes.
  - `strict` (default on macOS renderer processes): apply the built-in `pure-computation` profile (no filesystem access; no network access). Intended for production.
  - `relaxed`: still blocks network access, but allows read-only access to system font/framework locations needed for system font discovery. Useful for local development / debugging while still preventing accidental network access.
  - `off`: do not apply a sandbox (local debugging only; **not** safe for production).
  - Backwards-compatible spellings: `0` = `off`, `1` = `strict`.
- `FASTR_RENDERER_MACOS_SEATBELT_PROFILE=pure-computation|no-internet|renderer-default|<path>` – **macOS-only**: advanced override for the underlying Seatbelt profile used when `FASTR_RENDERER_SANDBOX` enables sandboxing.
  - When set, this overrides the `strict`/`relaxed` profile mapping.
  - `<path>` is treated as a path to an SBPL profile file to load.

Note: the debug escape hatches `FASTR_DISABLE_RENDERER_SANDBOX=1` and `FASTR_MACOS_RENDERER_SANDBOX=off`
also disable in-process Seatbelt sandboxing, overriding `FASTR_RENDERER_SANDBOX`.

When `FASTR_RENDERER_SANDBOX` is unset, `FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts`
can override the strict/relaxed mode selection while still keeping sandboxing enabled. This is a
legacy macOS-only alias and is ignored when `FASTR_RENDERER_SANDBOX` is explicitly set.

Recommended: leave unset in production builds so macOS renderers default to `strict`. Use `relaxed`
when you need system fonts, and use `off` only when debugging sandbox behaviour.

## Renderer sandbox layers (Linux)

These environment variables control individual **Linux** renderer sandbox layers. They are
consumed by the `sandbox_probe` utility and by Linux renderer sandbox spawn helpers (e.g.
`fastrender::sandbox::spawn::configure_renderer_command`).

They are primarily intended for developer ergonomics and debugging. When sandboxing is disabled via
`FASTR_DISABLE_RENDERER_SANDBOX=1` (or other platform-specific disable knobs), these layer toggles
are ignored.

- `FASTR_RENDERER_SECCOMP=0|1` – enable/disable the Linux seccomp-bpf syscall filter layer.
- `FASTR_RENDERER_LANDLOCK=0|1` – enable/disable the Linux Landlock filesystem sandbox layer.
- `FASTR_RENDERER_CLOSE_FDS=0|1` – enable/disable closing non-stdio file descriptors at renderer startup.
  - Currently applied by `sandbox_probe`; renderer entrypoints may adopt this over time as IPC wiring
    stabilizes.

## Browser UI (`browser` binary)

These are consumed by the experimental desktop browser UI (`browser` binary; see [browser_ui.md](browser_ui.md); run with `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --features browser_ui --bin browser`).

- `FASTR_BROWSER_MEM_LIMIT_MB=<MiB>` – best-effort address-space (virtual memory) limit for the `browser` process.
  - CLI equivalent: `browser --mem-limit-mb <MiB>`.
  - Set to `0`, empty, or unset to disable.
  - Accepts `_` separators (e.g. `1_024`).
  - On Linux this attempts to apply an `RLIMIT_AS` cap at process start; on other platforms it is currently unsupported.
- `FASTR_BROWSER_MAX_PIXELS=<pixels>` – safety limit for the maximum total device pixels in a single rendered frame (`pixmap_width_px × pixmap_height_px`).
  - Default: `50_000_000` (~200 MiB for RGBA8).
  - Accepts `_` separators (e.g. `50_000_000`).
- `FASTR_BROWSER_MAX_DIM_PX=<px>` – safety limit for the maximum **width or height** in device pixels for a single rendered frame.
  - Default: `8192`.
  - Accepts `_` separators.
- `FASTR_BROWSER_MAX_DPR=<ratio>` – safety cap for the effective device pixel ratio used for rendering.
  - Default: `10.0` (matches the renderer’s built-in clamp).
  - Values are clamped to the renderer’s supported DPR range.
  - Accepts `_` separators.
- `FASTR_BROWSER_PAGE_FILTER=nearest|linear|auto` – control the sampler filter used when drawing rendered page pixmaps in the windowed UI.
  - Default: `auto` (nearest at ~1:1 physical pixel mapping; linear when scaled).
  - Note: this affects page textures only; favicons continue to use linear filtering.
- `FASTR_BROWSER_RESIZE_DPR_SCALE=<ratio>` – during interactive window resize drags, multiply the computed page DPR by this scale to reduce pixmap allocation + GPU upload work (helps keep the UI responsive).
  - Default: `0.5`.
  - Values are clamped to `0.25..=1.0` (set to `1.0` to disable DPR downscaling).
- `FASTR_BROWSER_WGPU_FALLBACK=1` – force `wgpu` to use a fallback (software) adapter when creating the windowed UI.
  - CLI equivalent: `browser --force-fallback-adapter` (alias: `browser --wgpu-fallback`).
  - This can help in environments without a discrete GPU, under remote desktop, or when GPU driver setup is incomplete.
- `FASTR_BROWSER_WGPU_BACKENDS=<backend[,backend...]>` – select `wgpu` backend(s) used by the `browser` UI.
  - CLI equivalent: `browser --wgpu-backends <backend[,backend...]>` (alias: `browser --wgpu-backend <backend>`).
  - Accepted values: `vulkan`, `metal`, `dx12`, `dx11`, `gl` (alias: `opengl`), `browser-webgpu`
    (alias: `webgpu`), `all` (aliases: `auto`, `default`).
- `FASTR_BROWSER_DOWNLOAD_DIR=/path/to/dir` – override the download directory used by the windowed browser UI.
  - CLI equivalent: `browser --download-dir /path/to/dir`.
- `FASTR_BROWSER_BOOKMARKS_PATH=/path/to/bookmarks.json` – override the bookmarks persistence file path (JSON).
- `FASTR_BROWSER_HISTORY_PATH=/path/to/history.json` – override the history persistence file path (JSON).
- `FASTR_BROWSER_ALLOW_CRASH_URLS=0|1` – allow navigating to `crash://` URLs (testing hook).
  - CLI equivalent: `browser --allow-crash-urls`.
  - Note: this only allowlists the scheme for typed/explicit navigations; the UI worker still only
    *crashes* on `crash://panic` when the internal crash hook is explicitly enabled (see
    `FASTR_ENABLE_CRASH_URLS` below).
- `FASTR_TEST_BROWSER_EXIT_IMMEDIATELY=1` – **test-only** hook: make the `browser` binary exit successfully immediately after parsing/applying its startup env vars (so tests can exercise `FASTR_BROWSER_MEM_LIMIT_MB` handling without opening a window).
  - CLI equivalent: `browser --exit-immediately`.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE=1` – **test-only** hook: run a minimal end-to-end headless smoke test of the real `browser` entrypoint and UI↔worker messaging (for CI environments without a display/GPU). On success it prints `HEADLESS_SMOKE_OK` to stdout and exits without opening a window or initialising winit/wgpu.
  - CLI equivalent: `browser --headless-smoke`.
  - JS variant: combine with `browser --headless-smoke --js` (or set the env var and pass `--js`) to
    run a vm-js `api::BrowserTab` smoke test instead; on success it prints `HEADLESS_VMJS_SMOKE_OK`.
- `FASTR_TEST_BROWSER_HEADLESS_CRASH_SMOKE=1` – **test-only** hook: run a headless crash-isolation smoke test that intentionally crashes the renderer worker and validates that the crash is contained (future: renderer *process* crash isolation). On success it prints `HEADLESS_CRASH_SMOKE_OK` to stdout.
  - CLI equivalent: `browser --headless-crash-smoke`.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON=<json>` – **test-only** hook: override the restored session used by headless smoke mode with an explicit `BrowserSession` JSON value.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE_BOOKMARKS_JSON=<json>` – **test-only** hook: override the bookmarks store used by headless smoke mode with an explicit JSON value.
  - This is expected to be the same schema as the bookmarks file on disk (`BookmarkStore`), but legacy bookmark list schemas are still accepted and migrated.
- `FASTR_TEST_BROWSER_HEADLESS_SMOKE_HISTORY_JSON=<json>` – **test-only** hook: override the global history store used by headless smoke mode with an explicit JSON value.
  - This is expected to be the same schema as the history file on disk (`PersistedGlobalHistoryStore`), but legacy list schemas are still accepted and migrated.

### Browser UI crash URL hooks (test-only)

These are internal crash-isolation test hooks used by browser/worker integration tests. They are
**disabled by default** and should not be enabled in normal browsing sessions.

- `FASTR_ENABLE_CRASH_URLS=0|1` – enable internal `crash://` crash triggers in the UI worker.
  - When enabled, navigating to `crash://panic` (typed navigation) deliberately panics the UI
    worker thread after emitting `WorkerToUi::NavigationStarted` so the browser can attribute the
    crash to the correct tab.

### Performance / responsiveness logging (browser UI)

These env vars enable machine-readable performance logging for the windowed browser UI (frame times,
scroll/resize smoothness, and input latency). See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness)
and [`instructions/browser_responsiveness.md`](../instructions/browser_responsiveness.md) for how to
interpret the metrics.

For an interactive capture helper (sets `FASTR_PERF_LOG=1`, runs under `run_limited`, and writes the
JSONL stream to a file via `FASTR_PERF_LOG_OUT`), see
[`scripts/capture_browser_perf_log.sh`](../scripts/capture_browser_perf_log.sh).

- `FASTR_PERF_LOG=0|1` – enable JSONL (“JSON Lines”) perf logging in the windowed `browser` UI.
  - When enabled, the `browser` binary emits JSONL (one JSON object per line) events describing
    frame-time samples, input latency, resize latency, and navigation TTFP measurements.
    Event types: `frame`, `input`, `resize`, `navigation`, `ttfp`, `stage`, `cpu_summary`.
  - Worker stage heartbeat events (`event=stage`) are emitted when the windowed UI processes
    `WorkerToUi::Stage` messages. These include:
    - `tab_id`, `stage` (e.g. `layout`, `paint_build`), and `hotspot` (coarse bucket such as
      `fetch`, `css`, `layout`, `paint`).
  - Output:
    - Defaults to **stdout** (so it can be piped/collected).
    - If `FASTR_PERF_LOG_OUT` is set, the log is written to that path instead (created/truncated).
  - CPU usage summary (`event=cpu_summary`, emitted ~once per second):
    - `cpu_time_ms_total`: total process CPU time (user + system) since startup (milliseconds).
    - `cpu_percent_recent`: CPU usage over the most recent interval (`Δcpu / Δwall * 100`).
    - Example:
    ```bash
    FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
      timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
      bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- about:test-layout-stress
    ```
- `FASTR_PERF_LOG_OUT=/path/to/log.jsonl` – output path for `FASTR_PERF_LOG` JSONL events.
  - When unset/empty, events go to **stdout**.
- `FASTR_BROWSER_TRACE_OUT=/path/to/trace.json` – write a Chrome trace of the windowed `browser`
  UI event loop (winit input handling, worker message draining, egui frame build, GPU submission,
  and present). The trace is written when the browser process exits.
  - Legacy alias: `FASTR_PERF_TRACE_OUT=/path/to/trace.json`.

### Appearance / accessibility / debugging (browser UX)

These are intended for the windowed `browser` UI and affect the browser chrome (and, where noted, the default user-preference media query surface seen by rendered pages).

Many of these settings can also be changed in-app via the browser’s Appearance menu (gear icon in
the toolbar). Those in-app settings are persisted in the browser session file so they survive
restarts.

Not all builds implement all of these toggles yet; unsupported values are expected to be ignored. When in doubt, confirm the current behaviour in `src/bin/browser.rs` / `src/ui/`.

- `FASTR_BROWSER_THEME=system|light|dark` – select the browser chrome theme.
  - Default: `system`.
  - `system` follows the OS light/dark preference when available.
  - `light` / `dark` force a specific theme regardless of the OS preference.
  - Interaction with content rendering:
    - The browser UI appearance settings provide **defaults** for the page media-query surface
      (`prefers-*`):
      - Theme → `prefers-color-scheme` (`light`/`dark`).
      - `FASTR_BROWSER_HIGH_CONTRAST=1` → `prefers-contrast: more`.
      - `FASTR_BROWSER_REDUCED_MOTION=1` → `prefers-reduced-motion: reduce`.
    - Explicit renderer overrides (`FASTR_PREFERS_COLOR_SCHEME`, `FASTR_PREFERS_CONTRAST`,
      `FASTR_PREFERS_REDUCED_MOTION`) take precedence over any browser UI preference.
- `FASTR_BROWSER_ACCENT=<hex>` – override the browser chrome accent color (links, focus rings,
  selection highlight, etc).
  - When set to a valid value, this overrides the persisted in-app accent selection.
  - Accepted formats: `RGB`, `RRGGBB`, `RRGGBBAA` (leading `#` optional).
  - Invalid values are ignored.
- `FASTR_BROWSER_UI_SCALE=<float>` – UI scale multiplier for browser chrome widgets.
  - Default: `1.0` (no additional scaling beyond the OS/window scale factor).
  - Must be a finite, positive float (e.g. `1.25`).
  - This is intended to affect **UI density/readability** and is separate from per-tab page zoom.
- `FASTR_BROWSER_HIGH_CONTRAST=0|1` – enable a high-contrast UI theme / stronger focus indicators.
  - Default: `0`.
  - Interaction with content rendering:
    - Pages see `prefers-contrast: more` by default unless explicitly overridden via
      `FASTR_PREFERS_CONTRAST=...`.
- `FASTR_BROWSER_REDUCED_MOTION=0|1` – reduce/disable non-essential UI animations.
  - Default: `0`.
  - Interaction with content rendering:
    - Pages see `prefers-reduced-motion: reduce` by default unless explicitly overridden via
      `FASTR_PREFERS_REDUCED_MOTION=...`.
- `FASTR_BROWSER_HUD=0|1` – show an in-app HUD overlay with browser/debug metrics.
  - Includes FPS / frame-time samples, frame queue/backpressure stats, and (when enabled) UI latency
    + CPU summary metrics.
  - Default: `0`.
- `FASTR_BROWSER_DEBUG_LOG=0|1` – enable browser/worker debug logging UI.
  - Default: enabled in debug builds; disabled in release builds unless set to `1`.
- `FASTR_BROWSER_LOG_SURFACE_CONFIGURE=0|1` – log `wgpu::Surface::configure` calls to stderr.
  - Default: `0`.
  - Useful when debugging interactive resize performance (should configure at most once per rendered frame).
- `FASTR_BROWSER_SHOW_MENU_BAR=0|1` – override whether the in-window menu bar is shown.
  - When set, this takes precedence over the persisted session setting (useful for CI).
- `FASTR_BROWSER_RENDERER_CHROME=0|1` – experimental: render the browser chrome UI using FastRender
  (“renderer-chrome”) instead of egui.
  - This is a work-in-progress under [`instructions/renderer_chrome.md`](../instructions/renderer_chrome.md).
  - When enabled, the browser uses a custom `accesskit_winit::Adapter` path instead of egui’s
    built-in AccessKit integration. See [chrome_accessibility.md](chrome_accessibility.md).

### Browser session file (tabs / zoom persistence)

- `FASTR_BROWSER_SESSION_PATH=/path/to/fastrender_session.json` – override where the browser session file is stored.
  - Primary use case: tests/integration harnesses that need an isolated session file.
  - The default is a per-user config path (via `directories`) with a fallback to `./fastrender_session.json` in the current working directory.

### Browser bookmarks file

- `FASTR_BROWSER_BOOKMARKS_PATH=/path/to/fastrender_bookmarks.json` – override where the browser bookmarks file is stored.
  - Primary use case: tests/integration harnesses that need an isolated bookmarks file.
  - The default is a per-user config path (via `directories`) with a fallback to `./fastrender_bookmarks.json` in the current working directory.

### Browser global history file

- `FASTR_BROWSER_HISTORY_PATH=/path/to/fastrender_history.json` – override where the browser global history file is stored.
  - Primary use case: tests/integration harnesses that need an isolated history file.
  - The default is a per-user config path (via `directories`) with a fallback to `./fastrender_history.json` in the current working directory.

## Compatibility toggles

- `FASTR_COMPAT_REPLACED_MAX_WIDTH_100=0|1` – control whether FastRender applies a **non-standard** default `max-width: 100%` to replaced elements (`img`, `video`, `audio`, `canvas`, `iframe`, `embed`, `object`).
  - Default: `0` (disabled) to match browser UA defaults (no implicit `max-width`).
  - Set `1` to enable the compatibility behavior, which may prevent replaced elements from overflowing their containing block when author CSS does not constrain them.
  - Note: this toggle intentionally does **not** apply to inline `<svg>` elements.

- `FASTR_CLASSIC_SCROLLBARS=0|1` – enable legacy classic-scrollbar layout behavior for `overflow:auto` containers.
  - Default: `0` (disabled). FastRender models scrollbars as overlay by default; layout space is reserved only when the author opts in via `scrollbar-gutter: stable`.
  - When enabled, block layout may perform extra reflow passes to “force” scrollbars when content overflows. This is significantly more expensive on float-heavy pages and is intended only for compatibility experiments.

## HTTP fetch tuning

These env vars tune HTTP(S) requests made by FastRender’s [`HttpFetcher`](../src/resource.rs) when used by the CLI binaries (notably `fetch_pages`, `prefetch_assets`, `render_pages`, `fetch_and_render`, and `pageset_progress` workers).

Some knobs are implemented directly in `HttpFetcher` (so they also apply to library users who rely
on `HttpFetcher`), while the retry/backoff knobs are parsed by the shared CLI helper
[`cli_utils::render_pipeline::build_http_fetcher`](../src/cli_utils/render_pipeline.rs) (imported as
`fastrender::cli_utils as common` in the bins).

Unless noted otherwise, they are parsed once at process startup; invalid values are ignored.

- `FASTR_HTTP_BACKEND=auto|ureq|reqwest|curl` – choose the HTTP backend.
  - `auto` (default): use the Rust backends (`reqwest` for `https://`, `ureq` otherwise) and fall back to the system `curl` binary for retryable network/TLS/HTTP2 errors.
    - If the fallback also fails, the original error message is annotated with `curl fallback failed: ...` to make it obvious both backends were attempted.
    - Fallback is not triggered for all failure modes (for example, `empty HTTP response body` / 0 bytes); set `FASTR_HTTP_BACKEND=curl` explicitly when comparing behavior on hard sites.
    - If `curl` is not available on `$PATH`, `auto` behaves like `reqwest`/`ureq` selection (no fallback).
    - When a timeout budget is active (CLI `--timeout` or a render deadline with a timeout), `auto` caps the initial Rust backend work so there is still time left to attempt the `curl` fallback (for example, it caps per-attempt `ureq` timeouts and disables `reqwest` retries).
  - `ureq`: force the Rust backend (disables the `curl` fallback; useful to confirm a failure is backend-specific).
  - `reqwest`: force the HTTP/2-capable Rust backend (disables the `curl` fallback).
  - `curl`: force the `curl` backend for all requests (HTTP/2-capable when your system `curl` has HTTP/2 support; useful for hard sites and differential diagnosis; requires `curl` on `$PATH`).
  - Accepted aliases: `fallback` (auto) and `rust`/`native` (ureq). Unknown values behave like `auto`.
- `FASTR_HTTP_BROWSER_HEADERS=0|1` – enable/disable browser-like request headers (per-resource `Accept` + `Sec-Fetch-*` + `Upgrade-Insecure-Requests`; fonts and CORS-mode image requests also get `Origin` + `Referer`). Defaults to `1`; set to `0` to preserve the legacy minimal header set for debugging.
  - When built with `disk_cache`, `FASTR_HTTP_BROWSER_HEADERS=0` also partitions the disk cache namespace so you don’t accidentally reuse subresources fetched under the browser-header profile.
- `FASTR_HTTP_LOG_RETRIES=0|1` – log retry attempts + backoff sleeps to stderr (default off; printed by the fetcher itself, so it also applies to library users).
  - `pageset_progress`: captured in `target/pageset/logs/<stem>.stderr.log` (worker stdout/stderr).
  - `render_pages`: captured in `fetches/renders/<stem>.stderr.log` when running in the default worker mode (with `--in-process`, logs go to the terminal).
  - Other CLIs (`fetch_pages`, `prefetch_assets`, `fetch_and_render`, `bundle_page`): printed directly to the terminal.
- `FASTR_HTTP_WWW_FALLBACK=0|1` – enable/disable a single `www.` hostname retry for document-like HTTP(S) requests that fail with a timeout / no-response network error. Defaults to `1`. The rewritten URL is still subject to `ResourcePolicy` allow/deny checks, and the fallback is skipped for IP-literal hosts or hosts that already start with `www.`.

Retry/backoff knobs map to [`fastrender::resource::HttpRetryPolicy`](../src/resource.rs) and are applied by the CLI helper `build_http_fetcher` (defaults below refer to that CLI path):

- `FASTR_HTTP_MAX_ATTEMPTS=<N>` – total attempts per HTTP request (initial request + retries). Set to `1` to disable retries (default `3`).
- `FASTR_HTTP_BACKOFF_BASE_MS=<ms>` – base delay for exponential backoff (default `50`).
- `FASTR_HTTP_BACKOFF_CAP_MS=<ms>` – maximum delay between retries (default `500`).
- `FASTR_HTTP_RESPECT_RETRY_AFTER=0|false|no` – disable honoring `Retry-After` headers for retryable responses (enabled by default).

When a CLI timeout is configured (e.g. `fetch_pages --timeout 60` or `prefetch_assets --timeout 30`),
it is treated as a **total** wall-clock budget for a single fetch call when no render deadline is
installed: retry attempts and backoff sleeps are bounded by the remaining budget, and per-attempt
HTTP timeouts are clamped so one request cannot take `max_attempts × timeout`.

Because the pageset CLIs share `build_http_fetcher`, these env vars apply consistently to HTML fetches (`fetch_pages`) and subresource fetches (`prefetch_assets`, `render_pages`, `pageset_progress`, `fetch_and_render`) without adding new flags.

Note: when rendering under a cooperative render timeout (e.g. `pageset_progress` soft timeouts), `HttpFetcher` disables retries to avoid extending past the deadline. Retry knobs primarily affect `fetch_pages`, `prefetch_assets`, and any fetches that happen outside a render deadline (such as client redirect following).

Hard-site starting point (Akamai/CDN bot gating, empty bodies, HTTP/2 errors):

```bash
FASTR_HTTP_BACKEND=reqwest FASTR_HTTP_BROWSER_HEADERS=1 FASTR_HTTP_LOG_RETRIES=1 \
  bash scripts/cargo_agent.sh xtask pageset --pages tesco.com,washingtonpost.com
```

When comparing backends/header profiles, make sure you are actually exercising the network path:
use `fetch_pages --refresh` for HTML and consider disabling the disk cache (`DISK_CACHE=0` / `NO_DISK_CACHE=1`) so previously cached subresources don’t mask differences.

## Resource limits

- `FASTR_MAX_FOREIGN_OBJECT_CSS_BYTES=<N>` – cap the amount of document-level CSS injected into nested
  `<foreignObject>` HTML renders (default 262_144 bytes).
- `FASTR_SVG_EMBED_DOCUMENT_CSS=0|1` – force-disable/force-enable embedding document `<style>` CSS into
  serialized inline `<svg>` replaced elements. When unset, embedding is automatic and is disabled when
  the document CSS exceeds 64KiB. When `FASTR_SVG_EMBED_DOCUMENT_CSS_MAX_SVGS` is set, embedding is also
  disabled when the document contains more than that many replaced inline SVGs. The 64KiB cap still
  applies when forced on.
- `FASTR_SVG_EMBED_DOCUMENT_CSS_MAX_SVGS=<N>` – maximum replaced inline `<svg>` elements allowed before
  document CSS embedding is disabled. Unset means unlimited. Only used when
  `FASTR_SVG_EMBED_DOCUMENT_CSS` is unset.
- `FASTR_INLINE_MAX_STYLESHEETS=<N>` – maximum number of stylesheets inlined across `<link>`/embedded
  discovery and `@import` chains (default 128).
- `FASTR_EMBEDDED_CSS_MAX_CANDIDATES=<N>` – cap the number of stylesheet URLs discovered via the
  embedded `.css` string heuristic (default 16; only used when the HTML has no `<link rel=stylesheet>`
  candidates and no inline `<style>` tags).
- `FASTR_INLINE_MAX_INLINE_CSS_BYTES=<N>` – total CSS bytes allowed when inlining stylesheets
  (default 2 MiB).
- `FASTR_INLINE_MAX_INLINE_IMPORT_DEPTH=<N>` – maximum @import nesting depth during inlining
  (default 8).

## Debug dumps (paint / display list)

These are emitted by the paint pipeline:

- `FASTR_DETERMINISTIC_PAINT=1` – enable deterministic clearing of certain paint/filter thread-local
  scratch buffers. This can help diagnose nondeterministic pixel diffs that are caused by stale
  bytes leaking between renders (at the cost of extra memory clears).
- `FASTR_DUMP_STACK=1`
- `FASTR_DUMP_FRAGMENTS=1`
- `FASTR_DUMP_COUNTS=1`
- `FASTR_DUMP_TEXT_ITEMS=1`
- `FASTR_DUMP_COMMANDS=<N>` (omit `N` to dump all)
- `FASTR_TRACE_IMAGE_PAINT=<N>` – log up to N image paints (empty value defaults to 50).
- `FASTR_LOG_IMAGE_FAIL=1` – log failed image loads/placeholders.
- `FASTR_PRESERVE3D_DEBUG=1` – log preserve-3d scene depth sorting + warp fallback decisions during painting.
- `FASTR_PRESERVE3D_DISABLE_SCENE=1` – disable the preserve-3d scene compositor and fall back to the display-list precomposition path (less correct but can enable parallel tiling on preserve-3d pages).
- `FASTR_PRESERVE3D_DISABLE_WARP=1` – disable projective warping and force 2D affine approximation for 3D/perspective transforms (including preserve-3d scenes).
- `FASTR_PRESERVE3D_WARP=1` – opt into the projective warp path when building without the default `preserve3d_warp` feature.

## Debug dumps (layout / fragments)

- `FASTR_LOG_FRAG_BOUNDS=1` – log fragment-tree bounds vs the viewport.
- `FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX=<px>` – inflate the layout-time viewport used for `content-visibility:auto` skipping by `<px>` on all sides (default `0`). Helpful when debugging near-viewport heuristics without changing code.
- `FASTR_DUMP_TEXT_FRAGMENTS=<N>` – sample text fragments (default 20 when enabled).
- `FASTR_TEXT_DIAGNOSTICS=verbose` – log sampled clusters that relied on last-resort font fallback.
- `FASTR_TEXT_FALLBACK_DESCRIPTOR_STATS=1` – collect per-render font fallback descriptor keyspace statistics (unique descriptor/family/language/weight counts + a small sample of descriptor summaries). When render diagnostics are enabled, this populates `RenderStats.counts.fallback_descriptor_stats` (and therefore appears in pageset progress JSON / `RenderDiagnostics.stats`).
- `FASTR_TRACE_TEXT=<substring>` – walk the fragment tree and print a containment trail for the first text fragment containing the substring.
- `FASTR_TRACE_FRAGMENTATION=1` – trace fragmentation break opportunities/boundary choices (also available via `inspect_frag --trace-fragmentation`).
- `FASTR_FIND_TEXT` / `FASTR_FIND_BOX_TEXT` / `FASTR_INSPECT_MASK` – search for matching text fragments/boxes.
- `FASTR_NEEDLE=<string>` – generic string matcher used by various debug paths.

## Performance / profiling

- `FASTR_CASCADE_PROFILE=1` – cascade profiling (populates `RenderDiagnostics.stats.cascade` with
  selector candidate/match counters and `:has()` evaluation counters).
- `FASTR_LAYOUT_PROFILE=1` – layout-context profiling.
- `FASTR_GRID_MEASURE_CACHE_PROFILE=1` – grid item measurement cache profiling (TLS/shared hits/misses plus override-key breakdown). When enabled (or when `FASTR_LAYOUT_PROFILE=1` is enabled), additional counters are exposed via `RenderDiagnostics.stats.layout.grid_measure_cache_*` and `layout profile` logs.
- `FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES=0|1` – opt into storing style override (`override_fingerprint`) grid measure keys in the shared cross-thread cache. Defaults to off; use with `FASTR_GRID_MEASURE_CACHE_PROFILE=1` to confirm override-heavy workloads benefit without polluting the cache. Override entries are still memory-bounded via an explicit per-shard cap and are evicted preferentially under shared-cache pressure.
- `FASTR_FLEX_PROFILE=1` – flex profiling (with additional `FASTR_FLEX_PROFILE_*` knobs).
- `FASTR_SVG_PROFILE=1` – print per-render inline SVG serialization stats (calls/bytes/time and whether
  document CSS embedding was enabled).
- `FASTR_PROFILE_FRAGMENT_CLONES=1` – count fragment clones when layout/flex/grid caches reuse measured/layout fragments and enable fragment instrumentation (deep clone counts, traversal).
- `FASTR_INTRINSIC_STATS=1` – intrinsic sizing cache stats.
- `FASTR_LAYOUT_CACHE_STATS=1` – layout cache stats (intrinsic cache hits/misses and pass counts).
- `FASTR_TABLE_STATS=1` – table auto-layout counters (cell intrinsic measurements + per-cell layout calls).
- `FASTR_LAYOUT_CACHE_MAX_ENTRIES=<N>` – per-thread layout cache entry cap (default auto-scaled from box tree size, min 8192 / max 32768; set to 0 to disable).
- `FASTR_SCOPE_SELF_MATCH_SELECTOR_CACHE_MAX_ENTRIES=<N>` – per-thread `@scope` helper cache entry cap for selector "self-match" variants (default 4096; set to 0 to disable caching).
- `FASTR_TEXT_SHAPING_CACHE_CAPACITY=<N>` – shaping cache entry cap (default 2048; 0/empty/unset uses the default).
- `FASTR_TAFFY_CACHE_LIMIT=<N>` – Taffy flex/grid template cache capacity (default 512; auto-scaled for large box trees). Use `FASTR_TAFFY_FLEX_CACHE_LIMIT` / `FASTR_TAFFY_GRID_CACHE_LIMIT` to override adapters independently.
- `FASTR_TRACE_OUT=/path/to/trace.json` – emit Chrome trace events for parse/style/layout/paint.
- `FASTR_TRACE_MAX_EVENTS=<N>` – cap the number of Chrome trace events retained per render (default 200000). Excess events are dropped deterministically and counted in the output trace JSON.
- `FASTR_PAINT_BUILD_BREAKDOWN=0|1` – collect low-overhead display-list build sub-timers (stacking tree, clip-path resolution, underline decoration, image decode, etc). Requires render diagnostics and populates `RenderStats.paint.build_*_{ms,calls}` fields.
- `FASTR_DISABLE_LAYOUT_CACHE=1` / `FASTR_DISABLE_FLEX_CACHE=1` – disable layout/flex caches.
- `FASTR_IMAGE_PROFILE_MS=<ms>` / `FASTR_STACK_PROFILE_MS=<ms>` / `FASTR_TEXT_PROFILE_MS=<ms>` / `FASTR_CMD_PROFILE_MS=<ms>` – emit timing when operations exceed the threshold.
- `FASTR_IMAGE_PROBE_MAX_BYTES=<bytes>` – cap bytes fetched when probing image dimensions/metadata (default 65536; the probe retries with a larger prefix before falling back to a full fetch).
- `FASTR_IMAGE_META_CACHE_ITEMS=<N>` – max entries in the in-memory image metadata probe cache (default 2000).
- `FASTR_IMAGE_META_CACHE_BYTES=<bytes>` – approximate byte cap for cached image probe metadata (default 16 MiB).
- `FASTR_IMAGE_RAW_CACHE_ITEMS=<N>` – max entries in the in-memory raw image cache used to share bytes between `probe()` and `load()` (default 64).
- `FASTR_IMAGE_RAW_CACHE_BYTES=<bytes>` – approximate byte cap for raw image bytes cached between `probe()` and `load()` (default 64 MiB).
- `FASTR_TEXT_FALLBACK_CACHE_CAPACITY=<N>` – override the font fallback cache capacity in entries (default 131072). This applies separately to the glyph and cluster fallback caches; 0/empty/unset disables the override (use the default). Applied values are clamped to >= 1.

How to interpret fallback-cache diagnostics (when render diagnostics are enabled and `RenderStats` is available):

- `fallback_cache_*_evictions` and `fallback_cache_clears` are the primary pressure signals.
- `fallback_cache_*_{entries,capacity}` and `fallback_cache_shards` provide context on cache sizing/sharding.

High eviction counts typically imply cache pressure (raise `FASTR_TEXT_FALLBACK_CACHE_CAPACITY`). High `RenderStats.counts.fallback_descriptor_stats.unique_descriptors` (enable `FASTR_TEXT_FALLBACK_DESCRIPTOR_STATS=1`) suggests a descriptor keyspace explosion, which can keep hit rates low even with a large cache.
- `FASTR_SELECTOR_BLOOM=0` – disable selector bloom-filter hashing (useful for perf A/B checks).
- `FASTR_SELECTOR_BLOOM_BITS=256|512|1024` – selector bloom summary bit size used for `:has()` pruning (default 1024; larger reduces false positives on large subtrees but costs more memory/build time).
- `FASTR_ANCESTOR_BLOOM=0` – disable the cascade's ancestor bloom filter fast-reject for descendant selectors.
- `FASTR_SVG_FILTER_CACHE_ITEMS=<N>` – SVG filter cache capacity (default 256).
- `FASTR_SVG_FILTER_CACHE_BYTES=<N>` – approximate SVG filter cache size limit in bytes (default 4 MiB).
- `FASTR_SVG_FILTER_RESOLVER_CACHE_ITEMS=<N>` – per-render SVG filter resolver memoization capacity (default 256). `0` disables caching.
- `FASTR_IFRAME_RENDER_CACHE_ITEMS=<N>` – iframe render cache capacity in entries (default 128). `0` disables caching.
- `FASTR_IFRAME_RENDER_CACHE_BYTES=<bytes>` – iframe render cache size limit in bytes (default 128 MiB). `0` disables caching.
- `FASTR_DECODED_IMAGE_CACHE_ITEMS=<N>` – decoded image cache capacity (display-list builder; default 256). `0` disables caching.
- `FASTR_DECODED_IMAGE_CACHE_BYTES=<bytes>` – decoded image cache size limit in bytes (default 128 MiB). `0` disables caching.
- `FASTR_GRADIENT_PIXMAP_CACHE_ITEMS=<N>` – rasterized gradient pixmap cache capacity (default 64). `0` disables caching.
- `FASTR_GRADIENT_PIXMAP_CACHE_BYTES=<bytes>` – rasterized gradient pixmap cache size limit in bytes (default 64 MiB). `0` disables caching.
- `FASTR_GLYPH_CACHE_ITEMS=<N>` – shared glyph-outline cache capacity (default 2048). `0` disables caching.
- `FASTR_GLYPH_CACHE_BYTES=<bytes>` – shared glyph-outline cache size limit in bytes (default 32 MiB). `0` disables caching.
- `FASTR_COLOR_GLYPH_CACHE_ITEMS=<N>` – shared color glyph cache capacity (default 2048). `0` disables caching.
- `FASTR_COLOR_GLYPH_CACHE_BYTES=<bytes>` – shared color glyph cache size limit in bytes (default 16 MiB). `0` disables caching.
- `FASTR_PAINT_IMAGE_PIXMAP_CACHE_ITEMS=<N>` – cap full-resolution image→pixmap conversion cache entries during display-list paint (default 128; set to 0 to disable caching).
- `FASTR_PAINT_IMAGE_PIXMAP_CACHE_BYTES=<bytes>` – cap full-resolution image→pixmap conversion cache size (default 128 MiB; uses `Pixmap::data().len()` as a minimum weight; set to 0 to disable caching).
- `FASTR_PAINT_SHARED_IMAGE_PIXMAP_CACHE_ITEMS=<N>` – cap shared image→pixmap conversion cache entries during parallel tiled paint (default 256; set to 0 to disable cross-tile sharing).

## Parallelism tuning

- `RAYON_NUM_THREADS=<N>` – cap Rayon’s global thread pool. This is not a FastRender-specific env var,
  but the pageset runners and wrappers rely on it to avoid CPU oversubscription when many worker
  processes run in parallel. When unset, `scripts/pageset.sh` / `bash scripts/cargo_agent.sh xtask pageset` and the
  `pageset_progress`/`render_pages` worker orchestration default to a per-worker budget derived from
  `available_parallelism()/jobs` (min 1, additionally clamped by a detected cgroup CPU quota on Linux). Set it explicitly to override the default.
- `FASTR_DISPLAY_LIST_PARALLEL=0|1` – enable/disable Rayon fan-out while building display lists (default enabled).
- `FASTR_DISPLAY_LIST_PARALLEL_MIN=<N>` – fragment-count threshold before the builder fans out (default 32).
- `FASTR_PAINT_PARALLEL=off|on|auto` – control tiled parallel rasterization when painting display lists (default `auto`).
- `FASTR_PAINT_PARALLEL_MAX_THREADS=<N>` – cap Rayon worker threads used during tiled paint fan-out (defaults to unlimited; useful when running many worker processes).
- `FASTR_PAINT_THREADS=<N>` – opt into a dedicated Rayon thread pool for paint build/rasterize. When unset, paint uses the current/global pool. This is useful when pageset workers set `RAYON_NUM_THREADS=1` but paint should still use multiple cores.
- `FASTR_THREAD_POOL_CACHE_MAX=<N>` – cap the number of distinct dedicated Rayon pools cached by thread-count for layout/paint/intrinsic image probing (default `4`; set `0` to disable caching).
- `FASTR_LAYOUT_PARALLEL=off|on|auto` – override layout fan-out mode regardless of RenderOptions/FastRenderConfig. When unset, the effective default comes from the caller (the CLI tools default to `auto`; the library defaults to `off`).
- `FASTR_LAYOUT_PARALLEL_MIN_FANOUT=<N>` – sibling threshold before layout attempts to fan out (default 8).
- `FASTR_LAYOUT_PARALLEL_MAX_THREADS=<N>` – cap Rayon worker threads used during layout fan-out.
- `FASTR_LAYOUT_PARALLEL_MIN_NODES=<N>` – minimum box nodes before auto layout fan-out will engage (default 1024).
- `FASTR_LAYOUT_PARALLEL_DEBUG=1` – capture layout parallel debug counters (worker threads/work items) for diagnostics/logging.
- `FASTR_INTRINSIC_PROBE_PARALLELISM=<N>` – cap the thread count for intrinsic image probing during box-tree construction. Uses a dedicated Rayon pool; defaults to a multiple of the per-worker thread budget (`RAYON_NUM_THREADS` when set, otherwise the platform-reported available parallelism) and is further capped by the probe prefetch logic.

## Media query overrides

These override user-preference media queries (and are also settable via CLI flags on the render binaries):

- `FASTR_PREFERS_COLOR_SCHEME=light|dark|no-preference`
- `FASTR_PREFERS_CONTRAST=more|high|less|low|custom|forced|no-preference`
- `FASTR_PREFERS_REDUCED_MOTION=reduce|no-preference`
- `FASTR_PREFERS_REDUCED_DATA=reduce|no-preference`
- `FASTR_PREFERS_REDUCED_TRANSPARENCY=reduce|no-preference`
- `FASTR_MEDIA_TYPE=screen|print|all|speech` and `FASTR_SCRIPTING=none|initial-only|enabled`

## Layout logging (grab-bag)

- `FASTR_LOG_CONTAINER_FIELDS` / `FASTR_LOG_CONTAINER_PASS` / `FASTR_LOG_CONTAINER_REUSE` – container query tracing.
- `FASTR_LOG_CONTAINER_IDS=<ids>` – restrict container logs to IDs.
- `FASTR_LOG_FLEX_*` – flexbox logging (constraints, child placement, overflow, drift; see `RuntimeToggles` for full list).
- `FASTR_LOG_BLOCK_PROGRESS_MS=<ms>` – per-child progress logging in block layout (with optional `FASTR_LOG_BLOCK_PROGRESS_IDS`/`MATCH` filters).
- `FASTR_LOG_LINE_WIDTH` / `FASTR_LOG_INLINE_BASELINE` / `FASTR_LOG_OVERFLOW_TEST` – inline layout diagnostics.
- `FASTR_LOG_TABLE` / `FASTR_DUMP_CELL_CHILD_Y` – table layout tracing.
- `FASTR_LOG_ABS_CLAMP` – clamp logging for absolutely positioned elements.
- `FASTR_LOG_TRANSITIONS=1` – log applied @starting-style transitions with property names and progress.

## Source of truth

These flags are internal and evolve. To find the full current set, run:

`rg -o --no-filename "FASTR_[A-Z0-9_]+" -S src tests | sort -u`
