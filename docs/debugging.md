# Render and paint debugging

When a render comes out blank/black or is missing content, use the debug/profiling environment variables to narrow down where pixels disappear. These flags are read at runtime (no rebuild required).

The canonical list lives in [env-vars.md](env-vars.md). This page highlights the most useful ones and the common workflow.

`render_pages` writes per-page logs to `fetches/renders/<page>.log`, captures worker stdout/stderr in `fetches/renders/<page>.stderr.log`, and writes a summary to `fetches/renders/_summary.log`.

## Hard sites (Akamai) fetch checklist

When a pageset run fails to fetch a page or subresource from a CDN-protected "hard" site (common examples: Akamai), the failure mode often looks like:

- HTTP/1.1 hangs/timeouts or "0 bytes"/empty-body responses.
- HTTP/2 errors such as `INTERNAL_ERROR` (backend-dependent).

Checklist (start here, in order):

1. **Use an HTTP/2-capable backend**: `FASTR_HTTP_BACKEND=reqwest` (in-process) or `FASTR_HTTP_BACKEND=curl` (shell out).
   - `auto` (default) prefers `reqwest` for `https://` URLs and will fall back to `curl` on retryable network/TLS/HTTP2 errors.
   - Ensure your system `curl` supports HTTP/2 (check `curl --version` includes `HTTP2`) before relying on the `curl` backend.
   - For differential diagnosis:
     - `FASTR_HTTP_BACKEND=reqwest` disables the `curl` fallback (pure in-process HTTP/2).
     - `FASTR_HTTP_BACKEND=ureq` disables the `curl` fallback and forces the HTTP/1.1 Rust backend.
   - If an error message includes `curl fallback failed: ...`, `auto` already attempted both backends.
   - If you are seeing `empty HTTP response body` (0 bytes) failures, force `FASTR_HTTP_BACKEND=curl`: `auto` only falls back on network/TLS/HTTP2-style errors and will not switch backends for empty-body responses.
2. **Enable browser-like headers**: `FASTR_HTTP_BROWSER_HEADERS=1` (this is the default; set it explicitly when debugging to ensure it wasn't disabled).
   - Some font/CDN endpoints are sensitive to `Accept: */*` plus `Origin`/`Referer`; the browser-header profile is intended to match those expectations.
3. **Turn on retry logging** when debugging transient failures: `FASTR_HTTP_LOG_RETRIES=1`.
   - `pageset_progress`: retry logs land in `target/pageset/logs/<stem>.stderr.log` (stdout/stderr from the worker process).
   - `render_pages`: retry logs land in `fetches/renders/<stem>.stderr.log` (default worker mode).
4. **Make sure you’re testing the network path** (not stale caches): use `fetch_pages --refresh` for HTML and consider disabling the disk cache (`DISK_CACHE=0`) when chasing subresource fetch behavior.

Example (pageset loop, targeted):

```bash
FASTR_HTTP_BACKEND=reqwest FASTR_HTTP_BROWSER_HEADERS=1 FASTR_HTTP_LOG_RETRIES=1 \
  bash scripts/cargo_agent.sh xtask pageset --pages tesco.com,washingtonpost.com
```

## inspect_frag overlays and dumps

- `inspect_frag --dump-json <dir> tests/fixtures/html/block_simple.html` writes `dom.json`, `composed_dom.json`, `styled.json`, `box_tree.json`, `fragment_tree.json`, and `display_list.json` for downstream tooling. Pair with `--filter-selector`/`--filter-id` to focus on a specific subtree.
- `inspect_frag --render-overlay out.png <file>` renders the document with overlays for fragment bounds, box ids, stacking contexts, and scroll containers to quickly correlate geometry with the rendered pixels.

## Offline fixture “page loop” (recommended)

When iterating on a single offline fixture under `tests/pages/fixtures/<stem>/index.html`, use the
one-command driver:

```bash
bash scripts/cargo_agent.sh xtask page-loop --fixture bbc.co.uk --overlay --write-snapshot --chrome
```

If you don’t know which page to pick next, you can select a single fixture from the committed
pageset progress artifacts (`progress/pages/*.json`):

```bash
# Pick the current single worst-accuracy ok page (requires `accuracy.diff_percent` in progress JSON).
bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-worst-accuracy 1 --overlay --write-snapshot --chrome

# Or pick the first failing page (status != ok):
bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --only-failures --overlay --write-snapshot --chrome

# Or pick the slowest page (useful for perf hotspots):
bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-slowest 1 --overlay --write-snapshot --chrome

# Or restrict to a hotspot category (case-insensitive; e.g. css/cascade/layout/paint):
bash scripts/cargo_agent.sh xtask page-loop --from-progress progress/pages --top-worst-accuracy 1 --hotspot layout --overlay --write-snapshot --chrome
```

Note: `--chrome` spawns headless Chrome. Modern Chrome reserves a very large virtual address space up front (>64GiB), so if you see a failure containing `Oilpan: Out of memory`, bump the xtask address-space cap:

```bash
FASTR_XTASK_LIMIT_AS=128G bash scripts/cargo_agent.sh xtask page-loop --fixture bbc.co.uk --overlay --write-snapshot --chrome
# Or disable the address-space cap entirely (less safe on shared hosts):
FASTR_XTASK_LIMIT_AS=unlimited bash scripts/cargo_agent.sh xtask page-loop --fixture bbc.co.uk --chrome
```

Artifacts are written under `target/page_loop/<stem>/`:

- `fastrender/<stem>.png` (+ `<stem>.json` metadata)
- `fastrender/<stem>/snapshot.json` (when `--write-snapshot`)
- `overlay/<stem>.png` (when `--overlay`)
- `chrome/<stem>.png` and `report.html` (when `--chrome`)

Use `--viewport`, `--dpr`, and `--media screen|print` to align FastRender, overlays, and Chrome.

## Display-list dumps (paint pipeline)

- `FASTR_DUMP_STACK=1` – dump the stacking context tree
- `FASTR_DUMP_FRAGMENTS=1` – dump the fragment tree used for painting
- `FASTR_DUMP_COUNTS=1` – dump display-list counts only
- `FASTR_DUMP_TEXT_ITEMS=1` – dump text display items
- `FASTR_DUMP_COMMANDS=<N>` – dump the first N display commands (omit `N` to dump all)

## Helpful logging

- `FASTR_RENDER_TIMINGS=1` – high-level per-stage timings
- `FASTR_PAINT_STATS=1` – paint-stage timings
- `FASTR_LOG_IMAGE_FAIL=1` – image decode/load failures (raster + SVG)
- `FASTR_LOG_FRAG_BOUNDS=1` – fragment-tree bounds vs viewport
- `FASTR_TRACE_FRAGMENTATION=1` – trace fragmentation break opportunities/boundary choices (`inspect_frag --trace-fragmentation` sets this for you)

## Text-oriented debugging

- `FASTR_DUMP_TEXT_FRAGMENTS=<N>` – sample text fragments (positions + preview text)
- `FASTR_TRACE_TEXT=<substring>` – print a containment trail for the first text fragment containing the substring

## Quick examples

Dump the first 400 paint commands:

```bash
FASTR_DUMP_COMMANDS=400 \
  bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin render_pages -- --pages news.ycombinator.com
```

Log timings + fragment bounds for a single render:

```bash
FASTR_RENDER_TIMINGS=1 FASTR_LOG_FRAG_BOUNDS=1 \
  bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- https://example.com out.png --timeout 20
```

## Chrome/Perfetto traces from `pageset_progress`

- `pageset_progress run --trace-failures` or `--trace-slow-ms <ms>` re-runs targeted pages with Chrome tracing enabled. Traces are written to `target/pageset/traces/<stem>.json`, and rerun progress/logs live under `target/pageset/trace-progress/` and `target/pageset/logs/*.trace.log`.
- Trace reruns default to more generous budgets: `--trace-timeout` defaults to `timeout * 2`, `--trace-soft-timeout-ms` defaults to the trace timeout minus a 250ms buffer, and `--trace-jobs` defaults to 1 to keep captures stable. Increase the trace timeouts instead of the main ones if the extra tracing overhead trips the hard kill.
- To inspect a trace locally, open Chrome and navigate to `chrome://tracing` then load the JSON, or drag the file into https://ui.perfetto.dev/. `pageset_progress report --include-trace` lists the stems and paths of traces that were collected.

## Pipeline dumps from `pageset_progress`

- `pageset_progress run --dump-failures summary` writes pipeline snapshots for failing pages under `target/pageset/dumps/<stem>/`. Use `--dump-slow-ms <ms> --dump-slow <summary|full>` to capture slow-but-OK pages.
- Dumps default to `--dump-timeout timeout*2` with a soft timeout 250ms earlier; bump the dump timeout instead of the main timeout when captures need extra headroom.

## Pageset triage report (brokenness inventory)

To turn the committed `progress/pages/*.json` scoreboard (timings/hotspots/accuracy) plus the latest offline fixture-vs-Chrome diffs into an actionable per-page template:

- Each page section reports whether `tests/pages/fixtures/<stem>/index.html` exists.
- The generated markdown includes a ready-to-run `xtask page-loop` command for each page.
- When a fixture is missing, the report also includes turnkey `bundle_page fetch` → `xtask import-page-fixture` → `xtask validate-page-fixtures` commands to capture/import/validate the offline repro.

```bash
# Optional: generate/update the fixture diff report first.
bash scripts/cargo_agent.sh xtask fixture-chrome-diff --from-progress progress/pages --only-failures --top-worst-accuracy 20

# Generate the triage markdown (no rendering; pure file processing).
bash scripts/cargo_agent.sh xtask pageset-triage \
  --progress-dir progress/pages \
  --report target/fixture_chrome_diff/report.json

# Output: target/pageset_triage/report.md
```

Use `--only <stem,...>` to focus on a subset, or `--top-worst-accuracy N` / `--top-slowest N` to slice the report, then fill in each page’s **Brokenness inventory** section to coordinate fixes across subsystems (layout/text/paint/resources).
