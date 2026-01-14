# Performance logging

Helpful environment variables for profiling layout/cascade on large pages. The canonical list is in [env-vars.md](env-vars.md); this page highlights the ones that tend to be most actionable.

When using `render_pages`/`fetch_and_render`, per-page logs are written to `fetches/renders/<page>.log` and a summary to `fetches/renders/_summary.log`; review these alongside the flags below when investigating slow or blank renders.

## Common profiling flags (renderer pipeline)

- `FASTR_CASCADE_PROFILE=1`
  - Enables cascade profiling. Logs node count, candidate/match counts, and timing breakdown for selector matching, declaration application, and pseudo computation at the end of `apply_styles`.

- `FASTR_TRACE_OUT=/tmp/trace.json`
  - Writes a Chrome trace of the render pipeline (fetch/decode/parse/style/layout/paint). Open it in `chrome://tracing` or Perfetto to inspect spans.
  - `fetch_and_render` also supports `--trace-out trace.json`, and library consumers can set `RenderOptions::with_trace_output`.
  - Use `FASTR_TRACE_MAX_EVENTS=<N>` to cap the number of events retained per trace (default 200000). This applies to both `FASTR_TRACE_OUT` and `FASTR_BROWSER_TRACE_OUT`.

- `FASTR_BROWSER_TRACE_OUT=/tmp/browser-trace.json` (legacy alias: `FASTR_PERF_TRACE_OUT`)
  - Writes a Chrome trace of the windowed `browser` UI event loop (winit event handling, egui frame build, worker message drain, frame uploads, wgpu present). The trace is written when the browser process exits.
  - Open it in Perfetto by visiting <https://ui.perfetto.dev> and using "Open trace file", or in Chromium via `chrome://tracing`.

- Container query second-pass logging (used in `render_pages`/`fetch_and_render`):
  - `FASTR_LOG_CONTAINER_PASS=1` prints the number of query containers (size vs inline-size) and a few samples of their dimensions/names when the second cascade/layout runs.
  - `FASTR_LOG_CONTAINER_REUSE=1` reports how many styled nodes are reused during the container-query recascade.
  - `FASTR_LOG_CONTAINER_DIFF=<n>` samples up to `n` styled ids whose fingerprints changed between passes (tag/#id/.class + fingerprint).
  - `FASTR_LOG_CONTAINER_IDS=<id,id,...>` dumps styled summaries for specific ids during the second pass to trace why they differ.
  - `FASTR_LOG_CONTAINER_FIELDS=1` lists which layout-affecting fields changed for the sampled diff entries.
  - `FASTR_LOG_CONTAINER_QUERY=1` logs container sizes while building the container-query context (useful when debugging “why did this query match?”).

These env vars are read in the rendering binaries (`render_pages`, `fetch_and_render`), the windowed
`browser` UI, and the cascade internals; leave them unset for normal runs.

## Browser responsiveness

FastRender’s windowed `browser` UI has separate instrumentation for **responsiveness** (UI frame
times, scroll/resize smoothness, and input latency). This is distinct from the render-pipeline
profiling above (which focuses on parse/style/layout/paint timings during page renders).

See the workstream goals/metric definitions in
[`instructions/browser_responsiveness.md`](../instructions/browser_responsiveness.md).

Quick start (HUD + JSONL perf log enabled):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release --hud --perf-log about:test-layout-stress
```

### Windowed JSONL perf logging (`browser --perf-log` / `FASTR_PERF_LOG=1`)

The windowed `browser` UI can emit a lightweight **JSON Lines** perf log (one JSON object per line)
describing UI responsiveness events.

Enable it via either:

- CLI (preferred):
  - `browser --perf-log` emits JSONL events to **stdout**.
  - `browser --perf-log-out <path>` writes JSONL events to a file (creates parent directories).
- Environment variables (legacy / wrapper-friendly):
  - `FASTR_PERF_LOG=1` enables perf logging.
  - `FASTR_PERF_LOG_OUT=/path/to/log.jsonl` redirects output to a file instead of stdout.

For interactive captures, prefer the convenience wrapper (runs under the repo guardrails, passes
`browser --perf-log`, and tees the stdout JSONL stream to a file):

```bash
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl

# Capture + summarize (runs `browser_perf_log_summary` after the browser exits):
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl --summary
```

Typical run (CLI; writes a JSONL log you can post-process with `jq`, pandas, etc.):

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
  --perf-log-out target/browser_perf.jsonl about:test-layout-stress
```

Legacy env-var equivalent:

```bash
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- about:test-layout-stress
```

When enabled, you should expect events covering at least:

- **TTFP** (“time to first paint”): navigation start → first presented frame for that tab.
- **Worker stage heartbeats**: `event=stage` is emitted when the windowed UI processes
  `WorkerToUi::Stage` messages. These include `tab_id`, `stage` (e.g. `layout`, `paint_build`), and
  `hotspot` (coarse bucket such as `fetch`, `script`, `css`, `cascade`, `box_tree`, `layout`,
  `paint`, `unknown`).
- **Frame time samples** during scroll and resize (used to spot jank and dropped frames).
- **Input latency** samples (input arrival → visible UI response).
- **Frame upload/coalescing** samples: `event=frame_upload` reports wgpu upload timing and
  `FrameUploadCoalescer` counters (push/overwrite/drain/pending) to help diagnose scroll/resize
  jank caused by dropped frames or expensive texture uploads.
- **Memory summary** samples (`event=memory_summary`) with `rss_bytes` and `rss_mb` to spot RSS growth
  during workloads (Linux-only; fields are nullable elsewhere).

The exact schema evolves, but each JSON line is intended to be self-describing. Common fields
include:

- `schema_version` (integer) — currently `2` (omitted on some legacy/diagnostic events).
- `event` (string) — event kind (current: `frame`, `input`, `resize`, `navigation`, `ttfp`, `stage`; plus
  periodic diagnostics like `idle_sample` (legacy alias: `idle_summary`) / `worker_wake_summary` / `cpu_summary` /
  `memory_summary`).
- `t_ms` (integer) — monotonic timestamp in milliseconds since process start (some events use `ts_ms`).
- `window_id` (string) — identifier for the window instance (or `"process"` for process-wide
  summaries).
- Event-specific numeric fields such as `ui_frame_ms`, `input_to_present_ms`, `resize_to_present_ms`,
  `ttfp_ms`, etc.

### Summarizing a capture: `browser_perf_log_summary`

To turn a captured JSONL log into p50/p95/max headline numbers, run:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- \
  --input target/browser_perf.jsonl >/dev/null
```

(`>/dev/null` suppresses the tool’s JSON output; the human-readable summary is printed to stderr.)

Filtering options (see `browser_perf_log_summary --help`):

- `--from-ms <ms>` / `--to-ms <ms>`: limit to a timestamp window.
- `--only-event frame|input|resize|ttfp|idle_sample|cpu_summary`: summarize a single event type.
  (`idle_summary` is accepted as a legacy alias.)

Note: the perf log contains additional event types (e.g. `frame_upload`, `memory_summary`,
`worker_wake_summary`) that are currently not summarized by `browser_perf_log_summary`; use
`jq`/pandas for custom analysis.

### Headless benchmark harness: `ui_perf_smoke`

For automated/regression-friendly measurements, use the headless harness `ui_perf_smoke`. It runs a
small scripted set of UI scenarios and writes a single JSON summary.

By default, `ui_perf_smoke` runs in a deterministic **offline** mode: `http://` and `https://`
subresource fetches are disabled via `ResourcePolicy` (while `file://` and `data:` remain allowed
for local fixtures). To opt into real network benchmarking locally, pass `--allow-network` (alias:
`--http`).

On Linux, each per-scenario summary also includes RSS snapshots to help catch memory growth:
`rss_bytes_start`, `rss_bytes_end`, and `rss_bytes_peak` (nullable elsewhere).

Via `xtask` (recommended):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
```

Or run the binary directly:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin ui_perf_smoke -- \
  --output target/ui_perf_smoke.json
```

The `xtask` wrapper mirrors the `perf-smoke` workflow: it runs the harness in `--release` mode,
enables bundled fonts (`FASTR_USE_BUNDLED_FONTS=1`) for determinism, and forwards common regression
gating flags (`--output`, `--baseline`, `--threshold`, `--fail-on-regression`).

For deterministic CI-friendly timings, `ui_perf_smoke` defaults to a single Rayon thread when
neither `--rayon-threads` nor `RAYON_NUM_THREADS` are set. Override with `--rayon-threads <N>` (or
`RAYON_NUM_THREADS=<N>`) when you explicitly want more parallelism. The JSON summary records the
resolved value as `run_config.effective_rayon_threads` and the source as
`run_config.rayon_threads_source` (`cli`, `env`, or `harness_default`).

Examples:

```bash
# Run a single scenario (see `ui_perf_smoke --help` for the full list).
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke -- --only ttfp_newtab

# Compare against a saved baseline and fail on regressions.
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke \
  --baseline baseline/ui_perf_smoke.json --threshold 0.05 --fail-on-regression \
  -- --only ttfp_newtab
```

Scenario fixtures (current default):

- `scroll_fixture` / `resize_fixture` run against the built-in offline page
  [`about:test-layout-stress`](../src/ui/about_pages.rs) so scroll/resize latency includes
  meaningful width-sensitive layout/reflow work.
- `input_text` runs on the file fixture `tests/pages/fixtures/ui_perf_smoke/index.html` (keeps an
  `<input>` at a stable coordinate for deterministic pointer+text input).

How to map the harness output back to the metrics table in
`instructions/browser_responsiveness.md`:

- **TTFP**: scenario `ttfp_newtab` reports `ttfp_p50_ms` / `ttfp_p95_ms` / `ttfp_max_ms`
  (and `ttfp_ms` as a convenience alias for p50).
- **Scroll responsiveness**: scenario `scroll_fixture` reports `scroll_latency_p50_ms` / `scroll_latency_p95_ms`
  / `scroll_latency_max_ms` (ScrollTo → next frame).
- **Resize responsiveness**: scenario `resize_fixture` reports `resize_latency_p50_ms` / `resize_latency_p95_ms`
  / `resize_latency_max_ms` (ViewportChanged → next frame).
- **Input latency**: scenario `input_text` reports `input_latency_p50_ms` / `input_latency_p95_ms`
  / `input_latency_max_ms` (TextInput/Backspace → next frame).

### Traces (Perfetto)

If you need a timeline view (what happened *during* a janky frame), enable trace output and open it
in [Perfetto UI](https://ui.perfetto.dev):

- Renderer pipeline trace (parse/style/layout/paint): `FASTR_TRACE_OUT=/tmp/trace.json` (captures the
  most recent render).
- Browser UI trace: `FASTR_BROWSER_TRACE_OUT=/tmp/ui_trace.json` (legacy alias: `FASTR_PERF_TRACE_OUT`).

## Other useful profiling flags

- `FASTR_RENDER_TIMINGS=1` — prints high-level timing for parse/cascade/box_tree/layout/paint per page in the render binaries.
- `FASTR_LOG_INTERACTION_INVALIDATION=1` — log which invalidation path `BrowserDocument` chose for each render (paint-only vs restyle vs relayout). Useful when validating hover/focus performance in renderer-chrome dogfooding.
- `FASTR_LAYOUT_PROFILE=1` — enables layout-context profiling (block/inline/flex/grid/table/absolute) with call counts and inclusive times.
- `FASTR_GRID_MEASURE_CACHE_PROFILE=1` — enables grid item measurement cache counters (thread-local/shared hits + misses, including style-override key breakdown). Pair with `FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES=1` to experiment with sharing override-key measurements across rayon threads.
- `FASTR_FLEX_PROFILE=1` — flex-specific profiling (measure/compute/convert stats, cache hits). Optional helpers:
  - `FASTR_FLEX_PROFILE_NODES=1`
  - `FASTR_FLEX_PROFILE_NODE_KEYS=1`
- `FASTR_FLEX_PROFILE_HIST=1`
- `FASTR_INTRINSIC_STATS=1` — reports intrinsic sizing cache hits/misses/lookups after layout.
- `FASTR_LAYOUT_CACHE_STATS=1` — reports layout cache stats (intrinsic cache hits/misses, layout pass count).
- `FASTR_TABLE_STATS=1` — reports table-specific counters (cell intrinsic measurements + per-cell layout calls) after layout; also attached to `RenderStats.layout` when diagnostics are enabled.
- `FASTR_DISPLAY_LIST_PARALLEL_MIN=<N>` — lowers the display list parallel fan-out threshold when debugging determinism or forcing rayon paths in tests.

All profiling logs are best run in release builds to reflect real performance.
## Interactive profiling (windowed browser UI)

To capture a CPU profile while interacting with the windowed `browser` UI (resize/scroll, etc.), use
the samply helper:

```bash
timeout -k 10 600 bash scripts/profile_browser_samply.sh [--url <url>] [-- <browser args...>]
```

Close the browser window to finish recording; the script writes a terminal-friendly profile artifact
under `target/browser/profiles/` and prints a `samply load ...` command to view it later.

## Perf smoke (deterministic offline fixtures)

Run a quick offline perf pass against the curated pages fixtures with bundled fonts and
machine-readable timings:

```
timeout -k 10 600 bash scripts/cargo_agent.sh xtask perf-smoke [--suite core|pageset-guardrails|all] [--top 5] [--output target/perf_smoke.json] [-- <extra perf_smoke args...>]
```

The command renders the HTML under `tests/pages/fixtures/*`, captures `DiagnosticsLevel::Basic`
stats, and writes `target/perf_smoke.json` with per-fixture totals and stage timings/counters. The
JSON includes `stage_ms` buckets (`fetch`, `css`, `cascade`, `box_tree`, `layout`, `paint`) derived
from `RenderStats` wall-clock stage timers so regressions can be attributed to a single pipeline
stage without double-counting. (`text_*` timings are subsystem breakdown counters and are reported
separately; they are not included in `stage_ms`.)
Compare against a saved baseline to flag obvious regressions:

```
timeout -k 10 600 bash scripts/cargo_agent.sh xtask perf-smoke --baseline ../baseline/perf_smoke.json --threshold 0.05 --fail-on-regression
```

`--top N` prints the slowest fixtures. With `--fail-on-regression`, baseline comparisons fail the
run when any fixture metric exceeds the relative threshold, making the output suitable for
lightweight CI or local preflight checks.

## Pipeline benchmarks (Criterion)

Run `timeout -k 10 600 bash scripts/cargo_agent.sh bench --bench perf_regressions -- --noplot` to exercise each stage of the rendering pipeline with fixed fixtures and bundled fonts (`tests/fixtures/fonts/DejaVuSans-subset.*`):

- `bench_parse_dom`
- `bench_css_parse`
- `bench_cascade`
- `bench_box_generation`
- `bench_layout_{block,flex,grid,table}`
- `bench_table_intrinsic`
  - `table_column_constraints_only` — isolates table auto-layout intrinsic sizing by measuring column constraint building + column width distribution (no per-cell layout).
  - `table_cell_intrinsic_only` — synthetic N-cell workload that isolates `measure_cell_intrinsic_widths`.
- `bench_paint_{display_list_build,optimize,rasterize}`
- `bench_end_to_end_small` / `bench_end_to_end_realistic`

Outputs are written under `target/criterion/<bench>/new/estimates.json` and do not require network access.

### Comparing runs

Use the helper to flag regressions between two Criterion output trees:

```
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline ../baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

`--metric` accepts `mean` or `median`; `--regression-threshold` is a relative delta (e.g., 0.05 = 5%). CI uses `median` to cut down noise. A non-zero exit status indicates regressions suitable for gating.

### Automated regression checks

Nightly at 06:00 UTC (and on `workflow_dispatch`), `.github/workflows/perf.yml` runs the pipeline benchmarks with `timeout -k 10 600 bash scripts/cargo_agent.sh bench --bench perf_regressions --locked -- --noplot`, uploads `target/criterion` as the `criterion-output` artifact, and downloads the most recent successful artifact from `main`. It then runs:

```
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

The comparison output is attached to the job summary, and regressions over 5% fail the workflow.

### Reproducing the CI diff locally

1. Generate a fresh set of measurements: `timeout -k 10 600 bash scripts/cargo_agent.sh bench --bench perf_regressions -- --noplot`.
2. Download the latest `criterion-output` artifact from the "Performance regression" workflow so it sits under `baseline/target/criterion` (for example, `gh run download --workflow perf.yml --branch main --name criterion-output --dir baseline`).
3. Compare against your local run with the same thresholds CI uses:

```
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

If the command exits non-zero, the benches listed in its output regressed beyond the configured threshold.

## Microbenchmarks

- Run `timeout -k 10 600 bash scripts/cargo_agent.sh bench --bench cascade_bench -- ":has"` to focus on the `:has()` traversal microbench alongside the existing cascade benchmark. The full suite is available via `timeout -k 10 600 bash scripts/cargo_agent.sh bench --bench cascade_bench`.
