# Workstream: Browser Responsiveness (performance, not aesthetics)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

## The job

Make the browser **fast and responsive**. Measurable performance, not subjective aesthetics.

This workstream focuses on **what can be measured**:
- Frame rate (fps)
- Input latency (ms)
- Resize smoothness (ms per frame)
- Scroll performance (fps, jank)
- Time to first paint (ms)

**This workstream does NOT own:**
- Visual design (colors, typography, spacing)
- "Modern look" or aesthetic judgments
- Theming beyond functional requirements
- Animation style (only animation performance)

## What counts

A change counts if it lands at least one of:

- **Frame rate improvement**: Measurable fps increase.
- **Latency reduction**: Measurable input→response time decrease.
- **Jank elimination**: Dropped frames reduced or eliminated.
- **Memory efficiency**: Lower memory usage for same performance.

**Changes that do NOT count:**
- "Looks better" (subjective)
- "Feels smoother" without measurement
- Visual polish without perf impact

## Metrics (measure everything)

### Required metrics

| Metric | Target | Current | How to measure |
|--------|--------|---------|----------------|
| Resize frame time | <16ms | ? | Headless: `scripts/cargo_agent.sh xtask ui-perf-smoke` (`resize_latency_*`).<br>Windowed: `scripts/capture_browser_perf_log.sh` + `browser_perf_log_summary` (`resize_to_present_ms`). |
| Scroll frame time | <16ms | ? | Headless: `scripts/cargo_agent.sh xtask ui-perf-smoke` (`scroll_latency_*`).<br>Windowed: `scripts/capture_browser_perf_log.sh` + `browser_perf_log_summary` (`ui_frame_ms`/fps while scrolling). |
| Input latency | <50ms | ? | Headless: `scripts/cargo_agent.sh xtask ui-perf-smoke` (`input_latency_*`).<br>Windowed: `scripts/capture_browser_perf_log.sh` + `browser_perf_log_summary` (`input_to_present_ms`). |
| Time to first paint (TTFP) | <100ms | ? | Headless: `scripts/cargo_agent.sh xtask ui-perf-smoke` (`ttfp_*`).<br>Windowed: `scripts/capture_browser_perf_log.sh` + `browser_perf_log_summary` (`ttfp_ms`). |
| Idle CPU | ~0% | ? | `scripts/capture_browser_perf_log.sh` + `browser_perf_log_summary` (`cpu_summary.cpu_percent_recent`).<br>Also confirm in an OS profiler while idle. |

### Profiling tools

```bash
# Canonical headless responsiveness harness (`ui_perf_smoke`; JSON summary).
# Maps directly to the required metrics:
# - TTFP: `ttfp_p50_ms` / `ttfp_p95_ms`
# - Scroll: `scroll_latency_p50_ms` / `scroll_latency_p95_ms` (ScrollTo → next frame)
# - Resize: `resize_latency_p50_ms` / `resize_latency_p95_ms` (ViewportChanged → next frame)
# - Input: `input_latency_p50_ms` / `input_latency_p95_ms` (TextInput/Backspace → next frame)
# Note: scroll/resize scenarios run against `about:test-layout-stress` so measurements include
# non-trivial, width-sensitive reflow/layout work.
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json

# Windowed perf-log capture + summary (p50/p95/max) from an interactive session.
# `capture_browser_perf_log.sh` runs the browser under `timeout -k 10 600` + `run_limited.sh --as 64G`.
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --summary \
  target/browser_perf.jsonl about:test-layout-stress

# Manual run: write perf JSONL directly to a file.
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  env FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
    about:test-layout-stress

# Or summarize later (supports --from-ms/--to-ms windowing):
timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- \
  --input target/browser_perf.jsonl

# Debug invalidation fast paths for hover/focus/caret interactions (stderr one-line logs):
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  env FASTR_LOG_INTERACTION_INVALIDATION=1 \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
    about:test-layout-stress

# CPU profiling (Linux): reproduce resize/scroll jank, then close the window to finish recording.
timeout -k 10 600 bash scripts/profile_browser_samply.sh --url about:test-layout-stress
```

See [`docs/perf-logging.md#browser-responsiveness`](../docs/perf-logging.md#browser-responsiveness) for the JSONL schema and summary flags.

## Priority order

### P0: Smooth resize

Window resize must not drop frames.

**Current problem**: Resize is janky—frames drop, UI lags behind window.

**Root causes to investigate**:
- Layout recalculation on every resize event
- Texture reallocation
- Synchronous repaint blocking resize

**Targets**:
- Resize frame time <16ms (60fps)
- No visible lag between window edge and content

### P1: Smooth scrolling

Page scroll must be 60fps.

**Components**:
- Input event → scroll offset update
- Repaint of visible region
- Compositor (if using layers)

**Targets**:
- Scroll frame time <16ms
- No jank when scrolling fast
- Momentum scrolling works naturally

### P2: Input latency

Keystrokes and clicks must feel instant.

**Measure**:
- Keystroke in address bar → character appears
- Click on link → navigation starts
- Tab click → tab switches

**Targets**:
- <50ms for in-chrome interactions
- <100ms for page navigation start

### P3: Efficient idle

Browser should use ~0% CPU when idle.

**Problems to avoid**:
- Polling loops
- Unnecessary repaints
- Background timers spinning

**Verification**: Profile idle browser, CPU should flatline.

## Architecture considerations

### Avoid immediate-mode pitfalls

egui is immediate mode, which means:
- UI is rebuilt every frame
- Can be CPU-intensive for complex UIs
- Must be careful about unnecessary repaints

**Mitigations**:
- Only request repaint when something changes
- Cache expensive computations
- Use `egui::Context::request_repaint()` sparingly

### Async rendering

Page content rendering should not block chrome:
- Render worker on separate thread (already exists)
- Chrome remains responsive during slow renders
- Cancel stale renders when navigating away

### Compositor / layer management

For smooth scrolling and animations:
- Separate scrolling content into layers
- GPU-accelerated compositing
- Avoid full repaint on scroll

## Testing

### Automated performance tests

```rust
#[test]
fn resize_frame_time() {
    let mut app = TestBrowserApp::new();
    let frame_times = measure_frame_times(|| {
        for width in (800..1200).step_by(10) {
            app.resize(width, 600);
            app.render_frame();
        }
    });
    assert!(frame_times.max() < Duration::from_millis(16));
}
```

### Manual testing checklist

- [ ] Resize window quickly—no jank
- [ ] Scroll long page—smooth 60fps
- [ ] Type in address bar—instant response
- [ ] Switch tabs rapidly—no delay
- [ ] Leave browser idle—CPU stays low

## Relationship to other workstreams

- **browser_chrome.md**: Owns functionality; this workstream owns performance of that functionality
- **browser_interaction.md**: Owns interaction semantics; this workstream owns interaction latency
- **live_rendering.md**: Owns render loop architecture; this workstream owns render loop performance

## Success criteria

Browser responsiveness is **done** when:
- All metrics meet targets (see table above)
- No user-perceptible jank during normal use
- Profiling shows no obvious hotspots
- Idle CPU is near zero
