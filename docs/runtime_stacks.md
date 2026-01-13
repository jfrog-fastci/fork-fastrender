# Runtime stacks: `BrowserDocument` vs `BrowserDocument2` vs `BrowserDocumentDom2` vs `BrowserTab` vs `BrowserDocumentJs`

FastRender currently exposes multiple “document/tab” APIs that look similar but live at different layers.
This doc is a **choose-the-right-type** reference that answers:

- Which DOM representation is in use?
- Can JavaScript run?
- Is there an event loop (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`)?
- Do live DOM mutations trigger rerendering?

All types below are exported as `fastrender::api::*` and also re-exported at the crate root
(e.g. `fastrender::BrowserTab`).

## Quick selection (what should I use?)

- **One-shot, static render (no JS):** `FastRender::render_html` / `FastRender::render_url`.
- **Host-driven DOM mutations + rerender (no JS):**
  - `api::BrowserDocument` if you’re fine with the renderer’s internal `dom::DomNode`.
  - `api::BrowserDocumentDom2` / `api::BrowserDocument2` if you want to mutate a `dom2::Document`.
- **Headless screenshots with JS / author scripts:** `api::BrowserTab` + `run_until_stable(...)` +
  `render_frame()`.
- **Interactive/live rendering loop:** `api::BrowserTab` driven via repeated `tick_frame()` calls
  (see [`docs/live_rendering_loop.md`](live_rendering_loop.md) for how rAF/timers fit in).
- **Desktop browser UI embedding:** the windowed `browser` app’s worker currently renders via
  `api::BrowserDocument` but also maintains a JS-capable `api::BrowserTab` (JS enabled by default;
  no stable CLI toggle to disable it yet) and best-effort syncs its `dom2` snapshot into the renderer
  DOM before painting (see [`docs/browser_ui.md`](browser_ui.md)).
- **Legacy JS host harness (manual script execution, no HTML script scheduler):**
  `api::BrowserDocumentJs`.

## Composition sketch (how the types stack)

This is not a full architecture diagram; it’s a “what does this object own?” map:

- `BrowserDocument`
  - `FastRender` + live renderer DOM (`dom::DomNode`) + render caching/dirty flags
- `BrowserDocumentDom2`
  - `FastRender` + live spec-ish DOM (`dom2::Document`) + render caching/dirty tracking
- `BrowserDocument2`
  - `FastRender` + live `dom2::Document` + render caching (coarse full invalidation)
- `BrowserTab`
  - `BrowserDocumentDom2`
  - `EventLoop<BrowserTabHost>` (tasks + microtasks + timers + rAF + `requestIdleCallback` queues)
  - `BrowserTabJsExecutor` (e.g. `VmJsBrowserTabExecutor`)
  - navigation/history state
- `BrowserDocumentJs`
  - `BrowserDocumentDom2`
  - `EventLoop<BrowserDocumentJs>` (tasks + microtasks + timers + rAF + `requestIdleCallback` queues)
  - legacy `VmJsRuntime` + `ScriptOrchestrator` (manual script execution; no HTML `<script>` scheduling)

## Capability matrix (repo reality)

| Type | DOM representation | Live DOM mutation + rerender | JS execution | Event loop | HTML `<script>` processing | Navigation/history |
|---|---|---|---|---|---|---|
| `api::BrowserDocument` | `crate::dom::DomNode` | Yes (`render_if_needed`) | No | No | No | URL fetch+replace (`navigate_url`) |
| `api::BrowserDocumentDom2` | `crate::dom2::Document` (authoritative) + snapshots to `DomNode` for layout | Yes (`render_if_needed`) | No (host must embed) | No | No | URL fetch+replace (`navigate_url`) |
| `api::BrowserDocument2` | `crate::dom2::Document` + snapshots to `DomNode` for layout | Yes (`render_if_needed`) | No | No | No | URL fetch+replace (`navigate_url`) |
| `api::BrowserTab` | `BrowserDocumentDom2` + tab state | Yes (driven by event loop + rendering) | Yes (via `BrowserTabJsExecutor`) | Yes (`EventLoop<BrowserTabHost>`) | Yes (streaming parser + scheduler) | Yes (history + script-driven navigations) |
| `api::BrowserDocumentJs` | `BrowserDocumentDom2` | Yes (driven by its event loop + rendering) | Yes (manual; host-supplied `ScriptBlockExecutor` + legacy WebIDL `VmJsRuntime`) | Yes (`EventLoop<BrowserDocumentJs>`) | **No** (manual script execution) | Manual fetch+replace via `document_mut().navigate_url(...)` (no history) |

Notes:

- “Live DOM mutation + rerender” means: if you mutate the live DOM, the next `render_if_needed()` /
  `tick_frame()` will rerun the render pipeline and produce a new `Pixmap`.
- “HTML `<script>` processing” means: parser-inserted scripts execute at parse time, and script
  scheduling follows FastRender’s current HTML-integration implementation (see
  [`docs/js_embedding.md`](js_embedding.md) and
  [`docs/html_script_processing.md`](html_script_processing.md)).

## `api::BrowserDocument` (renderer DOM, no JS)

Implementation: [`src/api/browser_document.rs`](../src/api/browser_document.rs)

`BrowserDocument` is a **multi-frame renderer** that owns:

- a `FastRender` instance, and
- a live renderer DOM tree: `crate::dom::DomNode`.

It is designed for “render, mutate, rerender” workflows **without JavaScript**:

- mutate via `dom_mut()` / `mutate_dom(...)`,
- render via `render_frame()` or `render_if_needed()`.

What it does *not* provide:

- no JS runtime,
- no HTML event loop (tasks/microtasks),
- the DOM type is the renderer’s internal DOM, not the `dom2`/WebIDL-shaped DOM used by JS bindings.

## `api::BrowserDocumentDom2` (dom2 + render caching; foundation for JS)

Implementation: [`src/api/browser_document_dom2.rs`](../src/api/browser_document_dom2.rs)

`BrowserDocumentDom2` mirrors `BrowserDocument`, but the **authoritative DOM** is a spec-ish,
mutable `crate::dom2::Document`.

Key properties:

- The renderer snapshots the `dom2::Document` into an immutable `dom::DomNode` only when a new
  layout/style recomputation is needed.
- DOM mutations are tracked so `render_if_needed()` can rerun the pipeline when required.
- This is the document type used by `BrowserTab` and by dom2-backed JS bindings (`crate::js::DomHost`).

What it does *not* provide on its own:

- no JS executor,
- no `EventLoop` (tasks/microtasks),
- no HTML `<script>` scheduling.

Use it when you want a live, mutable `dom2` document + rendering, but you are **not** running page JS
(or you are embedding your own JS engine on top).

## `api::BrowserDocument2` (dom2 renderer, no JS/event loop)

Implementation: [`src/api/browser_document2.rs`](../src/api/browser_document2.rs)

`BrowserDocument2` is another dom2-backed, multi-frame renderer. Like `BrowserDocumentDom2`, it
stores a live `crate::dom2::Document` and snapshots it to `dom::DomNode` when layout needs to be
recomputed.

Compared to `BrowserDocumentDom2`, it does **not** carry the extra host-side state that `BrowserTab`
relies on (for example `Document.currentScript` bookkeeping and active event tracking).

It also uses **coarser invalidation**: any `dom2` mutation reported as “changed” invalidates the
entire render pipeline (style/layout/paint), rather than consuming `dom2` mutation logs and tracking
dirty sets.

`BrowserTab` is built on `BrowserDocumentDom2` (not `BrowserDocument2`).


## `api::BrowserTab` (dom2 + JS + event loop + navigation)

Implementation: [`src/api/browser_tab.rs`](../src/api/browser_tab.rs)

`BrowserTab` is the JS-capable “tab runtime” API. It owns, at minimum:

- a `BrowserDocumentDom2` (live `dom2` document + render caching),
- an HTML-shaped `EventLoop<BrowserTabHost>` (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`),
- script scheduling/plumbing for HTML parsing (`StreamingHtmlParser` + `HtmlScriptScheduler`),
- a pluggable JS executor (`BrowserTabJsExecutor`), e.g. [`VmJsBrowserTabExecutor`](../src/api/browser_tab_vm_js_executor.rs).

This is the type that supports **page scripts that mutate the DOM over time**.

### Headless screenshots *with JS*

For “load a page, let JS settle, then screenshot” workflows, use `BrowserTab` and drive it with
`run_until_stable(...)` before rendering:

```rust,no_run
use fastrender::{BrowserTab, RenderOptions, Result};

fn main() -> Result<()> {
    let mut tab = BrowserTab::from_url_with_vmjs(
        "https://example.com",
        RenderOptions::new().with_viewport(1280, 720),
    )?;

    // Run JS/event-loop work until stable (bounded).
    let _ = tab.run_until_stable(/* max_frames */ 10)?;

    // Then render a frame.
    let pixmap = tab.render_frame()?;
    pixmap.save_png("out.png")?;
    Ok(())
}
```

### Live rendering: the “tick loop” integration point

For interactive/live rendering (a tab that never “finishes”), the core integration is a **tick loop**:

- run some amount of event-loop work (tasks + microtasks; plus at most one `requestAnimationFrame`
  turn when callbacks are queued),
- if the document became dirty, render and display the new frame,
- repeat, driven by your outer UI loop (vsync, timers, network wakeups, input events).

In the public API this maps to:

- `BrowserTab::tick_frame()` — run at most one task turn (or a microtask checkpoint) and return a
  freshly rendered `Pixmap` *only if* something invalidated rendering.
- `BrowserTab::run_until_stable(...)` — a bounded helper for “drive until idle + rendered”.

Note: `tick_frame()` runs at most one `requestAnimationFrame` “turn” when callbacks are queued and
the next frame is due (paced by `JsExecutionOptions.animation_frame_interval`), and it drains the
post-rAF microtask checkpoint before rendering. It does **not** enforce a wall-clock frame cadence
by itself; interactive embedders should call it on their chosen frame schedule and can use
`BrowserTab::next_wake_time()` as a sleep hint (see
[`docs/live_rendering_loop.md`](live_rendering_loop.md)).

Conceptually:

```rust,no_run
# use fastrender::{BrowserTab, Result};
# fn drive(mut tab: BrowserTab) -> Result<()> {
loop {
    if let Some(pixmap) = tab.tick_frame()? {
        // Present pixmap in your window/texture.
        drop(pixmap);
    }

    // Your outer loop decides when to call tick_frame() again:
    // - after forwarding input to the tab (mouse/keyboard),
    // - when timers/network wake the event loop,
    // - on vsync / a frame budget.
    //
    // `next_wake_time()` is a convenience helper for sleeping until the next due timer or rAF turn:
    // if let Some(wake_at) = tab.next_wake_time() {
    //     sleep_for(wake_at.saturating_sub(tab.now()));
    // }
}
# }
```

See also: [`instructions/live_rendering.md`](../instructions/live_rendering.md).
For a more detailed breakdown of the different `BrowserTab` drivers (`run_event_loop_until_idle`,
`tick_frame`, `run_until_stable`) and how rAF fits in, see
[`docs/live_rendering_loop.md`](live_rendering_loop.md).

## `api::BrowserDocumentJs` (legacy JS host wrapper)

Implementation: [`src/api/browser_document_js.rs`](../src/api/browser_document_js.rs)

`BrowserDocumentJs` is a standalone “JS + document” host container that couples:

- `BrowserDocumentDom2`,
- an `EventLoop<BrowserDocumentJs>` (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`),
- and the legacy vm-js WebIDL runtime (`crate::js::webidl::legacy::VmJsRuntime`).

Current status and guidance:

- It is **independent** of `BrowserTab` (i.e. `BrowserTab` does not use `BrowserDocumentJs`).
- It does **not** implement the HTML `<script>` processing/scheduling model; scripts are executed via
  explicit host calls (e.g. `execute_script_element(...)`).
- Navigation is possible by reaching into the underlying `BrowserDocumentDom2` and calling
  `runtime.document_mut().navigate_url(...)`, but `BrowserDocumentJs` does not provide `BrowserTab`'s
  navigation/history semantics (and does not automatically reset per-document JS state).
- For JS-enabled HTML loading/navigation/rendering, prefer `BrowserTab`.
