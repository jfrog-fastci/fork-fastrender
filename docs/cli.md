# CLI tools

FastRender ships a few small binaries/examples intended for internal debugging and regression work.
Prefer `--help` output for the source of truth. Shared flag schemas for viewport/DPR, media type
and preferences, output formats, timeouts, and base URL overrides live in
[`src/cli_utils/args.rs`](../src/cli_utils/args.rs).

Compatibility toggles are **opt-in** across the render CLIs. Pass `--compat-profile site` to enable
site-specific hacks and `--dom-compat compat` to apply generic DOM compatibility mutations (class
flips + common lazy-load `data-*` → `src`/`srcset`/`poster` lifting); both default to spec-only
behavior. `bash scripts/cargo_agent.sh xtask pageset` and the shell wrappers leave these off unless
you explicitly provide the flags so pageset triage can choose when to enable them.

HTML parsing has a separate "scripting enabled" flag (html5ever tree-builder semantics) that affects
how `<noscript>` is tokenized. The render CLIs parse with scripting-enabled semantics by default (to
match Chrome baselines captured with CSP `script-src` blocked), even though scripts are not executed
in the static pipeline. Use `--render-parse-scripting-enabled=false` to force scripting-disabled
parsing semantics when debugging `<noscript>` fallbacks.

## JavaScript execution (`--js`)

FastRender’s render CLIs run in a **static** mode (HTML/CSS/layout/paint only): author scripts are
**not** executed unless you opt in with `--js`.
 
The windowed `browser` app is an exception: author JS execution is currently enabled by default
(experimental; there is no stable CLI toggle to disable it yet).

### Binaries that support `--js`

- Render CLIs:
  - `fetch_and_render --js …` (single URL/file render)
  - `render_pages --js …` (render cached pageset HTML under `fetches/html/`)
  - `render_fixtures --js …` (render offline fixtures under `tests/pages/fixtures/`)
  - `pageset_progress run --js …` (pageset scoreboard renders)
  - `bundle_page render --js …` (offline replay from a bundle)
- Browser:
  - Windowed UI: JS is currently enabled by default (experimental; `--js` does not toggle windowed
    mode yet).
  - `browser --headless-smoke --js …` (headless smoke test mode; selects a vm-js `api::BrowserTab`
    harness — this is what the `browser --js` flag currently controls)
  - Note: the `browser` binary does not expose the shared JS budget flags.
  - Example (headless smoke test):

    ```bash
    bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
      --headless-smoke --js
    ```

  - Example (windowed UI):

    ```bash
    bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
      https://example.com/
    ```

When `--js` is enabled in the render CLIs above, they use the `vm-js`-backed
[`api::BrowserTab`](../src/api/browser_tab.rs) and drive it via `BrowserTab::run_until_stable`
(typically bounded by `--js-max-frames`; `bundle_page render --js` currently uses a fixed 50-frame
budget).

### Determinism / time model

`BrowserTab::run_until_stable` is a deterministic “converge then snapshot” loop: it does **not**
sleep in real time. It repeatedly drains runnable tasks/microtasks/timers, runs
`requestAnimationFrame` callbacks (frame turns) and `requestIdleCallback` callbacks (idle periods),
and renders frames until the document reaches a quiescent state
(or `--js-max-frames` is hit).

Time-based APIs (`setTimeout`, `requestAnimationFrame`, `requestIdleCallback`, `Date.now()`,
`performance.now()`, etc.) are
driven by the tab’s monotonic clock (`js::Clock`). Today the render CLIs use the default
`RealClock` (elapsed wall time since the tab was created), so values like `Date.now()` and
`performance.now()` can vary across runs/machines.

`run_until_stable` does **not** sleep and does not “fast-forward” the clock to the next scheduled
timer. Timers only become due if they are scheduled for `<= now` as the clock advances naturally
while work executes, so long-delay timers may not fire before the snapshot render (even if you have
frame budget remaining).

For an interactive, real-time loop (sleeping until the next wake-up), see
[`docs/live_rendering_loop.md`](live_rendering_loop.md). Deterministic embeddings can additionally
provide a `VirtualClock` to the event loop, but this is not currently exposed as a CLI flag.

### Shared JS budget flags

JS-enabled render CLIs expose a mix of:

- a “frame” budget for the outer `BrowserTab::run_until_stable` loop, and
- shared `JsExecutionArgs` knobs that map to `JsExecutionOptions` (per-spin event loop budgets + VM budgets).

Run `--help` for per-binary defaults.

- `--js-max-frames <N>`: maximum “frame” iterations while driving `run_until_stable`.
  - Available on: `fetch_and_render`, `render_pages`, `render_fixtures`, `pageset_progress run`.
  - Note: `bundle_page render --js` currently uses a fixed `max_frames=50` and does not expose `--js-max-frames`.

The following shared `JsExecutionArgs` flags are available on: `fetch_and_render`, `render_pages`,
`pageset_progress run`, and `bundle_page render`:

- `--js-max-wall-ms <MS>`: wall-time budget per event-loop “spin” (0 disables the wall-time limit).
- `--js-max-script-bytes <BYTES>`: maximum bytes accepted for a single script source (inline or external).
- `--js-max-tasks <N>` / `--js-max-microtasks <N>`: maximum tasks/microtasks executed per spin.
- `--js-max-pending-tasks <N>`, `--js-max-pending-microtasks <N>`, `--js-max-pending-timers <N>`:
  caps on queued work to prevent unbounded memory growth.
- `--js-max-instructions <N>`, `--js-max-vm-heap-bytes <BYTES>`, `--js-max-stack-depth <N>`: VM
  budgeting knobs (instruction/heap/stack limits).

Background and rationale: [`docs/js_execution_budgets.md`](js_execution_budgets.md).

Note: `render_fixtures --js` currently exposes `--js` and `--js-max-frames` only (it uses
`JsExecutionOptions::default()` and does not yet accept the shared `JsExecutionArgs` budget override
flags like `--js-max-script-bytes` / `--js-max-wall-ms` / `--js-max-tasks`, etc).

Note: the Chrome baseline scripts (`scripts/chrome_baseline.sh`, `xtask chrome-baseline-fixtures`,
etc.) use a separate JavaScript toggle of the form `--js {on|off}`.

## Convenience scripts (terminal-friendly)

These are optional wrappers for the most common loops:

- Pageset loop (thin wrapper over `bash scripts/cargo_agent.sh xtask pageset`, defaults to bundled fonts for deterministic timing): `scripts/pageset.sh`
  - Defaults to `--features disk_cache`; set `DISK_CACHE=0` or `NO_DISK_CACHE=1` or pass `--no-disk-cache` to opt out; pass `--disk-cache` to force-enable.
  - Fonts: defaults to bundled fixtures; pass `--system-fonts` (alias `--no-bundled-fonts`) to run `pageset_progress` against host system fonts (useful for Chrome accuracy diffs). `--system-fonts` forces `FASTR_USE_BUNDLED_FONTS=0` and `CI=0` for the `pageset_progress` subprocess so the behavior is predictable even when those env vars are set.
  - Supports `--jobs/-j`, `--fetch-timeout`, `--render-timeout`, `--cache-dir`, `--no-fetch`, `--refresh`, `--pages`, `--shard`, `--allow-http-error-status`, `--allow-collisions`, `--timings`, `--bundled-fonts` (default) / `--system-fonts` (alias `--no-bundled-fonts`), `--accuracy` (plus `--accuracy-baseline`, `--accuracy-baseline-dir`, `--accuracy-tolerance`, `--accuracy-max-diff-percent`, and `--accuracy-diff-dir`), and `--capture-missing-failure-fixtures` (plus `--capture-missing-failure-fixtures-out-dir`, `--capture-missing-failure-fixtures-allow-missing-resources`, and `--capture-missing-failure-fixtures-overwrite`).
  - Prefetch/report flags like `--prefetch-fonts` / `--prefetch-images` / `--prefetch-media` / `--prefetch-scripts` / `--report-json` / `--discover-only` passed after `--` are forwarded to `prefetch_assets` when disk cache is enabled.
  - Pass extra `pageset_progress run` flags after `--` (for example `--accuracy`; consider `--system-fonts` for Chrome diffs so font substitution doesn’t dominate the results).
  - Use `--dry-run` to print the underlying `bash scripts/cargo_agent.sh xtask pageset ...` command instead of executing it.
- Cached-pages Chrome-vs-FastRender diff (best-effort; non-deterministic): `scripts/chrome_vs_fastrender.sh [options] [--] [page_stem...]`
  - Wraps `scripts/chrome_baseline.sh`, `render_pages`, and `diff_renders` into one command.
  - Defaults to `viewport=1200x800`, `dpr=1.0`, JavaScript disabled (to match FastRender’s default static pipeline, where author scripts are not executed unless `--js` is enabled).
  - Disables CSS animations/transitions by default for more deterministic Chrome screenshots; set `ALLOW_ANIMATIONS=1` to opt out when debugging.
  - `scripts/chrome_baseline.sh` also patches HTML to force a light color scheme + white background by default to avoid platform dark-mode/background differences (set `ALLOW_DARK_MODE=1` or run it with `--allow-dark-mode` to opt out).
  - Writes a report at `<out>/report.html` (default: `target/chrome_vs_fastrender/report.html`).
  - Core flags: `--pages <csv>`, `--shard <index>/<total>`, `--viewport <WxH>`, `--dpr <float>`, `--jobs <n>`, `--timeout <secs>`, `--out-dir <dir>`, `--chrome <path>`, `--js {on|off}`, `--no-chrome`, `--no-fastrender`, `--diff-only`, `--tolerance <u8>`, `--max-diff-percent <float>`, `--max-perceptual-distance <float>`, `--ignore-alpha`, `--sort-by {pixel|percent|perceptual}`, `--fail-on-differences`, `--no-build`.
  - Note: this wrapper’s `--js {on|off}` flag is forwarded to the **Chrome** capture step only (it does not enable `render_pages --js`). To diff JS-enabled pages in both engines, run the steps manually (`scripts/chrome_baseline.sh --js on` + `render_pages --js ...`).
  - `--pages` accepts cached stems or URLs; URL-looking inputs are normalized to cached stems best-effort (strip scheme/leading `www.`, etc).
  - Per-step timeouts: `--chrome-timeout <secs>` / `--render-timeout <secs>` override `--timeout`.
  - Passing page stems that are not present under `fetches/html/*.html` is an error (run `fetch_pages` first).
- Offline fixture Chrome-vs-FastRender diff (deterministic; offline): `scripts/chrome_vs_fastrender_fixtures.sh [options] [--] [fixture_glob...]`
  - Thin wrapper around the canonical implementation: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff` (inherits its validations and default fixture selection).
  - Defaults to `viewport=1040x1240`, `dpr=1.0`, JavaScript disabled.
  - Chrome baselines disable CSS animations/transitions by default for deterministic screenshots; use `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures --allow-animations` to opt out when debugging.
  - Writes outputs under `<out>/` (default: `target/fixture_chrome_diff/`):
    - `<out>/chrome/`, `<out>/fastrender/`, `<out>/report.html`, `<out>/report.json`.
  - When reusing existing FastRender renders (`--no-fastrender` / `--diff-only`), xtask validates per-fixture metadata to prevent stale diffs; pass `--allow-stale-fastrender-renders` to override.
  - Core flags mirror `bash scripts/cargo_agent.sh xtask fixture-chrome-diff` (run `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --help` for details; the wrapper forwards through newer selection flags like `--from-progress` / `--all-fixtures`).
  - Note: `xtask fixture-chrome-diff --js {on|off}` currently toggles JavaScript in the **Chrome** baseline step only; FastRender renders are still static by default. If you need JS-enabled FastRender fixture renders, run `render_fixtures --js` directly (and then `diff_renders`).
  - Legacy `--chrome-out-dir` / `--fastr-out-dir` / `--report-html` / `--report-json` flags are still accepted but must match the `<out>/chrome` / `<out>/fastrender` / `<out>/report.*` layout.
- Run any command under a hard memory cap (uses `prlimit` when available): `bash scripts/run_limited.sh --as 64G -- <command...>`
- Profile one page with samply (saves profile + prints summary): `scripts/profile_samply.sh <stem|--from-progress ...>` (builds `pageset_progress` with `disk_cache`)
- Profile one page with perf: `scripts/profile_perf.sh <stem|--from-progress ...>` (builds `pageset_progress` with `disk_cache`)
- Summarize a saved samply profile: `scripts/samply_summary.py <profile.json.gz>`

The full pageset workflow is:

`fetch_pages` (HTML) → `prefetch_assets` (CSS/@import/fonts into `fetches/assets/` by default; override with `--cache-dir <dir>`) → `pageset_progress` (render + write `progress/pages/*.json`).

`bash scripts/cargo_agent.sh xtask pageset` runs all three steps (the prefetch step is skipped when disk cache is disabled). `scripts/pageset.sh` is a thin convenience wrapper over `bash scripts/cargo_agent.sh xtask pageset` (kept for backwards-compatible flags/env defaults and muscle memory).

Pageset wrappers enable the disk-backed subresource cache by default, persisting assets under
`fetches/assets/` (override with `--cache-dir <dir>`) for repeatable/offline runs. Set
`NO_DISK_CACHE=1` or `DISK_CACHE=0` (or pass
`--no-disk-cache` to the wrappers) to force in-memory-only fetches. Pass `--disk-cache` to
`bash scripts/cargo_agent.sh xtask pageset` (or `scripts/pageset.sh`) to override an ambient `NO_DISK_CACHE=1` /
`DISK_CACHE=0` environment when you explicitly want the on-disk cache enabled.

## HTTP fetch knobs (env vars)

The pageset-oriented CLI binaries (`fetch_pages`, `prefetch_assets`, `render_pages`, `fetch_and_render`,
and `pageset_progress`) all build their network fetcher through
[`cli_utils::render_pipeline::build_http_fetcher`](../src/cli_utils/render_pipeline.rs) (imported as
`fastrender::cli_utils as common` in the bins), so they honor the `FASTR_HTTP_*` environment variables
documented in [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning).

This includes backend selection (`FASTR_HTTP_BACKEND`), browser header profiles (`FASTR_HTTP_BROWSER_HEADERS`), and retry logging (`FASTR_HTTP_LOG_RETRIES`). `bash scripts/cargo_agent.sh xtask pageset`, `scripts/pageset.sh`, and `pageset_progress` worker subprocesses inherit these env vars automatically—set them once on the outer command when triaging hard fetch failures.

Example:

```bash
FASTR_HTTP_BACKEND=reqwest FASTR_HTTP_BROWSER_HEADERS=1 bash scripts/cargo_agent.sh xtask pageset --pages tesco.com
```

Note: `fetch_pages` skips URLs already cached under `fetches/html/`. When iterating on HTTP fetch knobs for document fetch failures, re-run with `fetch_pages --refresh` (or delete the cached HTML) so the network path is exercised.

Example (re-fetch HTML for one page with an explicit backend + browser headers):

```bash
FASTR_HTTP_BACKEND=reqwest FASTR_HTTP_BROWSER_HEADERS=1 \
  bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_pages -- --refresh --pages tesco.com
```

## `bash scripts/cargo_agent.sh xtask`

`bash scripts/cargo_agent.sh xtask` is the preferred entry point for common workflows; it wraps the binaries below but keeps them usable directly.

- Help: `bash scripts/cargo_agent.sh xtask --help`
- Capability map (breadth-first roadmap): `bash scripts/cargo_agent.sh xtask capability-map` (writes `docs/capability_map.md` by default)
- Tests: `bash scripts/cargo_agent.sh xtask test [core|style|fixtures|wpt|all]`
- Refresh goldens: `bash scripts/cargo_agent.sh xtask update-goldens [all|fixtures|reference|wpt|pages]` (sets `UPDATE_GOLDEN`, `UPDATE_WPT_EXPECTED`, or `UPDATE_PAGES_GOLDEN` as appropriate)
- Pageset scoreboard (`fetch_pages` → `prefetch_assets` → `pageset_progress` when disk cache is enabled; bundled fonts by default): `bash scripts/cargo_agent.sh xtask pageset [--pages example.com,news.ycombinator.com] [--shard 0/4] [--no-fetch] [--refresh] [--allow-http-error-status] [--allow-collisions] [--timings] [--disk-cache] [--no-disk-cache] [--cache-dir <dir>] [--bundled-fonts|--system-fonts] [--cascade-diagnostics] [--cascade-diagnostics-slow-ms 500] [--accuracy] [--accuracy-baseline existing|chrome] [--accuracy-baseline-dir <dir>] [--accuracy-tolerance <u8>] [--accuracy-max-diff-percent <float>] [--accuracy-diff-dir <dir>] [--capture-missing-failure-fixtures] [-- <pageset_progress args...>]`
  - Sharded example: `bash scripts/cargo_agent.sh xtask pageset --shard 0/4` (applies to fetch + prefetch (disk cache only) + render; add `--no-fetch` to reuse cached pages)
  - Forward compatibility gates when needed: `--compat-profile site` and/or `--dom-compat compat` are passed through to `pageset_progress run` but remain off by default.
  - Fonts: bundled fonts are best for deterministic perf/timing; use `--system-fonts` when generating `pageset_progress run --accuracy` metrics against Chrome screenshots so diffs are less dominated by font substitution. `--system-fonts` forces `FASTR_USE_BUNDLED_FONTS=0` and `CI=0` for the `pageset_progress` subprocess so host font usage is predictable even when those env vars are set.
  - Disk cache directory override: `--cache-dir <dir>` is forwarded to both `prefetch_assets` and `pageset_progress` so the warmed cache matches the render step (defaults to `fetches/assets/`).
  - Disk cache tuning flags passed after `--` (e.g. `--disk-cache-max-bytes`, `--disk-cache-max-age-secs`, `--disk-cache-lock-stale-secs`) are also forwarded to `prefetch_assets` when it runs.
  - Prefetch tuning flags passed after `--` (e.g. `--prefetch-fonts`, `--prefetch-images`, `--prefetch-media`, `--prefetch-scripts`, `--prefetch-iframes`) are also forwarded to `prefetch_assets` when it runs.
  - Accuracy capture: `--accuracy` stores Chrome-vs-FastRender diff metrics per ok page in `progress/pages/*.json`.
    - Defaults to comparing against existing baselines under `fetches/chrome_renders/`.
    - Use `--accuracy-baseline chrome` to auto-generate missing baseline PNGs via `scripts/chrome_baseline.sh`.
  - Cascade triage: `--cascade-diagnostics` re-runs slow-cascade ok pages (defaults to 500ms threshold; override with `--cascade-diagnostics-slow-ms`) plus cascade timeouts with cascade profiling enabled, then merges `diagnostics.stats.cascade` into the committed progress JSON.
  - Fixture capture helper: `--capture-missing-failure-fixtures` scans `progress/pages/*.json` after the run and, for each `status != ok` page missing `tests/pages/fixtures/<stem>/index.html`, captures a bundle from the warmed disk cache (`bundle_page cache`) and imports it via `bash scripts/cargo_agent.sh xtask import-page-fixture`.
    - Requires disk cache enabled (either by default, or explicitly via `--disk-cache`; it is skipped with a warning when disk cache is disabled).
    - Override intermediate bundle output: `--capture-missing-failure-fixtures-out-dir <dir>` (default: `target/pageset_failure_fixture_bundles`).
    - Use `--capture-missing-failure-fixtures-allow-missing-resources` to allow incomplete caches (`bundle_page cache --allow-missing` + `import-page-fixture --allow-missing`).
    - Use `--capture-missing-failure-fixtures-overwrite` to replace existing fixture directories on import.
- Pageset diff: `bash scripts/cargo_agent.sh xtask pageset-diff [--baseline <dir>|--baseline-ref <git-ref>] [--no-run] [--fail-on-regression] [--fail-on-missing-stages] [--fail-on-missing-stage-timings] [--fail-on-slow-ok-ms <ms>]`
  - Extracts `progress/pages` from the chosen git ref by default and compares it to the freshly updated scoreboard.
  - `--fail-on-regression` also enables the missing-stage gates and `--fail-on-slow-ok-ms=5000` by default (use `--no-fail-on-missing-stages` / `--no-fail-on-missing-stage-timings` / `--no-fail-on-slow-ok` to opt out, or pass `--fail-on-slow-ok-ms <ms>` to override the default threshold).
  - Stage-bucket sanity guardrail: when changing stage timing accounting, run `pageset_progress report --fail-on-stage-sum-exceeds-total` (tune `--stage-sum-tolerance-percent`, default 10%) to catch double-counting/CPU-sum mixups early.
- Render one page: `bash scripts/cargo_agent.sh xtask render-page --url https://example.com --output out.png [--viewport 1200x800 --dpr 1.0 --full-page]`
- Browser UI (windowed):
  - HUD + perf log to stdout (JSONL): `timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release --hud --perf-log about:test-layout-stress`
  - Capture perf log to a file (JSONL): `timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release --perf-log-out target/browser_perf.jsonl about:test-layout-stress`
  - Summarize a captured log: `timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- --input target/browser_perf.jsonl`
- Offline fixture “page loop” (FastRender render + optional overlay + optional inspect dumps + optional Chrome diff): `bash scripts/cargo_agent.sh xtask page-loop --fixture <stem> [--debug] [--overlay --inspect-dump-json --write-snapshot --chrome]`
  - Tip: pass `--debug` to skip `--release` for the FastRender/diff steps when you want faster rebuilds (slower runtime).
  - When you want “the next highest-signal page”, you can select a single fixture from committed progress JSON:
    - Worst-accuracy ok page: `bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-worst-accuracy 1 --debug --overlay --inspect-dump-json --write-snapshot --chrome`
    - First failing page: `bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --only-failures --debug --overlay --inspect-dump-json --write-snapshot --chrome`
    - Slowest page: `bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-slowest 1 --debug --overlay --inspect-dump-json --write-snapshot --chrome`
    - Filter by hotspot: `bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-worst-accuracy 1 --hotspot layout --debug --overlay --inspect-dump-json --write-snapshot --chrome`
- Diff renders: `bash scripts/cargo_agent.sh xtask diff-renders --before fetches/renders/baseline --after fetches/renders/new [--output target/render-diffs]`
  - Supports directory diffs (recursive) and PNG file-to-file diffs.
  - Writes `diff_report.html` / `diff_report.json` into `--output` (diff images under `diff_report_files/diffs/`).
  - `--threshold` controls the per-channel tolerance passed through to the underlying `diff_renders --tolerance`.
  - `--ignore-alpha`, `--max-diff-percent`, `--max-perceptual-distance`, `--sort-by`, and `--shard` are forwarded to `diff_renders` (defaults match the historical `--max-diff-percent=0` behavior).
  - Use `--fail-on-differences` to exit non-zero when the report contains diffs/missing/error entries.
  - Use `--no-build` to reuse an existing `target/release/diff_renders` binary (skips `bash scripts/cargo_agent.sh build`).
  - Chrome baseline screenshots for offline fixtures (local-only; not committed): `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures`
  - Disables CSS animations/transitions by default for deterministic baselines; pass `--allow-animations` to opt out.
  - Forces a light color scheme + white background by default to avoid platform dark-mode/background differences; pass `--allow-dark-mode` to opt out.
  - Chrome-vs-FastRender diff report for offline fixtures (deterministic; offline): `bash scripts/cargo_agent.sh xtask fixture-chrome-diff`
    - Defaults to the curated `pages_regression` fixture set in `tests/regression/pages.rs`.
    - Pass `--all-fixtures` to render every fixture under `tests/pages/fixtures/`.
    - Select fixtures from pageset progress JSON (instead of a manual `--fixtures a,b,c` list):
      - Failing pages: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --from-progress progress/pages --only-failures`
      - Worst accuracy pages: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --from-progress progress/pages --top-worst-accuracy 10`
    - FastRender writes `<out>/fastrender/<fixture>.json` alongside each PNG with render settings, fixture input fingerprints, and status/timing.
    - When reusing an existing FastRender output directory (`--no-fastrender` / `--diff-only`), xtask validates the metadata matches the requested `--viewport/--dpr/--media/--fit-canvas-to-content/--timeout`, font config, and fixture input fingerprints. Missing/incomplete metadata warns by default; pass `--require-fastrender-metadata` to fail instead. Use `--allow-stale-fastrender-renders` to downgrade fingerprint mismatches to warnings.
  - Import a bundled capture into a `pages_regression` fixture: `bash scripts/cargo_agent.sh xtask import-page-fixture <bundle_dir|.tar> <fixture_name> [--output-root tests/pages/fixtures --overwrite --dry-run --include-media]`
    - Relative `<bundle>` and `--output-root` paths are resolved relative to the repository root so the command behaves consistently even when invoked from subdirectories (pass absolute paths to override).
    - Media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`) are rewritten to deterministic empty `assets/missing_<hash>.<ext>` placeholder files by default (fixtures stay small and safe to commit).
      - Opt in to vendoring playable media with `--include-media`.
      - When enabled, media bytes are capped by `--media-max-bytes` (total, default **5 MiB**) and `--media-max-file-bytes` (per file, default **2 MiB**). Set either to `0` to disable the limit.
  - Recapture and (re)import offline page fixtures from a manifest (pageset guardrails by default): `bash scripts/cargo_agent.sh xtask recapture-page-fixtures [--capture-mode cache|crawl|render] [--only stripe.com] [--overwrite]`
  - Validate that offline page fixtures do not reference network resources: `bash scripts/cargo_agent.sh xtask validate-page-fixtures [--only stripe.com]`
- Update `tests/pages/pageset_guardrails.json` from the pageset scoreboard: `bash scripts/cargo_agent.sh xtask update-pageset-guardrails`
  - Defaults to `--strategy coverage`; always includes every `timeout`/`panic`/`error` page from `progress/pages/*.json` for offline triage, then adds a small set of slow `ok` pages for hotspot coverage.
  - Use `--strategy worst_accuracy` to select `ok` pages with the worst Chrome-vs-FastRender diffs (by `accuracy.diff_percent`, tie-breaking by perceptual distance).
  - Warns when failures exceed `--count`.
  - Defaults to crawl-based capture for missing fixtures.
  - Use `--capture-missing-fixtures --capture-mode cache` to build bundles from warmed pageset caches without network access (requires `--features disk_cache`; pass `--asset-cache-dir <dir>`/`--cache-dir <dir>` when the warmed cache is not under `fetches/assets/`).
  - Use `--capture-mode render` to force render-driven bundle capture.
  - Use `--bundle-fetch-timeout-secs <secs>` to bound per-request network time during capture when using `render`/`crawl`.
  - Note: `update-pageset-timeouts` remains as an alias; the legacy `tests/pages/pageset_timeouts.json` file is kept in sync for backwards compatibility.
- Update `budget_ms` entries in `tests/pages/pageset_guardrails.json` based on offline `perf_smoke` timings: `bash scripts/cargo_agent.sh xtask update-pageset-guardrails-budgets --write --isolate` (runs `perf_smoke --suite pageset-guardrails` with bundled fonts, then rewrites each fixture's `budget_ms` as `total_ms * --multiplier` clamped/rounded; use `--dry-run` to preview). Note: this xtask defaults to `--no-isolate` unless you pass `--isolate` (even though `perf_smoke --suite pageset-guardrails` auto-enables isolation). (`update-pageset-timeout-budgets` remains as an alias; the legacy `tests/pages/pageset_timeouts.json` file is kept in sync for backwards compatibility.)
- Perf smoke: `bash scripts/cargo_agent.sh xtask perf-smoke [--suite core|pageset-guardrails|all] [--only flex_dashboard,grid_news]`
  `[--top 5 --baseline baseline.json --threshold 0.05 --count-threshold 0.20 --fail-on-regression --fail-on-failure --fail-fast]`
  `[--fail-on-budget] [--isolate|--no-isolate] [--fail-on-missing-fixtures|--allow-missing-fixtures] [-- <extra perf_smoke args...>]`
  - Offline fixtures, bundled fonts, JSON summary at `target/perf_smoke.json`, per-fixture `status`/`error`.
  - The `pageset-guardrails` suite runs in isolation and **fails on missing fixtures by default**; pass `--allow-missing-fixtures` to skip missing pageset-guardrails captures for local partial runs.
  - Pass `--fail-on-budget` to exit non-zero when a fixture exceeds its `budget_ms`.
  - Pass `--count-threshold`, `--fail-fast`, and `--fail-on-failure` to tune count regression and fixture failure gating.
- Browser UI responsiveness harness (headless):
  - Via `xtask`: `timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json`
  - Direct: `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin ui_perf_smoke -- --output target/ui_perf_smoke.json`
  - See [`docs/perf-logging.md#browser-responsiveness`](perf-logging.md#browser-responsiveness) for metric mapping (TTFP, scroll/resize frame times, input latency).
  - Determinism: defaults to a single Rayon thread when neither `--rayon-threads` nor `RAYON_NUM_THREADS` are set; the output JSON records `run_config.effective_rayon_threads` and `run_config.rayon_threads_source`.

`render-page` wraps `fetch_and_render` in release mode by default (add `--debug` to keep a debug build).

## `fetch_pages`

- Purpose: fetch a curated set of real pages and cache HTML under `fetches/html/` (and metadata alongside).
- Entry: `src/bin/fetch_pages.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_pages -- --help`
- HTTP fetch tuning: honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
- Note: `fetch_pages` only caches HTML; it does not use the disk-backed subresource cache (`--cache-dir` is a flag on `prefetch_assets`/`pageset_progress` and pageset wrappers).
- Cached metadata sidecars: `fetches/html/<stem>.html.meta` stores response metadata (key/value lines like `content-type`, `status`, `url`, `referrer-policy`, and `content-security-policy`) that downstream tools use when replaying cached HTML.
  - `content-security-policy:` may appear multiple times (one per `Content-Security-Policy` response header value) so offline replays can enforce CSP deterministically.
- Supports deterministic sharding with `--shard <index>/<total>` when splitting the page list across workers.
- Cache filenames and `--pages` filters use the canonical stem from `normalize_page_name` (strip the scheme and a leading `www.`). Colliding stems fail fast unless you opt into `--allow-collisions`, which appends a deterministic suffix.
- `--allow-http-error-status` treats HTTP 4xx/5xx responses as fetch successes and allows caching them for debugging (e.g. Cloudflare challenges). When used with `--refresh`, `fetch_pages` will avoid clobbering an existing cached snapshot with a transient 4xx/5xx response unless the existing snapshot is also known to be an HTTP error page.
- **Migration:** cached HTML written before canonical stems were enforced may be ignored. Delete stale `fetches/html/*.html` entries and re-run `fetch_pages`.

## `prefetch_assets`

- Purpose: warm the subresource cache (`fetches/assets/`) by prefetching linked stylesheets and their `@import` chains (plus referenced fonts) for the cached pages under `fetches/html/`. Optional flags can also prefetch additional HTML-linked subresources (images, media sources, iframes/embeds, icons, video posters, scripts). This makes subsequent pageset renders more repeatable and reduces time spent fetching during `pageset_progress`.
- Entry: `src/bin/prefetch_assets.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin prefetch_assets -- --help`
- HTTP fetch tuning: honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
- Requires the `disk_cache` cargo feature (otherwise it only supports `--capabilities` and exits with an error) so warmed cache entries persist across processes.
- Tooling: `prefetch_assets --capabilities` (alias `--print-capabilities-json`) prints stable JSON describing which optional knobs are supported. When `disk_cache` is unavailable it reports `disk_cache_feature=false` and all optional flags false; pageset wrappers use this instead of grepping the repo source tree.
- Key flags: page selection (`--pages`), deterministic sharding (`--shard <index>/<total>`), parallelism (`--jobs`), and fetch timeout (`--timeout`). See `--help` for the full flag list.
  - Cache directory: `--cache-dir <dir>` overrides the disk-backed cache location (defaults to `fetches/assets/`). Use the same value for `pageset_progress` so warmed entries are reused during render.
  - Optional subresource warming:
    - `--prefetch-fonts`: prefetch font URLs referenced by fetched CSS (true/false, defaults to true).
    - `--prefetch-images`: prefetch common HTML image-like assets (`<img>`, `<picture><source srcset>`, video posters, icons/manifests (including `mask-icon`), and `<link rel="preload" as="image">`). This uses the renderer's responsive image selection (DPR/viewport + `srcset`/`sizes`/`picture`) instead of enumerating every candidate.
      - Safety valves: `--max-images-per-page` and `--max-image-urls-per-element` bound image prefetching when pages contain large `srcset` lists.
      - Note: if you only need a small subset (e.g. icons or video posters) without fetching all `<img>` content, use `--prefetch-icons` / `--prefetch-video-posters` instead.
    - `--prefetch-media`: prefetch playable media sources referenced directly from HTML (`<video src>`, `<audio src>`, `<source src>`). This is opt-in because media files can be large.
      - Budgets: `--max-media-bytes-per-file` (default **10 MiB**) and `--max-media-bytes-per-page` (default **50 MiB**). Set either to `0` to disable the cap.
      - When caps are enabled, `prefetch_assets` probes size using a partial fetch and **skips** media URLs that would exceed the budgets (recorded as `false` in the JSON report).
      - Current limitation: `<track src>` and `<link rel="preload" as="video|audio|track">` are not discovered by the media prefetcher yet.
    - `--prefetch-scripts`: prefetch script resources referenced directly from HTML (`<script src>`, script `preload`, `modulepreload`). This is opt-in because it can significantly increase cache size.
    - `--prefetch-iframes` (alias `--prefetch-documents`): prefetch `<iframe src>` documents and best-effort warm their linked stylesheets (and images when `--prefetch-images` is enabled).
    - `--prefetch-embeds`: prefetch `<object data>` and `<embed src>` subresources. If the fetched resource is HTML, it is treated like a nested document and its CSS/images can also be warmed (same behavior as `--prefetch-iframes`).
    - `--prefetch-icons`: prefetch icon resources referenced by `<link rel=icon|shortcut icon|apple-touch-icon|mask-icon href=...>` without enabling full `--prefetch-images` (note: `--prefetch-images` already includes these).
    - `--prefetch-video-posters`: prefetch `<video poster>` images without enabling full `--prefetch-images` (note: `--prefetch-images` already includes posters).
    - `--prefetch-css-url-assets`: prefetch non-CSS assets referenced via CSS `url(...)` (including in `@import`ed stylesheets).
    - `--max-images-per-page`: cap how many image-like elements are considered during HTML discovery when `--prefetch-images` is enabled.
    - `--max-image-urls-per-element`: cap how many URLs are prefetched per image element (primary + fallbacks) when `--prefetch-images` is enabled.
    - `--max-discovered-assets-per-page`: safety valve for pathological pages (0 disables the cap).
- Disk cache tuning flags (`--disk-cache-max-age-secs`, `--disk-cache-max-bytes`, `--disk-cache-lock-stale-secs`, or the corresponding `FASTR_DISK_CACHE_*` env vars) match the pageset render binaries.
- Reporting/debugging:
  - `--dry-run` (alias `--discover-only`): scan cached HTML/CSS and populate the report without performing any network/disk cache fetches.
  - `--report-json <path>`: write a deterministic JSON manifest of discovered assets and fetch outcomes.
  - `--report-per-page-dir <dir>`: write one JSON report per page stem under `<dir>/<stem>.json`.
  - `--max-report-urls-per-kind <n>`: cap the number of sampled URL strings per section (default 50; 0 => counts only).
  - Note: the report tracks unique URL sets. A URL can appear in both `fetched` and `failed` if some attempts succeeded and others failed (for example, `Image` fetch succeeded but `ImageCors` failed).

Example (prefetch CSS + media into the disk cache for one cached page and write a report):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin prefetch_assets -- \
  --pages w3.org \
  --prefetch-media \
  --max-media-bytes-per-file $((2*1024*1024)) \
  --max-media-bytes-per-page $((8*1024*1024)) \
  --report-json target/prefetch_assets_media.w3.org.json
```

## `disk_cache_audit`

- Purpose: audit (and optionally clean) the disk-backed subresource cache directory (defaults to `fetches/assets/`) for common pageset poisoning cases such as cached 4xx/5xx responses or HTML responses stored for URLs that look like static subresources (CSS/images/fonts).
- Entry: `src/bin/disk_cache_audit.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin disk_cache_audit -- --help`
- Typical usage:
  - Audit: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin disk_cache_audit --`
  - JSON output (stable keys): `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin disk_cache_audit -- --json`
  - Cleanup: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin disk_cache_audit -- --delete-http-errors --delete-html-subresources --delete-error-entries`
  - Match non-default cache directory: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin disk_cache_audit -- --cache-dir <dir>`

## `render_pages`

- Purpose: render all cached HTML in `fetches/html/` to PNGs (plus per-page logs and `_summary.log`) (defaults to `fetches/renders/`; override with `--out-dir`).

### Chrome baseline screenshots (from cached HTML)

If you want a “known-correct engine” visual baseline for the same cached HTML that FastRender renders, you can use headless Chrome/Chromium to screenshot `fetches/html/*.html`.

This workflow is **best-effort / non-deterministic** because it still depends on live subresources. Prefer the offline fixture loop (`render_fixtures` + `bash scripts/cargo_agent.sh xtask fixture-chrome-diff`) once you’ve captured a deterministic repro.

For convenience, `scripts/chrome_vs_fastrender.sh` wraps the full cached-pages loop and writes a
report at `target/chrome_vs_fastrender/report.html` by default.

```bash
# Install deps on Ubuntu (python + fonts + chrome/chromium):
scripts/install_chrome_baseline_deps_ubuntu.sh

# 1) Ensure cached HTML exists:
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_pages

# 2) Screenshot with Chrome/Chromium (JS disabled by default; injects <base href=...> from *.html.meta):
scripts/chrome_baseline.sh

# 3) Render FastRender output:
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_pages

# 4) Diff the two directories:
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin diff_renders -- \
  --before fetches/chrome_renders \
  --after fetches/renders \
  --json target/chrome_vs_fastrender/report.json \
  --html target/chrome_vs_fastrender/report.html
```

For a one-command wrapper that runs all three steps (Chrome baseline → FastRender → diff report),
use:

```bash
scripts/chrome_vs_fastrender.sh [--] [stem...]
```

Notes:
- This is not fully deterministic (live subresources can change); it’s still excellent for rapid “why is our render different from Chrome on the same HTML?” debugging.
- Pass `--chrome /path/to/chrome` (or set `CHROME_BIN=/path/to/chrome`) if auto-detection fails.
- `scripts/chrome_baseline.sh` prefers Chrome's `--headless=new` mode but automatically retries with legacy `--headless` on older Chrome versions.
- Passing page stems that are not present under `fetches/html/*.html` is an error (run `fetch_pages` first).
- `scripts/chrome_baseline.sh` supports `--shard <index>/<total>` (0-based) for deterministic sharding of the baseline capture set.
- Entry: `src/bin/render_pages.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_pages -- --help`
- HTTP fetch tuning: honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
- Accepts `--shard <index>/<total>` to render a slice of the cached pages in a stable order.
- `--pages` (and positional filters) use the same canonical stems as `fetch_pages` (strip scheme + leading `www.`). Cached filenames are normalized when matching filters so `www.`/non-`www` variants map consistently.
- JavaScript: pass `--js` to execute author scripts via the `vm-js` `BrowserTab` harness, driven by `BrowserTab::run_until_stable` (bounded by `--js-max-frames`). See [JavaScript execution (`--js`)](#javascript-execution---js) above for the shared time model + budget flags.
- Output directory: `--out-dir <dir>` overrides where renders/logs are written (defaults to `fetches/renders/`).
- Disk cache directory: `--cache-dir <dir>` overrides the disk-backed subresource cache location (defaults to `fetches/assets/`; only has an effect when built with `--features disk_cache`).
- Optional outputs:
  - `--diagnostics-json` writes `<out-dir>/<page>.diagnostics.json` containing status, timing, and `RenderDiagnostics`.
  - `--dump-intermediate {summary|full}` emits per-page summaries or full JSON dumps of DOM/composed DOM/styled/box/fragment/display-list stages (use `--only-failures` to gate large artifacts on errors); `full` also writes a combined `<out-dir>/<page>.snapshot.json` pipeline snapshot.
- Layout fan-out defaults to `auto` (only engages once the box tree is large enough and has sufficient independent sibling work); use `--layout-parallel off` to force serial layout, `--layout-parallel on` to force fan-out, or tune thresholds with `--layout-parallel-min-fanout`, `--layout-parallel-auto-min-nodes`, and `--layout-parallel-max-threads`.
- Worker Rayon threads: in the default per-page worker mode, `render_pages` sets `RAYON_NUM_THREADS` for each worker process to `available_parallelism()/jobs` (min 1, additionally clamped by a detected cgroup CPU quota on Linux) to avoid CPU oversubscription. Set `RAYON_NUM_THREADS` in the parent environment to override this.

Example (render one cached page with JavaScript enabled into a separate output directory):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_pages -- \
  --pages stripe.com \
  --out-dir target/renders_js \
  --js --js-max-frames 20
```

### Offline fixture Chrome-vs-FastRender diffs (deterministic)

For pixel-accuracy work where you want stable evidence artifacts without network instability, use the self-contained offline fixtures under `tests/pages/fixtures/*`:

```bash
# Canonical one-command workflow (Chrome baseline + FastRender renders + diff report):
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --fixtures grid_news
# Report: target/fixture_chrome_diff/report.html

# Convenience wrapper (thin shim around `bash scripts/cargo_agent.sh xtask fixture-chrome-diff`):
scripts/chrome_vs_fastrender_fixtures.sh grid_news
```

Outputs (all under `target/fixture_chrome_diff/` by default):

- Chrome: `<out>/chrome/<fixture>.png` + `*.chrome.log` + `*.json` metadata
- FastRender: `<out>/fastrender/<fixture>.png` + `*.log`
- Diff report: `<out>/report.html` (plus `report.json` and embedded images under `report_files/`)

See `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --help` for the full set of selection/output flags; the wrapper script inherits that behavior.

You can also run the pieces independently (mostly useful when iterating on one step):

```bash
bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures --out-dir target/fixture_chrome_diff/chrome --fixtures grid_news
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_fixtures -- --fixtures grid_news --out-dir target/fixture_chrome_diff/fastrender
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin diff_renders -- \
  --before target/fixture_chrome_diff/chrome \
  --after target/fixture_chrome_diff/fastrender \
  --json target/fixture_chrome_diff/report.json \
  --html target/fixture_chrome_diff/report.html
```

Both `scripts/chrome_fixture_baseline.sh` and `render_fixtures` support `--shard <index>/<total>` (0-based) for deterministic parallelism.

## `render_fixtures`

- Purpose: render offline page fixtures (under `tests/pages/fixtures/`) to PNGs for deterministic debugging and Chrome-vs-FastRender diff reports.
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_fixtures -- --help`
- Defaults: fixed, deterministic viewport/DPR (1040x1240 @ 1.0) unless overridden.
- Offline policy: fixtures are rendered **without network access**; only `file://` and `data:` subresources are allowed.
- Fixture completeness: any blocked `http(s)://` subresource (network access) is treated as a fixture failure so captures stay self-contained/offline. Other fetch errors are reported in `<fixture>.log` (and in `diagnostics.json` when `--write-snapshot` is enabled).
- JavaScript (optional): pass `--js` to execute fixture scripts via the `vm-js` `BrowserTab` harness before capturing the final pixels (bounded by `--js-max-frames`). See [JavaScript execution (`--js`)](#javascript-execution---js) for the time model.
- Fonts: uses bundled fonts (`FontConfig::bundled_only`) so outputs are stable across machines (pass `--system-fonts` to opt into system font discovery).
- Output: by default writes `<fixture>.png` into `target/fixture_renders/` (override with `--out-dir`), plus `<fixture>.log` and `_summary.log`.
- Optional snapshot: `--write-snapshot` writes `<out-dir>/<fixture>/snapshot.json` and `<out-dir>/<fixture>/diagnostics.json` (for later `diff_snapshots`).
- Optional determinism harness: `--repeat <N>` renders each fixture multiple times in-process and compares raw pixel output (`Pixmap::data()` bytes) to detect scheduling-dependent nondeterminism.
  - Use `--shuffle` (and optionally `--seed`) to vary fixture ordering between repeats.
  - Use `--fail-on-nondeterminism` to exit non-zero when any fixture produces multiple pixel variants.
  - Use `--save-variants` to write each distinct variant under `<out-dir>/<fixture>/nondeterminism/<k>.png` plus `report.txt`.
  - Use `--reset-paint-scratch` to clear thread-local paint/filter scratch buffers between repeats (useful to bisect scratch-reuse-related nondeterminism).
- Core flags:
  - Selection: `--fixtures <csv>` (comma-separated stems).
  - Paths: `--fixtures-dir <dir>`, `--out-dir <dir>`.
  - Render params: `--viewport <WxH>`, `--dpr <float>`, `--media {screen|print}`, `--timeout <secs>`, `--js`, `--js-max-frames <N>`.
  - Parallelism: `--jobs/-j <n>`, `--shard <index>/<total>`.
  - Fonts: `--system-fonts`, `--font-dir <dir>` (repeatable).

Example (render one fixture with JavaScript enabled):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin render_fixtures -- \
  --fixtures grid_news \
  --out-dir target/fixture_renders_js \
  --js --js-max-frames 20
```

## `bash scripts/cargo_agent.sh xtask chrome-baseline-fixtures`

- Purpose: render the same offline fixtures in headless Chrome/Chromium to generate a “known-correct engine” PNG baseline for comparisons.
- Notes:
  - These baselines are **local-only** artifacts under `target/` (they are not committed).
  - Defaults match the fixture runner viewport/DPR (1040x1240 @ 1.0) unless overridden.
  - JavaScript is disabled by default to match FastRender’s default static pipeline (enforced via injected CSP; enable `--js on` to execute author scripts).
  - Prefers Chrome's `--headless=new` mode but automatically retries with legacy `--headless` on older Chrome versions (the chosen mode is recorded in `<fixture>.json` metadata).
  - `<fixture>.json` metadata also records hashes of fixture inputs (`input_sha256` for `index.html`, `assets_sha256` for other fixture-local files, plus `shared_assets_sha256` for the fixtures-root `assets/` directory) so `fixture-chrome-diff --no-chrome` can detect stale baselines.
  - Pass `--chrome /path/to/chrome` (or set `CHROME_BIN=/path/to/chrome`) if auto-detection fails.
  - Output defaults to `target/chrome_fixture_renders/<fixture>.png` plus `<fixture>.chrome.log` (includes the Chrome command line) and `<fixture>.json` metadata alongside.
- Core flags:
  - Selection: `--fixtures <csv>` (alias `--only`) or positional fixture names.
  - Paths: `--fixture-dir <dir>` (aliases `--fixtures-dir`, `--fixtures-root`), `--out-dir <dir>`.
  - Render params: `--viewport <WxH>`, `--dpr <float>`, `--media {screen|print}`, `--timeout <secs>`, `--js {on|off}`.
  - Parallelism: `--shard <index>/<total>`.

## `bash scripts/cargo_agent.sh xtask fixture-chrome-diff`

- Purpose: render offline fixtures with FastRender and headless Chrome, then generate a single HTML report comparing the two.
- Typical usage:
  - `bash scripts/cargo_agent.sh xtask fixture-chrome-diff` (writes `target/fixture_chrome_diff/report.html` and prints the path)
  - Select fixtures: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --fixtures grid_news,flex_dashboard`
  - Also generate fragment overlays: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --overlay --fixtures grid_news,flex_dashboard`
  - Re-run only the diff step (reuse the existing `<out>/chrome` renders; validated against current fixture inputs): `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --out-dir target/fixture_chrome_diff --no-chrome`
  - Re-run only the diff step (reuse both Chrome + FastRender renders): `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --out-dir target/fixture_chrome_diff --no-chrome --no-fastrender`
  - Shorthand for diff-only: `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --out-dir target/fixture_chrome_diff --diff-only`
- Output layout:
  - `<out>/chrome/<fixture>.png` (+ `<fixture>.chrome.log`, `<fixture>.json` metadata)
  - `<out>/fastrender/<fixture>.png` (rendered by `render_fixtures`)
  - `<out>/overlay/<fixture>.png` (when `--overlay` is enabled; rendered by `inspect_frag`)
  - `<out>/report.html`, `<out>/report.json`
- Notes:
  - JavaScript is disabled by default. The `--js {on|off}` flag currently toggles JavaScript in the **Chrome** baseline step only (it controls whether an injected CSP blocks scripts). FastRender fixture renders are still static by default; use `render_fixtures --js` directly if you want JS-enabled FastRender fixture renders.
  - When `--no-chrome` is set, existing Chrome baseline metadata (`<out>/chrome/<fixture>.json`) is validated against the current `--viewport`, `--dpr`, `--media`, and `--js` values when present. Missing metadata emits a warning; use `--require-chrome-metadata` to fail fast.
  - When `--no-chrome` is set, the command checks the stored Chrome baseline metadata against the current fixture `index.html`, other fixture-local inputs like `assets/`, `styles.css`, etc, and the shared fixtures-root `assets/` directory (when available). If the fixture inputs have changed since the baseline was generated, it fails fast to avoid misleading diffs. Use `--allow-stale-chrome-baselines` to downgrade this to a warning.
  - Pass `--write-snapshot` to also write per-fixture snapshots/diagnostics for later `diff_snapshots` (equivalent to `render_fixtures --write-snapshot`).
- Core flags:
  - Selection: `--fixtures <csv>`, `--shard <index>/<total>`
  - Paths: `--fixtures-dir <dir>`, `--out-dir <dir>`
  - Render params: `--viewport <WxH>`, `--dpr <float>`, `--timeout <secs>`, `--media {screen|print}`, `--jobs/-j <n>`, `--write-snapshot`, `--overlay`, `--js {on|off}`
  - Diff params: `--tolerance <u8>`, `--max-diff-percent <float>`, `--max-perceptual-distance <float>`, `--sort-by {pixel|percent|perceptual}`, `--ignore-alpha`, `--fail-on-differences`
  - Chrome: `--chrome <path>`, `--chrome-dir <dir>`, `--no-chrome`, `--require-chrome-metadata`, `--allow-stale-chrome-baselines`, `--no-fastrender`, `--diff-only`
  - Build: `--no-build` (skip building `diff_renders`; also skips building `inspect_frag` when `--overlay` is set)

## `bash scripts/cargo_agent.sh xtask recapture-page-fixtures`

- Purpose: (re)capture and (re)import offline page fixtures from a manifest (defaults to `tests/pages/pageset_guardrails.json`) using `bundle_page` + `bash scripts/cargo_agent.sh xtask import-page-fixture`.
- Typical usage:
  - Recapture all fixtures from the manifest (crawl mode; fetch HTML/CSS and discover subresources without rendering): `bash scripts/cargo_agent.sh xtask recapture-page-fixtures`
  - Only recapture a subset: `bash scripts/cargo_agent.sh xtask recapture-page-fixtures --only stripe.com,dropbox.com`
  - Replace existing fixtures (dangerous): `bash scripts/cargo_agent.sh xtask recapture-page-fixtures --overwrite`
  - Cache-only capture (offline; requires a warmed disk cache): `bash scripts/cargo_agent.sh xtask recapture-page-fixtures --capture-mode cache --asset-cache-dir fetches/assets`
- Manifest fields:
  - `name` (required): fixture directory name under `tests/pages/fixtures/`
  - `url` (optional; preferred): source URL to capture (used for `render`/`crawl` modes)
  - `viewport` (optional; defaults to 1200x800) and `dpr` (optional; defaults to 1.0)
  - When `url` is absent, the command falls back to `progress/pages/<name>.json` and reads its `url` field.
- Capture modes (`--capture-mode`):
  - `crawl` (default): `bundle_page fetch --no-render ...` (fast; avoids renderer crashes/timeouts)
  - `render`: `bundle_page fetch ...` (more complete discovery when needed)
  - `cache`: `bundle_page cache <stem> ...` (offline; requires the disk-backed cache and matching request headers)
- Related: run `bash scripts/cargo_agent.sh xtask validate-page-fixtures` afterwards to ensure imports stayed fully offline.

## `bash scripts/cargo_agent.sh xtask validate-page-fixtures`

- Purpose: ensure offline page fixtures under `tests/pages/fixtures/` do not contain fetchable network URLs (`http://`, `https://`, or `//`) in HTML/CSS/SVG.
- Run: `bash scripts/cargo_agent.sh xtask validate-page-fixtures`
- Useful after:
  - importing fixtures via `bash scripts/cargo_agent.sh xtask import-page-fixture`
  - recapturing fixtures via `bash scripts/cargo_agent.sh xtask recapture-page-fixtures`
- Core flags:
  - Fixtures root: `--fixtures-root <dir>` (default `tests/pages/fixtures`)
  - Selection: `--only <csv>` (comma-separated fixture names)

## `css_coverage`

- Purpose: scan fixture HTML/CSS (and optional cached pages) for CSS property usage and classify coverage gaps (unknown and vendor-prefixed properties, plus sampled rejected values for known properties).
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin css_coverage -- --help`
- Typical usage:
  - Scan pageset fixtures: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin css_coverage`
  - Include cached HTML pages too: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin css_coverage -- --fetches-html fetches/html`
  - Emit JSON: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin css_coverage -- --json > target/css_coverage.json`
- Core flags:
  - Roots: `--fixtures <dir>` (default `tests/pages/fixtures`), optional `--fetches-html <dir>`
  - Report knobs: `--top <n>`, `--sample-values <n>`, `--json`

## `font_coverage`

- Purpose: audit which Unicode codepoints in some input text have no glyph in the selected font set.
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin font_coverage -- --help`
- Inputs:
  - Direct text: `--text "..."`
  - HTML file (extracts visible text nodes): `--html-file <path>`
- Font sources:
  - `--bundled-fonts` / `--system-fonts`
  - `--font-dir <dir>` (repeatable)
  - Default is deterministic: bundled fonts only unless you explicitly opt into other sources.

## `bundled_font_coverage`

- Purpose: scan cached pageset HTML (`fetches/html/*.html`) and report which Unicode codepoints are not covered by the bundled font set.
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundled_font_coverage -- --pageset`
- Useful for data-driven bundled font subset decisions; see [`docs/notes/bundled-fonts.md`](notes/bundled-fonts.md).
- Core flags:
  - Page selection: `--pageset`, `--pages <csv>` (URLs or cache stems)
  - Inputs: `--html-dir <dir>`, `--include-css-content`
  - Output: `--json`, `--top <n>`, `--examples <n>`

## `fetch_and_render`

- Purpose: fetch one URL (or read one `file://` target) and render to a PNG.
- Entry: `src/bin/fetch_and_render.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- --help`
- JavaScript: pass `--js` to execute author scripts using the `vm-js`-backed [`api::BrowserTab`](../src/api/browser_tab.rs),
  driven via `BrowserTab::run_until_stable` (bounded by `--js-max-frames`). See [JavaScript execution (`--js`)](#javascript-execution---js)
  for the shared time model + budget flags; for background on which public containers include JS + an
  event loop, see [`docs/runtime_stacks.md`](runtime_stacks.md).
- HTTP fetch tuning: honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
- Disk cache directory: `--cache-dir <dir>` overrides the disk-backed subresource cache location (defaults to `fetches/assets/`; only has an effect when built with `--features disk_cache`).
- Security defaults mirror the library: `file://` subresources are blocked for HTTP(S) documents. Use `--allow-file-from-http` to override during local testing, `--block-mixed-content` to forbid HTTP under HTTPS, and `--same-origin-subresources` (plus optional `--allow-subresource-origin`) to block cross-origin CSS/images/fonts when rendering untrusted pages. This flag does not block cross-origin iframe/embed document navigation.
- Performance: layout fan-out defaults to `auto` (with optional `--layout-parallel-min-fanout` / `--layout-parallel-auto-min-nodes` / `--layout-parallel-max-threads`). Use `--layout-parallel off` to force serial layout or `--layout-parallel on` to force fan-out when chasing wall-time regressions on wide pages.

Example (execute JS and render a single URL to a PNG):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- \
  --timeout 60 \
  --viewport 1200x800 \
  --js --js-max-frames 30 \
  https://example.com \
  target/example.png
```

## `bundle_page`

- Purpose: capture a page (HTML + subresources) into a self-contained bundle and replay it offline.
- Entry: `src/bin/bundle_page.rs`
- Run:
  - Fetch: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- fetch <url> --out <bundle_dir|.tar>`
    - HTTP fetch tuning: `bundle_page fetch` honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
    - For pages that crash or time out during capture, add `--no-render` (alias `--crawl`) to discover subresources by parsing HTML + CSS instead of rendering.
      - Crawl discovery includes media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`), in addition to the existing CSS/images/fonts/document discovery. Media URLs are only downloaded into the bundle when `--prefetch-media` is enabled.
      - Current limitation: `<link rel="preload" as="video|audio|track">` is not discovered as a media source yet.
    - `--prefetch-media` (alias `--include-media`): prefetch playable media sources into the bundle during crawl-based capture.
      - This is opt-in because media files can be large.
      - Budgets: `--prefetch-media-max-bytes` (per asset, default **2,000,000 bytes**) and `--prefetch-media-max-total-bytes` (total, default **10,000,000 bytes**). Set either to `0` to disable the cap.
      - When caps are enabled, `bundle_page` uses a partial fetch and **skips** media URLs that exceed the budgets (warns and leaves the URL uncached).
    - Use `--fetch-timeout-secs <secs>` to bound per-request network time when crawling large pages.
    - Note: in render capture mode (default), FastRender may not fetch media sources yet. Use `--prefetch-media` (or `--no-render/--crawl --prefetch-media`) when you need media bytes inside the bundle.
    - For JS-enabled offline replay, add `--bundle-scripts` to include `<script src>` plus related `preload`/`modulepreload` script resources (can significantly increase bundle size).
  - Cache (offline, from pageset caches): `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- cache <stem> --out <bundle_dir|.tar>`
    - Reads HTML from `fetches/html/<stem>.html` (+ `.html.meta`) and subresources from the disk-backed cache under `fetches/assets/` (override with `--asset-cache-dir` (alias `--cache-dir`); this should match the `--cache-dir` used when warming/running the pageset).
    - Add `--prefetch-media` (alias `--include-media`) to include cached media sources in the bundle (subject to the same `--prefetch-media-max-*` budgets as above).
    - Fails if a discovered subresource is missing from the cache; pass `--allow-missing` to insert empty placeholders.
    - Add `--bundle-scripts` to include scripts for JS-enabled offline replay (requires those scripts to be present in the disk cache; increases bundle size).
    - The disk cache key namespace depends on request headers. If you warmed `fetches/assets/` with non-default values (e.g. `pageset_progress --user-agent ... --accept-language ...`, or `FASTR_HTTP_BROWSER_HEADERS=0`), pass matching `bundle_page cache --user-agent ... --accept-language ...` (and the same env var) so cache capture hits the correct entries.
  - Render: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- render <bundle> --out <png>`
    - `bundle_page render` is offline and ignores `FASTR_HTTP_*` env vars (it uses the bundle contents only).
    - JavaScript (optional): add `--js` to execute author scripts via the `vm-js` `BrowserTab` harness before capturing the final pixels.
      - `bundle_page render --js` currently drives `BrowserTab::run_until_stable` with a fixed `max_frames=50` budget (there is no `--js-max-frames` flag).
      - It *does* expose the shared `JsExecutionArgs` budget override flags like `--js-max-script-bytes` and `--js-max-wall-ms` (run `bundle_page render --help`).
- Security: `--same-origin-subresources` (plus optional `--allow-subresource-origin`) applies both when capturing and replaying bundles to keep cross-origin assets out of offline artifacts. It does not block cross-origin iframe/embed document navigation.
- Convert bundles to offline fixtures for the `pages_regression` harness: `bash scripts/cargo_agent.sh xtask import-page-fixture <bundle> <fixture_name> [--output-root tests/pages/fixtures --overwrite --dry-run --include-media]`.
  - By default, media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`) are rewritten to deterministic empty `assets/missing_<hash>.<ext>` placeholder files so fixtures stay small/offline-safe.
  - Pass `--include-media` to vendor playable media, subject to size budgets (`--media-max-bytes` default **5 MiB**, `--media-max-file-bytes` default **2 MiB**; set either to `0` to disable).
  - All HTML/CSS references are rewritten to hashed files under `assets/`, and the importer fails if any network URLs would remain.
  - Media asset provenance/licensing + regeneration guidance: [`tests/pages/fixtures/assets/media/README.md`](../tests/pages/fixtures/assets/media/README.md).

Example (capture a bundle with crawl mode and prefetch media sources into the bundle):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- \
  fetch --no-render --prefetch-media https://example.com --out target/bundles/example.com.tar
```

Example (render that bundle offline with JavaScript enabled):

```bash
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bundle_page -- \
  render target/bundles/example.com.tar --out target/example.com.js.png \
  --js \
  --js-max-script-bytes $((2*1024*1024)) \
  --js-max-wall-ms 50
```

Example (import that bundle as an offline fixture and vendor playable media within the default budgets):

```bash
bash scripts/cargo_agent.sh xtask import-page-fixture target/bundles/example.com.tar example_com \
  --overwrite \
  --include-media
```

## `import_wpt`

- Purpose: import a subset of Web Platform Tests from a local WPT checkout into `tests/wpt/tests/` for the offline WPT harness.
- Entry: `src/bin/import_wpt.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin import_wpt -- --help`
- Typical usage:

  ```bash
  bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin import_wpt -- \
    --wpt-root /path/to/wpt \
    --suite css/css-text/white-space \
    --out tests/wpt/tests
  ```

- Notes:
  - `--suite` accepts one or more glob(s) relative to the WPT root (e.g. `css/css-text/*`).
  - The importer is strict/offline by default: it rewrites `web-platform.test` and root-relative URLs to file-relative paths and fails if any **fetchable** network URL remains in common contexts:
    - HTML: `src`, `srcset`, and fetchable `href` contexts like `<link href=...>` (navigation links like `<a href=...>` are ignored)
    - SVG: fetchable `href`/`xlink:href` on elements like `<image>`, `<use>`, `<feImage>` (navigation links like `<a xlink:href>` are ignored)
    - CSS: `url(...)` and `@import`
    Use `--allow-network` to opt out (not recommended).
    - Use `--strict-offline` to additionally scan the rewritten HTML/CSS for any remaining `http(s)://` or protocol-relative (`//`) URL strings anywhere in the file.
  - Sidecar `.ini` metadata files (e.g. `*.html.ini`) are copied alongside tests when present so local expectations/viewport/DPR settings are preserved.
  - Use `--dry-run` to preview, `--overwrite` to update existing files, and `--manifest <path>` / `--no-manifest` to control manifest updates.

## `inspect_frag`

- Purpose: inspect fragment output (and related style/layout state) for a single input.
- Entry: `src/bin/inspect_frag.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin inspect_frag -- --help`
- `--dump-json <dir>` writes deterministic snapshots of each pipeline stage (`dom.json`, `composed_dom.json`, `styled.json`, `box_tree.json`, `fragment_tree.json`, `display_list.json`). Pair with `--filter-selector` / `--filter-id` to focus on a subtree.
- `--dump-snapshot` prints a combined pipeline snapshot JSON to stdout (and exits).
- `--render-overlay <png>` renders the page with optional overlays for fragment bounds, box ids, stacking contexts, and scroll containers.
- Pagination/debugging: set `FASTR_FRAGMENTATION_PAGE_HEIGHT=<css px>` (and optional `FASTR_FRAGMENTATION_GAP=<css px>`) to paginate layout during inspection. The fixture at `tests/fixtures/inspect_frag_two_pages.html` forces two pages via `@page` and `break-after: page`:

  ```bash
  FASTR_FRAGMENTATION_PAGE_HEIGHT=200 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin inspect_frag -- tests/fixtures/inspect_frag_two_pages.html
  ```

  Searching for `"Second page"` should show hits on `[root 1]`.

## `diff_renders`

- Purpose: compare two render outputs (directories or PNG files) and summarize pixel + perceptual diffs.
- Entry: `src/bin/diff_renders.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin diff_renders -- --before <dir|file.png> --after <dir|file.png>`
- Matching: directory inputs are walked recursively and paired by relative path (minus the `.png` extension). This allows diffing nested render trees (for example fixture render outputs or pageset dump layouts) without flattening them first.
- Outputs: `diff_report.json` and `diff_report.html` plus diff PNGs under `<html_stem>_files/diffs/` next to the HTML report (e.g. `diff_report_files/diffs/...`).
- Tuning: `--tolerance`, `--max-diff-percent`, and `--max-perceptual-distance` accept the same values as the fixture harness (`FIXTURE_TOLERANCE`, `FIXTURE_MAX_DIFFERENT_PERCENT`, `FIXTURE_MAX_PERCEPTUAL_DISTANCE`, and `FIXTURE_FUZZY` env vars are honored when flags are omitted). Use `--ignore-alpha` (or set `FIXTURE_IGNORE_ALPHA=1`) to ignore alpha differences. Use `--sort-by perceptual` to rank diffs by perceptual distance (windowed SSIM over downsampled luminance).
- Supports deterministic sharding with `--shard <index>/<total>` to split large sets across workers.
- Exit codes: `diff_renders` exits with code 1 when any diff/missing/error entries are present. When running via
  `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run`, Cargo will print a
  `process didn't exit successfully` wrapper message—this is expected.
  Use `bash scripts/cargo_agent.sh xtask diff-renders` (or run the built binary directly) if you want cleaner
  output while still keeping the report; pass `bash scripts/cargo_agent.sh xtask diff-renders --fail-on-differences`
  to preserve the non-zero exit code while still keeping the report.

## `compare_diff_reports`

- Purpose: compare two `diff_renders` JSON reports (baseline vs new) and summarize deltas (improvements/regressions) by fixture/page name.
- Entry: `src/bin/compare_diff_reports.rs`
- Run:

  ```bash
  bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin compare_diff_reports -- \
    --baseline target/fixture_chrome_diff_before/report.json \
    --new target/fixture_chrome_diff_after/report.json \
    --json target/fixture_chrome_diff_delta/report.json \
    --html target/fixture_chrome_diff_delta/report.html
  ```

- Outputs: `diff_report_delta.json` + `diff_report_delta.html` by default.
- Safety: refuses to compare reports generated with different diff parameters (`tolerance`, `max_diff_percent`, `max_perceptual_distance`, `ignore_alpha`) or different `--shard` settings unless you pass `--allow-config-mismatch` (mismatches are recorded in the delta report).
- Optional: pass `--baseline-html <report.html>` and/or `--new-html <report.html>` when the report HTML isn't alongside the JSON (or uses a non-standard filename). This improves report links and ensures diff thumbnails resolve correctly (diff image paths in `diff_renders` are relative to the report HTML directory).
- Optional filtering: `--include <REGEX>` / `--exclude <REGEX>` can be repeated to restrict which entry names are compared (useful when iterating on a specific fixture/page). The delta report records the applied patterns plus how many entries matched.
- HTML includes:
  - links to the baseline/new input `report.json` and `report.html`
  - baseline/new render thumbnails (after + diff), and diff% cells show raw pixel counts on hover
- Gating: `--fail-on-regression` (plus `--regression-threshold-percent <PERCENT>`) exits non-zero when any entry regresses. The delta report records the gating settings so stored reports remain self-describing, and entries that fail the gate are marked in JSON (`failing_regression: true`) and highlighted in the HTML.
- Works with both deterministic fixture diffs (`bash scripts/cargo_agent.sh xtask fixture-chrome-diff`) and cached-page diffs (`scripts/chrome_vs_fastrender.sh`), as long as you have two `report.json` files to compare.

## `diff_snapshots`

- Purpose: compare pipeline snapshots (`*.snapshot.json`) and highlight stage-level deltas that explain pixel diffs.
- Entry: `src/bin/diff_snapshots.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin diff_snapshots -- --before <dir|file> --after <dir|file>`
- Matching:
  - Directory inputs support both the render-pages layout (`<stem>.snapshot.json`) and directory-based snapshots (`<stem>/snapshot.json` produced by `pageset_progress --dump-*` and `render_fixtures --write-snapshot` / `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --write-snapshot`).
  - Entries are paired by stem (the `<stem>` part of the filename/directory). For directory snapshots, `render.png` is linked when present; otherwise the tool looks for `<stem>.png` next to the `<stem>/` directory (fixture outputs) and finally `<stem>/<stem>.png` (legacy).
- Outputs: `diff_snapshots.json` and `diff_snapshots.html` summarizing schema versions, per-stage counts, DOM/box/fragment/display list changes, and links to sibling `*.png` renders when present.

## `dump_a11y`

- Purpose: emit the computed accessibility tree for a document as JSON (without painting).
- Entry: `src/bin/dump_a11y.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin dump_a11y -- --help`
- Note: `dump_a11y` does not execute JavaScript (`--js` is not supported). It reflects the
  accessibility semantics of the input HTML/CSS as loaded; for JS-driven DOM changes, embed
  `api::BrowserTab` in a custom harness (see [`docs/runtime_stacks.md`](runtime_stacks.md)).
- Workflow details (a11y tree inspection + bounds mapping + screen reader testing): [page_accessibility.md](page_accessibility.md)

## `dump_accesskit`

- Purpose: emit the AccessKit tree update produced by the **egui-based browser chrome** (OS-facing
  accessibility).
- Entry: `src/bin/dump_accesskit.rs`
- Run: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --features browser_ui --bin dump_accesskit -- --help`
- Notes:
  - On Linux, building with `--features browser_ui` requires system GUI development headers (X11 / Wayland,
    EGL/Vulkan, etc). Real-time audio output via `--features audio_cpal` additionally requires ALSA headers
    (`libasound2-dev`). See [`docs/browser_ui.md#platform-prerequisites`](browser_ui.md#platform-prerequisites).
  - See [`docs/chrome_accessibility.md`](chrome_accessibility.md) for recommended `dump_accesskit`
    invocations (`--named-only`, `--show-menu-bar`, `--focus-address-bar`) and how to interpret the
    output.
  - `dump_accesskit` is a headless egui snapshot tool; it does **not** run the browser worker and
    therefore does not include any worker-produced page accessibility snapshot / injected page subtree.

## Offline / cached captures

- Use `bundle_page fetch` to save a single reproducible capture (HTML bytes, content-type + final URL, fetched subresources with HTTP metadata (CSS/images/fonts, plus crawl-prefetched assets like media when `--prefetch-media` is enabled), and a manifest mapping original URLs to bundle paths). Bundles can be directories or `.tar` archives and are deterministic.
- Use `bundle_page cache <stem> --out <bundle>` to convert an already-warmed pageset cache entry (cached HTML + disk-backed assets) into a portable bundle **without network access**.
- Replay with `bundle_page render <bundle> --out out.png` to render strictly from the bundle with zero network calls.
- For larger batch workflows, offline captures are also available via the existing on-disk caches:
  - `fetch_pages` writes HTML under `fetches/html/` and a `*.html.meta` sidecar with response metadata (content-type, final URL, status code, `Referrer-Policy`, and `Content-Security-Policy`).
    - `Content-Security-Policy` header values are stored as one `content-security-policy:` line per header value (multiple lines may appear) and restored when replaying cached HTML so CSP enforcement is deterministic offline.
  - `render_pages` and `fetch_and_render` use the shared disk-backed fetcher (when built with `--features disk_cache`; enabled by default in `scripts/pageset.sh`, `bash scripts/cargo_agent.sh xtask pageset`, and the profiling scripts) for subresources, writing into `fetches/assets/` (override with `--cache-dir <dir>`). After one online render, you can re-run against the same caches without network access (new URLs will still fail). Use `--no-disk-cache`, `DISK_CACHE=0`, or `NO_DISK_CACHE=1` to opt out.
  - Fresh HTTP caching headers are honored by default for disk-backed fetches; add `--no-http-freshness` to `fetch_and_render`, `render_pages`, or `pageset_progress` to force revalidation even when Cache-Control/Expires mark entries as fresh.
  - Disk-backed cache tuning (applies to `prefetch_assets`, `pageset_progress`, `render_pages`, and `fetch_and_render` when built with `disk_cache`):
    - `--disk-cache-max-age-secs <secs>` (or `FASTR_DISK_CACHE_MAX_AGE_SECS=<secs>`) caps how long cached subresources are trusted before forcing a refetch. Use `0` to disable age-based expiry (never age out).
    - `--disk-cache-max-bytes <bytes>` (or `FASTR_DISK_CACHE_MAX_BYTES=<bytes>`) sets the eviction budget for on-disk cached bytes. Use `0` to disable eviction.
    - `--disk-cache-lock-stale-secs <secs>` (or `FASTR_DISK_CACHE_LOCK_STALE_SECS=<secs>`) controls how quickly stale `.lock` files are removed when workers are hard-killed mid-write (default 8 seconds).
    - Defaults are `512MB`, `7d`, and `8s`. Example: `pageset_progress run --disk-cache-max-age-secs 0` keeps cached subresources pinned to avoid surprise refetches during short timeout runs.
- Asset fetches in library code go through [`fastrender::resource::CachingFetcher`] in-memory by default, or [`fastrender::resource::DiskCachingFetcher`] behind the optional `disk_cache` feature.

## Diagnostics

- `render_pages` emits per-page logs in `<out-dir>/<page>.log` plus a summary in `<out-dir>/_summary.log` (defaults to `fetches/renders/`). CSS fetch failures show up there and correspond to `ResourceKind::Stylesheet` entries in the library diagnostics model.
- The library API exposes structured diagnostics via `render_url_with_options` returning `RenderResult { pixmap, accessibility, diagnostics }`. Set `RenderOptions::allow_partial(true)` to receive a placeholder image and a `document_error` string when the root fetch fails; subresource fetch errors are collected in `diagnostics.fetch_errors` with status codes, final URLs, and any cache validators observed.
- `render_pages` can emit structured reports via `--diagnostics-json` (per-page) plus `--dump-intermediate` for summaries or full intermediate dumps.
- The shipped binaries print a concise diagnostics summary (including status/final URL). Pass `--verbose` to `fetch_and_render` or `render_pages` to include full error chains when something fails.

## `pageset_progress`

- Purpose: **update the committed pageset scoreboard** under `progress/pages/*.json`.
- Entry: `src/bin/pageset_progress.rs`
- Run:
  - Help: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- run --help`
  - Typical: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- run --timeout 5`
  - JS-enabled (execute author scripts; see [JavaScript execution (`--js`)](#javascript-execution---js) for budgets/time model):

    ```bash
    bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- run \
      --timeout 5 \
      --pages stripe.com \
      --js --js-max-frames 10
    ```
  - HTTP fetch tuning: honors the `FASTR_HTTP_*` env vars described above (see [`docs/env-vars.md#http-fetch-tuning`](env-vars.md#http-fetch-tuning)).
  - Compatibility (opt-in only): `--compat-profile site` enables site-specific hacks and
    `--dom-compat compat` applies generic DOM compatibility mutations (see
    [`docs/notes/dom-compatibility.md`](notes/dom-compatibility.md)). Defaults stay spec-only;
    `bash scripts/cargo_agent.sh xtask pageset` forwards the flags only when you provide them.
- JavaScript: `pageset_progress run --js` executes author scripts using the `vm-js` `BrowserTab` harness, driven via `BrowserTab::run_until_stable` (bounded by `--js-max-frames`).
- Disk cache directory: `--cache-dir <dir>` overrides the disk-backed subresource cache location (defaults to `fetches/assets/`; only has an effect when built with `--features disk_cache`).
- Fonts: pass `--bundled-fonts` to skip system font discovery (pageset wrappers default to bundled fonts for deterministic timing; use `--system-fonts` on the wrappers when comparing `--accuracy` diffs against Chrome) or
  `--font-dir <path>` to load fonts from a specific directory without hitting host fonts.
- Accuracy (optional): pass `--accuracy --baseline=chrome` to compute pixel-diff metrics against the baseline PNGs and store the result under `progress.accuracy`.
  - Use `--accuracy-require-clean` to skip pages that are `status=ok` but still have known subresource failures (`failure_stage` set, fetch errors, bot mitigation blocks). Skipped pages keep `accuracy` unset and get an `auto_notes` line like `Accuracy skipped: ok-with-failures`.
- Sync: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- sync [--prune] [--html-dir fetches/html --progress-dir progress/pages]` bootstraps one JSON per pageset URL without needing any caches. Existing progress artifacts are preserved when `fetches/html/` is missing (only newly created entries are marked `auto_notes: "missing cache"`). `--prune` removes stale progress files for URLs no longer in the list. Stems are collision-aware (`example.com--deadbeef` when needed) to keep cache and progress filenames unique.
- Migrate: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- migrate [--html-dir fetches/html --progress-dir progress/pages]` rewrites existing progress JSON without fetching or rendering. It applies legacy schema migrations (notably splitting mixed legacy `notes` into durable `notes` + machine `auto_notes`) and reserializes deterministically using the runner's canonical formatter.
- Progress filenames use the cache stem from `pageset_stem` (strip scheme + leading `www.` plus a deterministic hash suffix on collisions); `--pages` filters accept the URL, canonical stem, or cache stem. If you have older `fetches/html` entries with `www.` prefixes in the filename, re-run `fetch_pages` so progress filenames line up.
  - For temporary/test runs, `FASTR_PAGESET_URLS="https://a.com,https://b.com"` overrides the built-in pageset everywhere.
- Triage reruns (reuse existing `progress/pages/*.json` instead of typing stems):
  - `--from-progress <dir>` enables selection from saved progress files (default intersection of filters, use `--union` to OR them).
  - Filters: `--only-failures`, `--only-status timeout,panic,error`, `--slow-ms <ms> [--slow-ok-only]`, `--hotspot css|cascade|box_tree|layout|paint|...`, `--top-slowest <n>`.
  - The deterministic stem list is printed before running; if nothing matches, the command exits cleanly without touching caches.
- Report: `bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report`
  `[--progress-dir progress/pages --top 10 --fail-on-bad --compare <other> --fail-on-regression --regression-threshold-percent 10 --fail-on-slow-ok-ms <ms> --fail-on-stage-sum-exceeds-total]`
  - Prints status counts, slowest pages, and hotspot histograms for the saved progress files.
  - With `--compare`, also prints status transitions plus the top regressions/improvements by `total_ms`.
  - `--fail-on-regression` exits non-zero for ok→bad or > threshold slowdowns.
  - Accuracy rankings: use `--rank-accuracy` (plus optional filters `--only-diff` / `--min-diff-percent`) to list the worst `status=ok` pages by `accuracy.diff_percent`. Add `--accuracy-only-clean` to restrict the listing to pages with no known subresource failures.
  - Accuracy regression gate: with `--compare`, `--fail-on-accuracy-regression` exits non-zero when a page's stored `accuracy.diff_percent` increases vs the baseline. This gate behaves as if `--accuracy-only-clean` is set unless you pass `--allow-ok-with-failures`.
  - `--fail-on-slow-ok-ms 5000` enforces the hard 5s/page budget for ok pages (entries missing `total_ms` are ignored by this gate).
  - `--include-trace` lists saved Chrome traces (from `target/pageset/traces/` + `target/pageset/trace-progress/`).
  - `--verbose-stats` prints structured per-page stats when present (including resource cache hit/miss/bytes breakdowns, single-flight inflight wait time, disk cache lock waits, and network fetch totals). It also prints an aggregated "Resource totals" summary plus top-N rankings for network/inflight/disk cache time (including disk lock wait time), and top-N rankings per stage bucket (fetch/css/cascade/box_tree/layout/paint).
  - Stage-bucket sanity guardrail (off by default): `--fail-on-stage-sum-exceeds-total` checks `status=ok` entries that have both `total_ms` and non-zero stage buckets, failing when `stages_ms.sum()` exceeds `total_ms` by more than `--stage-sum-tolerance-percent` (default 10%). This is intended as a regression guardrail for catching stage timing accounting bugs (double-counting or accidentally mixing CPU-summed metrics into wall-clock stage buckets).
  - Example:

    ```bash
    bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin pageset_progress -- report \
      --progress-dir progress/pages \
      --fail-on-stage-sum-exceeds-total
    ```
- Accuracy (optional; best-effort / non-deterministic because cached pages still load live subresources):
  - `pageset_progress run --accuracy --baseline=chrome` computes pixel-diff metrics against headless Chrome screenshots of the cached HTML (stored in each page's `progress/pages/<stem>.json`).
  - Baseline artifacts live under `fetches/chrome_renders/` by default (override with `--baseline-dir <dir>`):
    - `<stem>.png` — screenshot
    - `<stem>.json` — metadata sidecar used to detect stale baselines (records viewport/DPR/JS/headless mode plus `html_sha256` of `fetches/html/<stem>.html` before patching)
  - Baselines are auto-generated when missing or stale (cached HTML hash or viewport/DPR/JS mismatch) via `scripts/chrome_baseline.sh`.
  - Refresh controls (require `--accuracy --baseline=chrome`):
    - `--baseline-refresh` regenerates baselines for all selected pages (overwrites existing PNG + JSON).
    - `--baseline-refresh-if-unverified` regenerates only baselines that have a PNG but no metadata sidecar (useful for migrating older baseline directories; otherwise they are compared with a warning).
- Safety: uses **panic containment** (per-page worker process) and a **hard timeout** (kills runaway workers) so one broken page cannot stall the whole run.
- Worker Rayon threads: `pageset_progress run` spawns up to `--jobs` worker processes in parallel and sets `RAYON_NUM_THREADS` for each worker to `available_parallelism()/jobs` (min 1, additionally clamped by a detected cgroup CPU quota on Linux) unless the parent environment already defines it.
- Outputs:
  - `progress/pages/<stem>.json` — small, committed per-page progress artifact
  - `target/pageset/logs/<stem>.log` — per-page log (not committed)
  - `target/pageset/logs/<stem>.stderr.log` — worker stdout/stderr, including panic
    backtraces and a note if the parent kills the process on timeout (not committed)
  - Optional cascade profiling reruns: `--cascade-diagnostics` re-runs slow cascade pages and
    cascade timeouts with cascade profiling enabled (`FASTR_CASCADE_PROFILE=1`), then merges the
    resulting selector candidate/match counters into the committed progress JSON under
    `diagnostics.stats.cascade`.
    - Slow threshold: `--cascade-diagnostics-slow-ms <ms>` (defaults to 500ms).
    - Temp rerun progress dir (not committed): `--cascade-diagnostics-progress-dir <dir>` (defaults
      to `target/pageset/cascade-progress/`).
  - Optional traces: `--trace-failures` / `--trace-slow-ms <ms>` rerun targeted pages with Chrome tracing enabled; tune trace rerun budgets with `--trace-timeout` (defaults to `timeout * 2`), `--trace-soft-timeout-ms`, and `--trace-jobs` (defaults to 1 to avoid contention). Traces land in `target/pageset/traces/<stem>.json` with rerun progress under `target/pageset/trace-progress/<stem>.json` and logs at `target/pageset/logs/<stem>.trace.log`.
  - Workers accept `--layout-parallel {off|on|auto}` (plus `--layout-parallel-min-fanout` / `--layout-parallel-auto-min-nodes` / `--layout-parallel-max-threads`). The default is `auto`, so small pages stay serial while large pages can fan out across Rayon threads.

### Accuracy baselines (`pageset_progress run --accuracy`)

When `--accuracy` is enabled, ok pages record pixel-diff telemetry against baseline PNGs (stored in `progress/pages/*.json` under the `accuracy` field).

- Provide baseline PNGs via `--baseline-dir <dir>` (expected layout: `<dir>/<stem>.png`).
- Or use `--baseline=chrome` to auto-generate missing/stale baselines via `scripts/chrome_baseline.sh` into the resolved baseline directory (defaults to `fetches/chrome_renders/`).
- `pageset_progress` forwards the current run’s `--user-agent` and `--accept-language` to Chrome baseline generation so the Chrome screenshot uses the same request header knobs as FastRender. This reduces misleading accuracy metrics when live subresources vary by UA/locale.
