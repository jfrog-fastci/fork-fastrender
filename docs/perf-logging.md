# Performance logging

Helpful environment variables for profiling layout/cascade on large pages. The canonical list is in [env-vars.md](env-vars.md); this page highlights the ones that tend to be most actionable.

When using `render_pages`/`fetch_and_render`, per-page logs are written to `fetches/renders/<page>.log` and a summary to `fetches/renders/_summary.log`; review these alongside the flags below when investigating slow or blank renders.

## Browser perf JSONL logs (`FASTR_PERF_LOG`)

The windowed `browser` UI can emit newline-delimited JSON (`.jsonl`) perf events when `FASTR_PERF_LOG` is enabled. The event schema is defined in Rust in [`fastrender::perf_log_schema`](../src/perf_log_schema.rs) and versioned by `fastrender::perf_log_schema::PERF_LOG_VERSION`. `run_start` events must include the current schema version so offline tooling can validate compatibility.

- `FASTR_CASCADE_PROFILE=1`
  - Enables cascade profiling. Logs node count, candidate/match counts, and timing breakdown for selector matching, declaration application, and pseudo computation at the end of `apply_styles`.

- `FASTR_TRACE_OUT=/tmp/trace.json`
  - Writes a Chrome trace of the render pipeline (fetch/decode/parse/style/layout/paint). Open it in `chrome://tracing` or Perfetto to inspect spans.
  - `fetch_and_render` also supports `--trace-out trace.json`, and library consumers can set `RenderOptions::with_trace_output`.
  - Use `FASTR_TRACE_MAX_EVENTS=<N>` to cap the number of events retained per render (default 200000).

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

These env vars are read in the rendering binaries (`render_pages`, `fetch_and_render`) and cascade
internals; leave them unset for normal runs.

## Browser responsiveness

FastRender’s windowed `browser` UI has separate instrumentation for **responsiveness** (UI frame
times, scroll/resize smoothness, and input latency). This is distinct from the render-pipeline
profiling above (which focuses on parse/style/layout/paint timings during page renders).

See the workstream goals/metric definitions in
[`instructions/browser_responsiveness.md`](../instructions/browser_responsiveness.md).

### Windowed JSONL perf logging (`FASTR_PERF_LOG=1`)

Set `FASTR_PERF_LOG=1` when running the windowed browser to emit **JSON Lines** (one JSON object per
line) describing UI responsiveness events.

For interactive captures, prefer the convenience wrapper (handles `FASTR_PERF_LOG=1` and writes a
JSONL file under repo guardrails):

```bash
bash scripts/capture_browser_perf_log.sh --out target/browser_perf.jsonl --url about:test-layout-stress

# Capture + summarize (runs `browser_perf_log_summary` after the browser exits):
bash scripts/capture_browser_perf_log.sh --out target/browser_perf.jsonl --url about:test-layout-stress --summary
```

Typical run (writes a JSONL log you can post-process with `jq`, pandas, etc.):

```bash
FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

When enabled, you should expect events covering at least:

- **TTFP** (“time to first paint”): navigation start → first presented frame for that tab.
- **Frame time samples** during scroll and resize (used to spot jank and dropped frames).
- **Input latency** samples (input arrival → visible UI response).

The exact schema evolves, but each JSON line is intended to be self-describing. Common fields
include:

- `schema_version` (integer) — currently `1` (omitted on some legacy/diagnostic events).
- `event` (string) — event kind (current: `frame`, `input`, `resize`, `navigation`, `ttfp`; plus
  periodic diagnostics like `idle_summary` / `worker_wake_summary` / `cpu_summary`).
- `ts_ms` (integer) — monotonic timestamp in milliseconds since process start (some legacy/diagnostic
  events use `t_ms`).
- `window_id` (string) — identifier for the window instance (or `"process"` for process-wide
  summaries).
- Event-specific numeric fields such as `ui_frame_ms`, `input_to_present_ms`, `resize_to_present_ms`,
  `ttfp_ms`, etc.

### Headless benchmark harness: `ui_perf_smoke`

For automated/regression-friendly measurements, use the headless harness `ui_perf_smoke`. It runs a
small scripted set of UI scenarios and writes a single JSON summary.

Via `xtask` (recommended):

```bash
bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
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

Examples:

```bash
# Run a single scenario (see `ui_perf_smoke --help` for the full list).
bash scripts/cargo_agent.sh xtask ui-perf-smoke -- --only ttfp_newtab

# Compare against a saved baseline and fail on regressions.
bash scripts/cargo_agent.sh xtask ui-perf-smoke \
  --baseline baseline/ui_perf_smoke.json --threshold 0.05 --fail-on-regression \
  -- --only ttfp_newtab
```

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

## Browser responsiveness (`FASTR_PERF_LOG`)

The windowed `browser` UI can emit a machine-readable JSONL stream describing responsiveness
metrics (per-frame and per-input timing). Enable it by setting:

```bash
FASTR_PERF_LOG=1
```

Each log line is a JSON object that includes:

- `schema_version` (currently `1`)
- `event` (tag)
- `ts_ms` (monotonic timestamp in milliseconds since process start)
- `window_id` (string)

Event payload fields (current schema in `src/bin/browser.rs`, `perf_log::PerfEvent`):

- `event=frame`: `ui_frame_ms`, `fps` (optional), plus window state flags (`window_focused`,
  `window_occluded`, `window_minimized`) and `active_tab_id` (optional).
- `event=input`: `kind` (`keyboard|mouse_wheel|pointer_move|button`), `input_to_present_ms`,
  `input_ts_ms`, `count`, and `active_tab_id` (optional).
- `event=resize`: `resize_to_present_ms`, `resize_ts_ms`, `new_width_px`, `new_height_px`.
- `event=navigation`: `tab_id`, `navigation_seqno`, `url`.
- `event=ttfp`: `tab_id`, `navigation_seqno`, `ttfp_ms`.
- `event=cpu_summary`: `cpu_time_ms_total`, `cpu_percent_recent` (process CPU time over the last interval)

Other JSONL diagnostics may be emitted (for example `event=idle_summary` and `event=worker_wake_summary`);
`browser_perf_log_summary` ignores unknown event types.

To turn a captured JSONL log into p50/p95/max summary numbers, pipe it into the helper:

```bash
FASTR_PERF_LOG=1 \
  bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser \
  | bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary
```

You can also summarize an existing file:

```bash
bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- --input perf.jsonl
```

Filtering options (see `browser_perf_log_summary --help`):

- `--from-ms <ms>` / `--to-ms <ms>`: limit to a timestamp window.
- `--only-event frame|input|resize|ttfp|idle_summary|cpu_summary`: summarize one event type (unknown events are ignored for forward compatibility).

## Interactive profiling (windowed browser UI)

To capture a CPU profile while interacting with the windowed `browser` UI (resize/scroll, etc.), use
the samply helper:

```bash
bash scripts/profile_browser_samply.sh [--url <url>] [-- <browser args...>]
```

Close the browser window to finish recording; the script writes a terminal-friendly profile artifact
under `target/browser/profiles/` and prints a `samply load ...` command to view it later.

## Perf smoke (deterministic offline fixtures)

Run a quick offline perf pass against the curated pages fixtures with bundled fonts and
machine-readable timings:

```
bash scripts/cargo_agent.sh xtask perf-smoke [--suite core|pageset-guardrails|all] [--top 5] [--output target/perf_smoke.json] [-- <extra perf_smoke args...>]
```

The command renders the HTML under `tests/pages/fixtures/*`, captures `DiagnosticsLevel::Basic`
stats, and writes `target/perf_smoke.json` with per-fixture totals and stage timings/counters. The
JSON includes `stage_ms` buckets (`fetch`, `css`, `cascade`, `box_tree`, `layout`, `paint`) derived
from `RenderStats` wall-clock stage timers so regressions can be attributed to a single pipeline
stage without double-counting. (`text_*` timings are subsystem breakdown counters and are reported
separately; they are not included in `stage_ms`.)
Compare against a saved baseline to flag obvious regressions:

```
bash scripts/cargo_agent.sh xtask perf-smoke --baseline ../baseline/perf_smoke.json --threshold 0.05 --fail-on-regression
```

`--top N` prints the slowest fixtures. With `--fail-on-regression`, baseline comparisons fail the
run when any fixture metric exceeds the relative threshold, making the output suitable for
lightweight CI or local preflight checks.

## Pipeline benchmarks (Criterion)

Run `bash scripts/cargo_agent.sh bench --bench perf_regressions -- --noplot` to exercise each stage of the rendering pipeline with fixed fixtures and bundled fonts (`tests/fixtures/fonts/DejaVuSans-subset.*`):

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
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline ../baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

`--metric` accepts `mean` or `median`; `--regression-threshold` is a relative delta (e.g., 0.05 = 5%). CI uses `median` to cut down noise. A non-zero exit status indicates regressions suitable for gating.

### Automated regression checks

Nightly at 06:00 UTC (and on `workflow_dispatch`), `.github/workflows/perf.yml` runs the pipeline benchmarks with `bash scripts/cargo_agent.sh bench --bench perf_regressions --locked -- --noplot`, uploads `target/criterion` as the `criterion-output` artifact, and downloads the most recent successful artifact from `main`. It then runs:

```
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

The comparison output is attached to the job summary, and regressions over 5% fail the workflow.

### Reproducing the CI diff locally

1. Generate a fresh set of measurements: `bash scripts/cargo_agent.sh bench --bench perf_regressions -- --noplot`.
2. Download the latest `criterion-output` artifact from the "Performance regression" workflow so it sits under `baseline/target/criterion` (for example, `gh run download --workflow perf.yml --branch main --name criterion-output --dir baseline`).
3. Compare against your local run with the same thresholds CI uses:

```
bash scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin bench_compare \
  -- --baseline baseline/target/criterion --new target/criterion \
  --regression-threshold 0.05 --metric median
```

If the command exits non-zero, the benches listed in its output regressed beyond the configured threshold.

## Microbenchmarks

- Run `bash scripts/cargo_agent.sh bench --bench cascade_bench -- ":has"` to focus on the `:has()` traversal microbench alongside the existing cascade benchmark. The full suite is available via `bash scripts/cargo_agent.sh bench --bench cascade_bench`.
