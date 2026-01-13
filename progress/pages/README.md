# `progress/pages/*.json`

This directory contains the **committed pageset scoreboard**: one tiny JSON file per cached page stem.

- Bootstrap with `pageset_progress sync` (`bash scripts/cargo_agent.sh run --release --bin pageset_progress -- sync [--prune]`) to materialize one placeholder per official pageset URL, even on a fresh checkout with no caches.
- `sync` writes minimal `status: error` entries with `auto_notes: "not run"` or `auto_notes: "missing cache"` for newly created pages; it does not downgrade existing committed progress artifacts just because the local checkout is missing cached HTML. `--prune` removes files for URLs no longer in the pageset list.
- `pageset_progress run` updates these files after caches exist.
- `pageset_progress migrate` rewrites existing progress files without rendering, applying legacy schema migrations (e.g. splitting mixed legacy `notes` into durable `notes` + machine `auto_notes`), backfilling missing `inputs` fingerprints when cached HTML exists, and reserializing deterministically.
- Non-placeholder progress files may include an `inputs` section describing the cached HTML used for the run:
  - `html_sha256`: SHA-256 hex digest of the cached HTML bytes read from `fetches/html/<stem>.html`.
  - `html_bytes`: cached HTML byte length.
  - `cached_status`: optional HTTP status code parsed from `fetches/html/<stem>.html.meta` (when present).
- Each `<stem>.json` should match the cached HTML filename stem under `fetches/html/<stem>.html` (same normalization as `fetch_pages`).
- Keep these files small and stable (no raw HTML, no machine-local paths, no traces).
- When diagnostics are enabled, successful renders may include `diagnostics.stats` (structured `RenderStats` timing/count summaries) to power `pageset_progress report --verbose-stats`. No giant blobs or logs.
  - `diagnostics.stats.resources` may include resource/cache breakdowns used for pageset triage:
    - `fetch_counts` (by resource kind)
    - `image_cache_hits` / `image_cache_misses`
    - `resource_cache_fresh_hits` / `resource_cache_stale_hits` / `resource_cache_revalidated_hits` / `resource_cache_misses` / `resource_cache_bytes`
    - `disk_cache_hits` / `disk_cache_misses` / `disk_cache_bytes` / `disk_cache_ms` / `disk_cache_lock_waits` / `disk_cache_lock_wait_ms` (disk-backed subresource cache reads)
    - `fetch_inflight_waits` / `fetch_inflight_wait_ms` (single-flight de-dup wait time)
    - `network_fetches` / `network_fetch_bytes` / `network_fetch_ms` (HTTP fetches performed by the underlying fetcher)
- These are auto-generated; don't hand-edit them except for durable human fields like `notes`/`last_*` when needed.
- `notes` is intended for durable human explanations; `auto_notes` is machine-generated last-run diagnostics and is rewritten on each run.
- Successful pages can still report non-fatal problems under `auto_notes` (for example: `ok with failures: ...` when a render completes but records `failure_stage=<...>` and/or subresource `fetch_errors`).
- `pageset_progress run` caching knobs:
  - `--disk-cache-stale-policy <revalidate|use-stale-when-deadline>` controls whether stale cache entries are served immediately under render deadlines (default: `use-stale-when-deadline`).
  - `--offline` disables network fetches and serves subresources from disk cache only (requires building with `--features disk_cache`). Cache misses show up as fetch errors and the page is marked `status=error`.
- When `pageset_progress run --accuracy --baseline-dir <dir>` is used, successful pages may include an `accuracy` section with visual diff telemetry against baseline PNGs (typically Chrome screenshots in `fetches/chrome_renders`).
  - `baseline`: baseline renderer label (currently `chrome`).
  - `diff_pixels`: number of pixels that differ (after applying `tolerance`).
  - `diff_percent`: percent of pixels that differ (0-100, after applying `tolerance`).
  - `perceptual`: perceptual distance derived from a **windowed SSIM** computed over **downsampled luminance (Y)** (0.0 = identical, 1.0 = maximally different).
  - `perceptual_metric`: optional id for the perceptual metric implementation that produced `perceptual` (omitted on older artifacts).
  - `tolerance`: per-channel tolerance used for pixel comparisons (0-255).
  - `max_diff_percent`: threshold used to classify diffs as acceptable/unacceptable (0-100).
  - `computed_at_commit`: git SHA captured when the metrics were computed (omitted when unknown).
  - Triage note:
    - `diff_percent` is a **raw pixel mismatch** rate. On real pages it can be dominated by tiny differences (subpixel anti-aliasing, font rasterization, rounding, or 1–2 LSB per-channel noise) and therefore read as “high” even when the render looks visually correct.
    - When diffing Chrome baseline screenshots (which use **system fonts**) against deterministic fixture renders (which typically use **bundled fonts**), pages that rely on **generic font families** like `sans-serif`/`serif` can accumulate large text diffs even if layout and paint logic are otherwise correct.
    - `perceptual` is usually a better indicator of **visually meaningful** differences; prefer it when prioritizing which pages are actually “broken”. However, SSIM can still be sensitive on extremely text-dense pages: small per-glyph rasterization differences (subpixel AA, hinting) can raise `perceptual` even when layout/colors are correct.
- Migration note (perceptual metric evolution):
  - `accuracy.perceptual` values computed on different commits may not be directly comparable, since the underlying perceptual metric implementation can change (for example: global SSIM → windowed SSIM over downsampled luminance).
  - `accuracy.computed_at_commit` records the git SHA of the run so you can tell which implementation produced the stored number.
  - Newer artifacts may also include `accuracy.perceptual_metric`, which records the metric id directly.
  - To avoid massive churn in `progress/pages/*.json`, prefer re-running `bash scripts/cargo_agent.sh xtask refresh-progress-accuracy` **only for the pages you care about** (and use sharded refresh for parallelism), rather than doing a repo-wide refresh immediately after each metric tweak.
- To seed initial `accuracy` values for pages that have offline fixtures under `tests/pages/fixtures/<stem>/index.html`, diff those fixtures against Chrome and sync the metrics into `progress/pages/*.json`:
  - Recommended starter set: `tests/pages/pageset_guardrails.json` (curated high-signal pages).
  - Commands:
    - `bash scripts/cargo_agent.sh xtask refresh-progress-accuracy --fixtures <stem1,stem2,...>`
    - Sharded refresh (run shards 0..7 in parallel as needed):
      - `bash scripts/cargo_agent.sh xtask refresh-progress-accuracy --from-progress progress/pages --only-failures --top-worst-accuracy 1000000 --min-diff-percent 0 --shard 0/8 --keep-going --out-dir target/refresh_progress_accuracy_0_8`
    - (manual) `bash scripts/cargo_agent.sh xtask fixture-chrome-diff --fixtures <stem1,stem2,...>` (defaults: viewport `1040x1240`, `--js off`, `tolerance=0`, `max-diff-percent=0`)
    - (manual) `bash scripts/cargo_agent.sh xtask sync-progress-accuracy --report target/fixture_chrome_diff/report.json --progress-dir progress/pages`
  - Note: baselines depend on the installed Chrome/Chromium build, so reruns can cause churn when Chrome versions differ.
  - Viewport defaults differ between harnesses:
    - `pageset_progress run` (cached pageset renders + `fetches/chrome_renders` baselines) defaults to `1200x800`.
    - Regression/fixture harnesses (`xtask page-loop`, `xtask fixture-chrome-diff`) default to `1040x1240`.
    - Use the viewport that matches the baseline you are comparing against; override with `--viewport WxH` when needed.
- Renderer-provided `failure_stage`/`timeout_stage` fields stay `null` on placeholders and are populated directly from diagnostics during runs for programmatic triage.
- `stages_ms` buckets are a coarse **wall-time** attribution (mutually exclusive buckets; `fetch`,
  `css`, `cascade`, `box_tree`, `layout`, `paint`) derived
  from the worker stage heartbeat timeline (`*.stage.timeline`) when available and rescaled so the
  buckets sum (within rounding error) to `total_ms`.
- If the timeline is missing/unreadable (or for legacy artifacts), stage buckets may fall back to
  `diagnostics.stats.timings` wall-clock stage timers (`*_ms`).
- CPU-sum subsystem counters (`*_cpu_ms`, e.g. `timings.text_shape_cpu_ms`,
  `layout.taffy_flex_compute_cpu_ms`) may exceed wall time and are intentionally excluded from
  `stages_ms` to avoid double-counting.
- Use `pageset_progress migrate` to refresh older artifacts (including rewriting legacy
  `text_*_ms` / `taffy_*_compute_ms` keys).
- `diagnostics.stats.cascade` can include selector-level counters (rule candidates/matches, bloom
  fast rejects, time splits, and `:has()` counters) when cascade profiling is enabled:
  - `FASTR_CASCADE_PROFILE=1` for ad-hoc renders, or
  - `pageset_progress run --cascade-diagnostics` to re-run slow cascade pages/timeouts and merge
    the resulting cascade counters into these committed progress artifacts.

`pageset_progress report --verbose-stats` prints these per-page resource breakdowns plus aggregated
totals and top-N rankings (network/inflight/disk) to speed up performance triage.

See `AGENTS.md` for the intent and schema.
