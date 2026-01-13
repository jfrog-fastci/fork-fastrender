# Workstream: Live Rendering (dynamic browser, not static images)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

See also:

- [`docs/runtime_stacks.md`](../docs/runtime_stacks.md) for a concrete map of which public API types
  currently include JavaScript execution, an event loop, and live DOM mutation → rerendering.
- [`docs/live_rendering_loop.md`](../docs/live_rendering_loop.md) for the intended `BrowserTab`
  “tick loop” driver API shape (`tick_frame`, `run_until_stable`, event loop vs rAF).

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

## The problem

**FastRender is optimized for static image rendering, not live browser use.**

Current architecture (one-shot render APIs):
- Render HTML → produce image → done
- No author JavaScript execution (`<script>`); JS requires a tab-like runtime (see `BrowserTab`)
- No repaints, animations, or other “over time” updates (single-frame)
- Optimized for "screenshot a page" use case

This is fundamentally wrong for a browser. Real browsers:
- Execute JavaScript continuously
- Repaint on DOM mutations
- Handle animations and timers
- Respond to user interaction in real-time
- Never "finish" rendering—they run until the tab closes

## The job

**Transform FastRender from a static renderer into a live browser engine.**

This is the highest-leverage architectural change for making FastRender a real browser.

## What counts

A change counts if it lands at least one of:

- **JS execution**: Scripts actually run during rendering (not skipped).
- **Repaint on mutation**: DOM changes trigger visual updates.
- **Animation support**: CSS animations, JS animations, requestAnimationFrame work.
- **Timer execution**: setTimeout/setInterval fire and cause repaints.
- **Event-driven rendering**: Render loop responds to events, not just initial load.

## Current state (problems to fix)

### 1. JavaScript may not execute

When rendering a page to an image via the one-shot render pipeline (without `BrowserTab`):
- `<script>` tags may be parsed but not executed
- Inline scripts may be skipped
- External scripts may not be fetched/run
- `DOMContentLoaded` and `load` events may not fire

**Impact**: Pages that build UI via JS render as blank or broken.

### 2. No repaint loop

Current flow:
```
Parse HTML → Style → Layout → Paint → Output image
                                         ↓
                                       DONE
```

Browser flow:
```
Parse HTML → Style → Layout → Paint → Display
     ↑          ↑        ↑        ↑
     └──────────┴────────┴────────┴── Event loop (forever)
                                        - JS execution
                                        - Timer callbacks
                                        - Animation frames
                                        - User input
                                        - Network responses
```

### 3. Animations don't animate

- In the **one-shot** render pipeline, time-based CSS animations/transitions resolve to a
  deterministic single-frame state unless the host explicitly drives time.
- `requestAnimationFrame` callbacks require a JS-capable runtime (`BrowserTab`) and a tick loop
  (`tick_frame` / `next_wake_time`); they are not executed by `FastRender::render_*`.
- Video/audio playback needs a real-time clocking model; see `video_support.md` and the A/V clocking
  model in `docs/media_clocking.md`.

### 4. Event loop isn't running

The **one-shot** render APIs (`FastRender::render_*`) do not include an HTML event loop.

JS-capable containers (`BrowserTab`, `BrowserDocumentJs`) do include an event loop (tasks +
microtasks + timers + rAF + `requestIdleCallback`), but **embedders must drive it** (see `docs/live_rendering_loop.md`).

## Architecture changes needed

### 1. Continuous render loop

```rust
// Current (static)
fn render_to_image(html: &str) -> Image {
    let dom = parse(html);
    let styled = style(dom);
    let layout = layout(styled);
    paint(layout)
}

// Target (live)
fn run_browser_tab(html: &str) -> impl Stream<Item = Frame> {
    let dom = parse(html);
    let mut state = BrowserState::new(dom);
    
    loop {
        // Process pending events
        state.run_event_loop_turn();
        
        // Repaint if dirty
        if state.needs_repaint() {
            let frame = state.paint();
            yield frame;
        }
        
        // Wait for next event or animation frame
        state.wait_for_event_or_vsync();
    }
}
```

### 2. JS execution during initial render

Scripts must execute as part of parsing:
- Inline `<script>`: Execute immediately when encountered
- External `<script>`: Fetch, then execute (blocking unless `async`/`defer`)
- `defer` scripts: Execute after parsing, before `DOMContentLoaded`
- `async` scripts: Execute when ready

### 3. Dirty tracking for incremental repaint

Track what changed:
- DOM mutations → mark subtree dirty
- Style changes → mark affected elements dirty
- Layout invalidation → mark layout dirty
- Only repaint dirty regions

### 4. Animation frame scheduling

```rust
impl BrowserState {
    fn request_animation_frame(&mut self, callback: JsCallback) {
        self.pending_animation_frames.push(callback);
        self.schedule_repaint();
    }
    
    fn run_animation_frames(&mut self) {
        let callbacks = std::mem::take(&mut self.pending_animation_frames);
        let timestamp = self.animation_time();
        for cb in callbacks {
            cb.call(timestamp);
        }
    }
}
```

## Priority order

### P0: JS must execute during rendering

This is **critical**. Without it, most modern pages are broken.

1. Ensure `<script>` tags execute during parsing
2. Ensure external scripts are fetched and executed
3. Fire `DOMContentLoaded` and `load` events
4. Verify with JS-heavy test pages (React, Vue apps)

### P1: Repaint on DOM mutation

1. Implement `MutationObserver` (if not done)
2. Track dirty state when DOM changes
3. Trigger repaint after JS execution completes
4. Test with pages that build UI dynamically

### P2: Timer and animation support

1. `setTimeout`/`setInterval` callbacks fire
2. `requestAnimationFrame` drives animations
3. CSS animations progress over time
4. Test with animated pages

### P3: Efficient incremental repaint

1. Dirty tracking at element level
2. Partial relayout (only affected subtrees)
3. Partial repaint (only damaged regions)
4. Compositor for layer management

## Testing

### Test pages that require live rendering

```html
<!-- js_builds_dom.html -->
<div id="app"></div>
<script>
  document.getElementById('app').innerHTML = '<h1>Hello from JS</h1>';
</script>
```

```html
<!-- js_timer.html -->
<div id="counter">0</div>
<script>
  let count = 0;
  setInterval(() => {
    document.getElementById('counter').textContent = ++count;
  }, 1000);
</script>
```

```html
<!-- js_animation.html -->
<div id="box" style="width:100px;height:100px;background:red"></div>
<script>
  const box = document.getElementById('box');
  function animate(t) {
    box.style.transform = `translateX(${Math.sin(t/1000)*100}px)`;
    requestAnimationFrame(animate);
  }
  requestAnimationFrame(animate);
</script>
```

### Metrics

- **JS execution rate**: % of pages where JS runs correctly
- **Time to interactive**: When can user interact with JS-built UI?
- **Animation frame rate**: Smooth 60fps for simple animations

## Relationship to other workstreams

- **js_html_integration.md**: Defines script loading semantics—this workstream ensures they're honored
- **browser_responsiveness.md**: Performance of the live render loop
- **capability_buildout.md**: CSS/layout correctness—this workstream makes it dynamic
- **video_support.md**: Video is a special case of live/animated content

## Success criteria

Live rendering is **done** when:
- Pages that build UI via JavaScript render correctly
- `setTimeout`/`setInterval` fire and cause visual updates
- CSS and JS animations run smoothly
- The browser can run a simple React/Vue app

This is the transformation from "screenshot tool" to "real browser."
