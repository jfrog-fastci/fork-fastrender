# Workstream: Browser Responsiveness

**This workstream is 100% performance-focused.** The philosophy.md 90/10 rule does not apply here - for a browser application, responsiveness IS the product. Users don't see "correct pixels"; they feel jank.

---

## Build Requirements (Non-Negotiable)

**Before pushing ANY change in this workstream:**

```bash
# MUST pass - verifies all feature combinations compile
timeout -k 10 600 bash scripts/cargo_agent.sh check --all-features

# MUST pass - verifies browser_ui feature specifically
timeout -k 10 600 bash scripts/cargo_agent.sh check --features browser_ui

# MUST pass - no new warnings
timeout -k 10 600 bash scripts/cargo_agent.sh clippy --features browser_ui -- -D warnings
```

Performance work often touches many modules. A change that improves scroll performance but breaks `--all-features` compilation is not acceptable. **Run these checks before every push.**

---

## The Reality

FastRender's browser UI is **fundamentally slow** due to architectural decisions that prioritize simplicity over performance. This document is honest about what's broken, why, and what would actually fix it.

**Current state**: Scroll and resize are janky on any non-trivial page. Input latency is noticeable. The browser feels like a 2005-era application.

**Target state**: 60fps scroll/resize on complex pages. <16ms input-to-paint latency. Feels native and instantaneous.

---

## Architecture Overview (Why It's Slow)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           UI Thread (egui/winit)                        │
│  - Receives OS events (resize, scroll, pointer, keyboard)               │
│  - Sends UiToWorker messages to render worker                           │
│  - Receives WorkerToUi messages (frames, state updates)                 │
│  - Uploads pixmaps to GPU textures                                      │
│  - Presents frames via wgpu                                             │
└─────────────────────────────────────────────────────────────────────────┘
                              │ mpsc channel │
                              ▼              ▲
┌─────────────────────────────────────────────────────────────────────────┐
│                         Render Worker Thread                            │
│  - Owns all tab state (DOM, styles, layout, interaction)                │
│  - Processes input events (pointer down/up/move, scroll, keys)          │
│  - Runs JS event dispatch (wheel, click, keydown, etc.)                 │
│  - Executes full render pipeline: Style → Layout → Paint → Rasterize    │
│  - Sends completed Pixmap back to UI thread                             │
└─────────────────────────────────────────────────────────────────────────┘
```

### The Fundamental Problems

1. **Single-pixmap architecture**: The entire page is one `tiny_skia::Pixmap`. No compositing layers. Scroll = full repaint (unless blit eligible).

2. **Synchronous render pipeline**: Input event → full pipeline → pixmap → GPU upload → present. No pipelining, no async layout.

3. **JS on the critical path**: Every pointer/scroll event dispatches to JS before the browser can respond. A page with wheel listeners adds latency to every scroll frame.

4. **Hover triggers full cascade**: Moving the mouse can trigger `invalidate_all()` → full style recalculation → full layout → full paint.

5. **No incremental layout**: Viewport resize = full layout. Text input = full layout. Any DOM mutation = full layout.

---

## Current Performance Characteristics

### Scroll Performance

**Path** (for every wheel tick):
```
1. winit::Event::MouseWheel
2. UI thread sends UiToWorker::Scroll { delta_css, pointer_css }
3. Worker receives message
4. Worker hit-tests to find scroll container under pointer     [~0.5ms]
5. Worker checks for wheel event listeners
6. If listeners: dispatch JS wheel event, pump microtasks      [0-50ms+]
7. If not preventDefault'd: apply scroll delta
8. Check if hover target changed (pointer now over different element)
9. If hover changed: invalidate_all() → full cascade + layout  [10-100ms+]
10. If scroll blit eligible: shift pixels, paint exposed region [~2ms]
11. If NOT blit eligible: full repaint                          [10-50ms]
12. Send Pixmap to UI thread
13. UI thread uploads to GPU texture                            [1-5ms]
14. Present frame                                                [~1ms]
```

**Why scroll blit fails** (see `src/ui/scroll_blit.rs`):
- `position: fixed` anywhere on page → `FixedOrStickyPresent`
- `position: sticky` with inset constraints → `FixedOrStickyPresent`
- `background-attachment: fixed` with image → `FixedOrStickyPresent`
- `scroll-snap-type` active → `ScrollSnapAdjustedEffectiveScroll`
- Scroll-driven animations (`animation-timeline: scroll()`) → `ScrollDrivenAnimationsPresent`
- Non-integer device pixel delta → `NonIntegerDevicePixelDelta`
- Find-in-page highlighting active → `FindHighlightActive`

**Most real-world pages have `position: fixed` headers/footers, so scroll blit almost never works.**

### Resize Performance

**Path** (for every resize event):
```
1. winit::Event::WindowEvent::Resized
2. UI thread sends UiToWorker::ViewportChanged { viewport_css, dpr }
3. Worker sets layout_dirty = true, paint_dirty = true
4. Worker runs FULL cascade (even though styles didn't change)
5. Worker runs FULL layout                                      [10-200ms]
6. Worker runs FULL paint                                       [5-50ms]
7. Worker allocates new Pixmap (may require new size)
8. Send Pixmap to UI thread
9. UI thread checks if GPU texture needs reallocation
10. If texture too small: create new wgpu::Texture              [5-20ms]
11. Upload pixmap data to texture                               [1-10ms]
12. Present frame
```

**The killer**: Step 4-5. We recompute EVERYTHING even though only the viewport size changed.

### Hover Performance

**Path** (for every pointer move):
```
1. winit::Event::CursorMoved
2. UI thread sends UiToWorker::PointerMove { pos_css }
3. Worker hit-tests to find element under pointer               [~0.5ms]
4. Worker computes new hover state
5. Worker computes interaction_css_fingerprint()
6. If fingerprint changed (different element hovered):
   6a. invalidate_all() → style_dirty = layout_dirty = paint_dirty = true
   6b. Run FULL cascade                                         [5-50ms]
   6c. Run FULL layout                                          [10-100ms]
   6d. Run FULL paint                                           [5-50ms]
7. Send Pixmap to UI thread
```

**The problem** (in `src/api/browser_document_dom2.rs`):
```rust
let interaction_css_hash = interaction_state_css_fingerprint(self.interaction_state.as_ref());
if interaction_css_hash != self.interaction_css_hash {
    self.invalidate_all();  // Nuclear option - recascade everything
}
```

This happens because `:hover` pseudo-classes can affect any selector anywhere in the stylesheet. We don't know which elements are actually affected.

---

## What Would Actually Fix This

### Tier 1: Compositing Layers (Architectural)

**The real fix for scroll performance.**

Instead of one pixmap, maintain separate layers:
- **Root layer**: Scrolling content
- **Fixed layers**: `position: fixed` elements
- **Sticky layers**: `position: sticky` elements (with scroll-dependent positioning)

On scroll:
1. Translate root layer by scroll delta (GPU operation, ~0μs)
2. Composite fixed layers on top (GPU operation, ~0μs)
3. Update sticky layer positions based on scroll offset
4. Only repaint if content actually changed

**Implementation sketch**:
```rust
struct CompositorLayer {
    pixmap: Pixmap,
    transform: Transform2D,  // Translation, scale
    opacity: f32,
    blend_mode: BlendMode,
    // Which fragment tree nodes paint to this layer
    fragment_ids: Vec<FragmentId>,
}

struct Compositor {
    layers: Vec<CompositorLayer>,
    // Layer assignment is determined during paint
    // position:fixed/sticky elements get their own layers
}

fn composite_for_scroll(&mut self, scroll_delta: Point) {
    // Just update transforms - no rasterization
    self.layers[0].transform.translate(-scroll_delta.x, -scroll_delta.y);
    // Fixed layers don't move
    // Sticky layers need position recalc but not repaint
}
```

**Effort**: Major architectural change. Touches paint, fragment tree, scroll handling.
**Impact**: 60fps scroll on ALL pages, regardless of fixed/sticky content.

### Tier 2: Incremental Style Invalidation (Algorithmic)

**The real fix for hover performance.**

Instead of `invalidate_all()`, track which elements are affected by `:hover`:

```rust
struct StyleInvalidationMap {
    // Elements that need restyle when this element's hover state changes
    hover_dependents: HashMap<NodeId, Vec<NodeId>>,
    // Elements that need restyle when this element's focus state changes  
    focus_dependents: HashMap<NodeId, Vec<NodeId>>,
}

fn on_hover_change(&mut self, old_hover: Option<NodeId>, new_hover: Option<NodeId>) {
    let mut dirty_nodes = FxHashSet::default();
    
    if let Some(old) = old_hover {
        dirty_nodes.extend(self.invalidation_map.hover_dependents.get(&old));
    }
    if let Some(new) = new_hover {
        dirty_nodes.extend(self.invalidation_map.hover_dependents.get(&new));
    }
    
    // Only restyle these specific nodes, not the entire tree
    for node_id in dirty_nodes {
        self.dirty_style_nodes.insert(node_id);
    }
    self.style_dirty = !self.dirty_style_nodes.is_empty();
}
```

**Building the invalidation map** requires analyzing selectors during stylesheet parsing:
- `.foo:hover .bar` → when `.foo`'s hover changes, `.bar` descendants need restyle
- `a:hover` → when `<a>` hover changes, only that element needs restyle

**Effort**: Medium. Requires selector analysis during CSS parsing.
**Impact**: Hover no longer triggers full cascade. Only affected elements restyle.

### Tier 3: Incremental Layout (Algorithmic)

**The real fix for text input and small DOM mutations.**

Track layout dependencies so we can re-layout only affected subtrees:

```rust
struct LayoutDependencies {
    // Nodes whose layout depends on this node's size
    size_dependents: HashMap<NodeId, Vec<NodeId>>,
    // Nodes whose layout depends on viewport size
    viewport_dependents: Vec<NodeId>,
}

fn on_text_input(&mut self, input_node: NodeId) {
    // Text input changes intrinsic size of input element
    // This may affect:
    // 1. The input's own layout
    // 2. Parent flex/grid container (if input is a flex item)
    // 3. Siblings (if flex wrap or grid auto-placement)
    
    let affected = self.layout_deps.compute_affected(input_node);
    self.dirty_layout_nodes = affected;
}
```

**Effort**: Large. Layout is complex; dependency tracking is hard.
**Impact**: Text input, small DOM changes don't require full layout.

### Tier 4: Async/Pipelined Rendering (Architectural)

**The real fix for input latency.**

Current: Input → Process → Render → Present (all serial)
Target: Input → Queue → [Render in background] → Present when ready

```rust
// UI thread: never blocks on render
fn handle_scroll(&mut self, delta: Point) {
    // Immediately update scroll offset (optimistic)
    self.optimistic_scroll += delta;
    
    // Queue render request (non-blocking)
    self.worker_tx.send(UiToWorker::Scroll { delta });
    
    // Use GPU compositor to show intermediate state
    // (translate existing texture while waiting for new frame)
    self.compositor.translate_root_layer(-delta);
    
    // Present immediately with stale content + translation
    self.present();
}

// When worker responds:
fn handle_frame_ready(&mut self, frame: RenderedFrame) {
    // Upload new content, reset compositor transforms
    self.upload_texture(&frame.pixmap);
    self.compositor.reset_transforms();
}
```

**Effort**: Major architectural change.
**Impact**: Input feels instant; render catches up asynchronously.

### Tier 5: Remove JS from Critical Path (Protocol)

**Quick win for scroll/input latency.**

Currently, wheel events dispatch to JS before scroll applies. This means a page with:
```js
document.addEventListener('wheel', () => {
    // Even empty listener adds latency
});
```
...makes scrolling slower.

**Fix**: Dispatch JS events asynchronously, apply scroll immediately:

```rust
fn handle_scroll(&mut self, delta: Point, pointer: Point) {
    // Apply scroll FIRST (before JS)
    self.scroll_state.viewport += delta;
    
    // Queue JS wheel event dispatch for later
    self.pending_js_events.push(JsEvent::Wheel { delta, pointer });
    
    // Render with new scroll state
    self.paint_if_needed();
    
    // JS runs AFTER frame is sent
    // If JS calls preventDefault(), we handle it on next frame
}
```

**Caveat**: This changes semantics. `preventDefault()` on wheel events would apply to the *next* scroll, not the current one. This is a deliberate trade-off: lower latency for the common case (non-cancelled scrolls) at the cost of one-frame delay for the rare case (JS wanting to cancel).

**Effort**: Medium. Changes event dispatch order.
**Impact**: Scroll latency reduced by JS execution time (can be 50ms+ on heavy pages).

---

## Measurement (The Easy Part)

### Primary Metrics

| Metric | Current | Target | Measurement |
|--------|---------|--------|-------------|
| Scroll frame time (no fixed) | ~8ms | <8ms | `ui_perf_smoke --only scroll_fixture` |
| Scroll frame time (with fixed) | ~40ms | <16ms | Need new fixture with `position:fixed` |
| Resize frame time | ~60ms | <16ms | `ui_perf_smoke --only resize_fixture` |
| Hover response time | ~30ms | <8ms | Need new metric |
| Input-to-paint latency | ~25ms | <16ms | `ui_perf_smoke --only input_text` |
| Idle CPU | ~2% | <0.5% | `browser_perf_log_summary --only-event cpu_summary` |

### Diagnostic Commands

```bash
# Full responsiveness harness (headless, deterministic)
timeout -k 10 600 bash scripts/cargo_agent.sh xtask ui-perf-smoke --output target/ui_perf_smoke.json

# Interactive capture with perf log
timeout -k 10 600 bash scripts/capture_browser_perf_log.sh --summary \
  --url about:test-layout-stress \
  --out target/browser_perf.jsonl

# CPU profile during interaction (close window to finish)
timeout -k 10 600 bash scripts/profile_browser_samply.sh --url about:test-layout-stress

# Timeline trace (Perfetto JSON format)
timeout -k 10 600 bash scripts/cargo_agent.sh xtask browser --release \
  --trace-out target/browser_trace.json \
  about:test-layout-stress

# Debug why scroll blit failed
FASTR_LOG_SCROLL_BLIT=1 timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
  about:test-layout-stress

# Debug what triggered invalidation
FASTR_LOG_INTERACTION_INVALIDATION=1 timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --features browser_ui --bin browser -- \
  about:test-layout-stress
```

---

## Implementation Roadmap

### Phase 0: Instrumentation (1-2 days)

Before fixing anything, add visibility:

1. **Scroll blit logging**: Log when blit fails and why
   - File: `src/ui/render_worker.rs`, scroll handling
   - Add: `WorkerToUi::ScrollBlitFallback { reason }` or perf log event

2. **Invalidation cause logging**: Log what triggered each repaint
   - File: `src/api/browser_document_dom2.rs`
   - Add: Track `InvalidationCause` enum through the pipeline

3. **Per-frame breakdown**: Finer timing granularity
   - File: `src/ui/browser_app.rs`, frame loop
   - Add: `cascade_ms`, `layout_ms`, `paint_ms`, `upload_ms` to perf log

4. **Hover invalidation stats**: Count how often hover triggers full cascade
   - File: `src/api/browser_document_dom2.rs`
   - Add: Counter for `hover_triggered_full_cascade`

### Phase 1: Quick Wins (1-2 weeks)

Low-risk changes with measurable impact:

1. **Paint-only hover for simple cases**
   - If page has no `:hover` selectors that affect layout, skip cascade/layout
   - File: `src/api/browser_document_dom2.rs`, `render_frame_with_deadlines`
   - Check: `interaction_paint_hash` vs `interaction_css_hash` distinction exists

2. **Scroll blit for fractional DPR**
   - Currently fails on non-integer device pixel deltas
   - Round to nearest pixel instead of failing
   - File: `src/ui/scroll_blit.rs`, `approx_integer`

3. **Aggressive scroll event coalescing**
   - Coalesce all pending scroll events before rendering
   - File: `src/ui/render_worker.rs`, `drain_scroll_messages`
   - Current: Coalesces but may still process multiple per frame

4. **Skip layout on scroll-only changes**
   - If only `scroll_state` changed and no hover change, skip layout
   - File: `src/ui/render_worker.rs`, paint decision logic

### Phase 2: Targeted Fixes (2-4 weeks)

Medium-effort changes for specific problems:

1. **Hover invalidation narrowing**
   - Parse `:hover` selectors to determine which elements are affected
   - Only invalidate those elements, not the whole tree
   - Files: `src/style/cascade.rs`, `src/style/selectors.rs`

2. **Viewport resize without full restyle**
   - Changing viewport shouldn't require cascade (styles don't change)
   - Only layout is actually viewport-dependent
   - File: `src/api/browser_document_dom2.rs`, `invalidate_layout` vs `invalidate_all`

3. **Texture pooling**
   - Pre-allocate a pool of GPU textures at common sizes
   - Avoid allocation on resize
   - File: `src/ui/wgpu_pixmap_texture.rs`

4. **Fixed element detection at layout time**
   - Mark fragments as "fixed-positioned" during layout
   - Use this to skip blit check entirely (we know it will fail)
   - File: `src/tree/fragment_tree.rs`, `src/ui/scroll_blit.rs`

### Phase 3: Architectural (1-3 months)

Major changes for step-function improvement:

1. **Compositor layer system**
   - Separate fixed/sticky content into layers
   - Composite layers on GPU instead of CPU repaint
   - Files: New `src/compositor/` module

2. **Async render pipeline**
   - Decouple input handling from render completion
   - Use GPU translation for immediate feedback
   - Files: `src/ui/browser_app.rs`, `src/ui/render_worker.rs`

3. **Incremental layout**
   - Track layout dependencies
   - Re-layout only affected subtrees
   - Files: `src/layout/engine.rs`, `src/api/browser_document_dom2.rs`

---

## Anti-Patterns (Don't Do These)

### ❌ "Optimize the hot function"

Profiling shows `cascade_apply_styles` is slow. The fix is NOT to micro-optimize it - the fix is to not call it at all when hover changes.

### ❌ "Add more caching"

Caching helps when you have cache hits. If every input event invalidates everything, caching just adds overhead.

### ❌ "Debounce/throttle events"

This adds latency. Users notice. The goal is to handle events faster, not less frequently.

### ❌ "Optimize GPU upload"

Uploading a 1920x1080 RGBA pixmap takes ~2ms. Optimizing this to 1.5ms doesn't matter when layout takes 50ms. Fix the pipeline, not the upload.

### ❌ "Parallel layout/paint"

Parallelism helps throughput but not latency. The problem is we do too much work, not that we do it serially.

---

## Success Criteria

This workstream is **done** when:

1. **Scroll**: 60fps on pages with `position: fixed` headers (e.g., any modern website)
2. **Resize**: 60fps during continuous window resize
3. **Hover**: No perceptible delay when moving mouse over interactive elements
4. **Input**: Keystrokes appear in <16ms
5. **Idle**: CPU usage <0.5% when browser is idle

**Evidence required**:
- `ui_perf_smoke` metrics meet targets
- Interactive testing on real pages (Twitter, GitHub, HN) feels smooth
- CPU profile shows no hot functions taking >5ms on the critical path

---

## Key Files

| Area | Primary Files |
|------|---------------|
| Event loop | `src/ui/browser_app.rs` |
| Worker thread | `src/ui/render_worker.rs` |
| Invalidation | `src/api/browser_document_dom2.rs` |
| Scroll blit | `src/ui/scroll_blit.rs` |
| Texture upload | `src/ui/wgpu_pixmap_texture.rs` |
| Cascade | `src/style/cascade.rs` |
| Layout | `src/layout/engine.rs` |
| Paint | `src/paint/painter.rs` |
| Interaction | `src/interaction/engine.rs` |
| Hit testing | `src/interaction/hit_test.rs` |

---

## References

- [CSS Containment Level 2](https://www.w3.org/TR/css-contain-2/) — understanding layout isolation
- [CSS Scroll Snap](https://www.w3.org/TR/css-scroll-snap-1/) — scroll snap point semantics
- [CSSOM View](https://www.w3.org/TR/cssom-view-1/) — scroll APIs and viewport semantics
- [UI Events](https://www.w3.org/TR/uievents/) — wheel/pointer event specifications
