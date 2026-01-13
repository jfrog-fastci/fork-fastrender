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
| Resize frame time | <16ms | ? | `ui_perf_smoke` (resize path) or windowed perf log capture |
| Scroll frame time | <16ms | ? | `ui_perf_smoke` (scroll path) or windowed perf log capture |
| Input latency | <50ms | ? | `ui_perf_smoke` (input→response latency) |
| Time to first paint (TTFP) | <100ms | ? | `ui_perf_smoke` / perf log summary |
| Idle CPU | ~0% | ? | OS profiler while idle (no interactions) |

### Profiling tools

```bash
# Capture perf logging from an interactive windowed session (closes when you close the window).
scripts/capture_browser_perf_log.sh --url about:test-layout-stress --out target/browser_perf.jsonl --summary

# Enable performance logging (JSONL)
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  env FASTR_PERF_LOG=1 FASTR_PERF_LOG_OUT=target/browser_perf.jsonl \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser

# Headless benchmark harness (`ui_perf_smoke`; JSON summary) for the required UI metrics:
# - TTFP (time to first paint)
# - scroll/resize frame time
# - input latency
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json

# Windowed perf-log capture (stdout JSONL) + summary (preferred over recording huge logs)
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh \
  --url about:test-layout-stress --out target/browser_perf.jsonl --summary

# Or summarize later (supports --from-ms/--to-ms windowing):
timeout -k 10 600 bash scripts/cargo_agent.sh run --release --bin browser_perf_log_summary -- \
  --input target/browser_perf.jsonl

# Profile with samply (Linux)
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  samply record bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser
```

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
