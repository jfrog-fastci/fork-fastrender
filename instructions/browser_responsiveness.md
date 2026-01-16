# Browser responsiveness (workstream quick reference)

This workstream is **performance-only**: scroll/resize/input latency are the product. This file is a
short pointer doc; the full architecture analysis, bottlenecks, and roadmap live in
[PLAN.md](../PLAN.md).

## Goals & metrics (from PLAN.md)

- **Scroll frame time (no fixed elements)**: target **< 8ms** (`ui_perf_smoke --only scroll_fixture`).
- **Scroll frame time (with fixed/sticky)**: target **< 16ms** (new fixture planned).
- **Resize frame time**: target **< 16ms** (`ui_perf_smoke --only resize_fixture`).
- **Hover response time**: target **< 8ms** (metric to be added).
- **Input-to-paint latency**: target **< 16ms** (`ui_perf_smoke --only input_text`).
- **Idle CPU**: target **< 0.5%** (`browser_perf_log_summary --only-event cpu_summary`).

Treat these as the headline outcomes; changes should move at least one of them.

## Canonical perf commands

### Headless harness (`ui_perf_smoke`)

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json
```

### Interactive perf log capture (`capture_browser_perf_log`)

```bash
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --summary \
  --url about:test-layout-stress \
  --out target/browser_perf.jsonl
```

### CPU profile (Samply)

```bash
timeout -k 10 600 bash scripts/profile_browser_samply.sh --url about:test-layout-stress
```

### UI trace (`--trace-out`, Perfetto/Chrome trace)

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release \
  --trace-out target/browser_trace.json \
  about:test-layout-stress
```

For deeper context, **read [PLAN.md](../PLAN.md)**.
