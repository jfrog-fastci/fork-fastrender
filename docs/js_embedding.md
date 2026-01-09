# JavaScript embedding in FastRender (developer guide)

This doc is the “how JS works here” guide for contributors:

- how the JS workstream is structured (host vs engine),
- where to add/extend a Web API binding,
- how script execution is *ordered* and *bounded*,
- how to run the JS conformance suite locally.

If you are looking for the spec-mapped `<script>` processing design, start with:
[`docs/html_script_processing.md`](html_script_processing.md).

---

## Safety first (mandatory wrappers)

FastRender runs on hostile inputs. Follow the repo-wide rules in [`AGENTS.md`](../AGENTS.md).

- **All cargo commands:** use `bash scripts/cargo_agent.sh`
- **Any renderer binary execution:** run under OS limits (`scripts/run_limited.sh --as 64G`)

Examples:

```bash
# Build (scoped) under a RAM cap:
bash scripts/cargo_agent.sh build --release

# Run a renderer binary under OS caps:
scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- <args...>
```

Scoped test examples (don’t run unscoped `cargo test`):

```bash
# Run only the library tests in the main crate, filtered to JS-related tests:
bash scripts/cargo_agent.sh test -p fastrender --lib js::event_loop

# Run an xtask integration test (one test binary):
bash scripts/cargo_agent.sh test -p xtask --test js_test262_smoke
```

---

## What the JS-enabled “tab” API is

FastRender’s JS embedding is designed around a **tab-like** host object that owns:

1. a mutable DOM (`dom2::Document`),
2. a JS runtime instance (used by WebIDL bindings and, later, page scripts),
3. an HTML-shaped event loop (tasks + microtasks + timers),
4. (eventually) a `<script>` scheduler that follows the HTML Standard.

### Current state (what exists today)

The public “JS-enabled tab/document runtime” container is:

- `fastrender::api::BrowserDocumentJs` (implementation: `src/api/browser_document_js.rs`)

`BrowserDocumentJs` couples:

- `BrowserDocumentDom2` (live document + multi-frame rendering on top of `dom2`)
- `VmJsRuntime` (WebIDL runtime adapter; **not** a full author-script engine yet)
- `EventLoop<BrowserDocumentJs>` (tasks + microtasks + timers, with explicit budgets)
- `ScriptOrchestrator` + `CurrentScriptState` (`Document.currentScript` bookkeeping)

What it does today:

- provides a single host object that owns the “DOM + JS runtime + event loop” state,
- lets you seed tasks/microtasks/timers and drive them with explicit run limits,
- re-renders automatically when DOM mutations invalidate layout/paint via `run_until_stable(...)`.

What it does **not** do yet:

- it does not execute author `<script>` elements from HTML parsing yet (see
  [`docs/html_script_processing.md`](html_script_processing.md) for the planned integration),
- it does not fetch external scripts,
- it does not yet expose a full `Window`/`Document` Web API surface to JS (bindings are still being
  built out).

### Minimal Rust example (create doc → run loop → render)

This is intentionally minimal and shows the *shape* of the embedding. It does **not** execute author
`<script>` elements from the HTML string yet; instead, you would seed tasks (e.g. “execute this
script” work) before calling `run_until_stable`.

```rust
use fastrender::{BrowserDocumentDom2, RenderOptions};
use fastrender::api::BrowserDocumentJs;
use fastrender::js::{RunLimits, TaskSource};

use std::time::Duration;

fn main() -> fastrender::Result<()> {
    // 1) Create a live document ("tab") from HTML.
    let doc = BrowserDocumentDom2::from_html(
        "<!doctype html><html><body><h1>Hello</h1></body></html>",
        RenderOptions::new().with_viewport(800, 600),
    )?;
    let mut tab = BrowserDocumentJs::new(doc);

    // 2) Optionally seed initial tasks (e.g. "execute this <script>" tasks).
    tab
        .event_loop_mut()
        .queue_task(TaskSource::Script, |_tab, _event_loop| Ok(()))?;

    // 3) Drive the event loop and re-render until no more work remains (bounded).
    let _outcome = tab.run_until_stable(
        RunLimits {
            max_tasks: 1_000,
            max_microtasks: 10_000,
            max_wall_time: Some(Duration::from_millis(50)),
        },
        /* max_frames */ 10,
    )?;

    // 4) Render the final frame.
    //
    // Note: `run_until_stable` renders internally while converging, but does not return a pixmap.
    // Rendering again here reuses cached layout/paint results.
    let pixmap = tab.document_mut().render_frame()?;
    pixmap.save_png("out.png")?;
    Ok(())
}
```

---

## Where the host environment lives

The JS “host environment” is everything ECMAScript expects the embedding to provide: globals, host
hooks, tasks/microtasks, and Web APIs.

### Public embedding surface (`src/api/*`)

The public API types that “own the embedding state” live in `src/api/`:

- `src/api/browser_document_dom2.rs`: `BrowserDocumentDom2` (live `dom2` document + multi-frame rendering)
- `src/api/browser_document_js.rs`: `BrowserDocumentJs` (adds `VmJsRuntime` + `EventLoop` + `run_until_stable`)

### Host-side JS plumbing (`src/js/*`)

Key modules:

- `src/js/event_loop.rs`
  - HTML-shaped `EventLoop`
  - `RunLimits` (max tasks/microtasks/wall-time) + `QueueLimits` (caps queued work)
  - integrates with renderer cancellation via `render_control::check_active(...)`
- `src/js/script_scheduler.rs`
  - `ScriptScheduler` → produces `ScriptSchedulerAction` values (start fetch / block parser / execute now / queue task)
  - `ClassicScriptScheduler` helper that runs classic scripts against an `EventLoop` through a tiny host trait boundary
- `src/js/streaming.rs`
  - parse-time helpers for building `ScriptElementSpec` (base URL + attrs + inline text)
- `src/js/orchestrator.rs`
  - host bookkeeping for `Document.currentScript` (spec-shaped, `dom2`-backed)
- `src/js/window_timers.rs`, `src/js/time.rs`, `src/js/url.rs`
  - early “web platform” primitives used by tests and eventual page execution

### DOM for bindings (`src/dom2/*`)

`dom2` is FastRender’s mutable DOM representation designed for JS bindings. The renderer still
primarily operates on the immutable `crate::dom::DomNode`; `BrowserDocumentDom2` snapshots `dom2`
into that renderer DOM when a new layout is needed.

### Web platform API implementations (`src/web/*`)

This is where host-side Web API behavior should live (separate from renderer internals):

- `src/web/dom/*` (DOM exceptions / helpers)
- `src/web/events/*` (event model foundations)

### WebIDL plumbing (shape extraction + runtime adapter)

FastRender uses WebIDL as the “shape source of truth” for DOM/web APIs.

- `src/webidl/*`
  - `src/webidl/generated/mod.rs` is committed, deterministic IDL metadata (updated via xtask)
- `xtask/src/webidl/*` + `xtask/src/webidl_codegen.rs`
  - extract/resolve/generate the committed snapshot
- `crates/webidl-js-runtime`
  - a small runtime adapter for WebIDL conversions, implemented on top of `ecma-rs`’s `vm-js`

Contributor workflow for updating the committed IDL snapshot:
[`docs/webidl_bindings.md`](webidl_bindings.md).

---

## How script execution is ordered

Script ordering is defined by the WHATWG HTML “script processing model”. The short version:

- parser-inserted classic scripts can block parsing,
- `async` external classic scripts execute when their fetch completes (unordered),
- `defer` external classic scripts execute after parsing completes (in document order),
- after each script executes, run a microtask checkpoint.

FastRender’s implementation scaffolding is split into clean boundaries:

- streaming parser driver: `src/html/pausable_html5ever.rs`
- base URL timing: `src/html/base_url_tracker.rs`
- scheduler actions: `src/js/script_scheduler.rs`
- event loop: `src/js/event_loop.rs`

Details and spec anchors: [`docs/html_script_processing.md`](html_script_processing.md).

---

## How execution is bounded (non-negotiable)

### 1) Process-level caps (OS guardrails)

Use OS address-space caps when running renderer binaries:

```bash
scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- <args...>
```

This is complementary to any in-process limits; it prevents catastrophic OOM behavior.

### 2) Renderer deadlines / cancellation (`RenderDeadline`)

Renderer stages are bounded via `crate::render_control::RenderDeadline` (driven by
`RenderOptions.timeout` and/or `RenderOptions.cancel_callback`).

The JS event loop integrates with these deadlines: `EventLoop::run_until_idle` calls
`render_control::check_active(...)` so tight task loops can be interrupted.

### 3) Event loop budgets

The JS event loop provides explicit budgets:

- **Run limits:** `js::RunLimits { max_tasks, max_microtasks, max_wall_time }`
- **Queue caps:** `js::QueueLimits { max_pending_tasks, max_pending_microtasks, max_pending_timers }`

These prevent untrusted scripts from queueing unbounded work or spinning forever.

### 4) VM memory budgets

The long-term plan is to plumb renderer-level budgets into the JS VM.

What exists today:

- `crates/webidl-js-runtime` builds on `engines/ecma-rs/vm-js`, which supports `HeapLimits`.
- The adapter currently constructs a `Heap` with conservative fixed limits (see
  `crates/webidl-js-runtime/src/ecma_runtime.rs`).

As JS execution is wired into page rendering, these heap limits should become part of the tab-level
budgeting story (alongside renderer stage budgets and OS caps).

---

## Running JS conformance suites

### `bash scripts/cargo_agent.sh xtask js test262` (language semantics)

1) Initialize submodules:

```bash
git submodule update --init engines/ecma-rs
git -C engines/ecma-rs submodule update --init test262-semantic/data
```

2) Run the curated suite (recommended wrapper):

```bash
bash scripts/cargo_agent.sh xtask js test262
```

Notes:

- This repo defines a Cargo alias `xtask = "run -p xtask --"` in `.cargo/config.toml`.
- The suite runner lives in the `engines/ecma-rs` submodule; `xtask` just drives it.
- See [`docs/js_test262.md`](js_test262.md) for flags and interpreting results.

### `bash scripts/cargo_agent.sh xtask js wpt-dom` (Web API behavior)

FastRender includes a minimal offline WPT (`testharness.js`) runner:

- `crates/js-wpt-dom-runner`

Run the full curated corpus under `tests/wpt_dom/tests`:

```bash
bash scripts/cargo_agent.sh xtask js wpt-dom
```

Run only the smoke subset:

```bash
scripts/cargo_agent.sh xtask js wpt-dom --suite smoke --fail-on none
```

By default it writes a JSON report to `target/js/wpt_dom.json` and classifies known gaps via
`tests/wpt_dom/expectations.toml`. Use `--filter` and `--shard` to run smaller subsets while
iterating.

### (Future) `fetch_and_render --js` (page execution)

FastRender’s CLI tools currently render without executing author `<script>` content.

Once JS execution is integrated into the renderer, the intended CLI shape is:

```bash
# planned:
scripts/run_limited.sh --as 64G -- bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- --js <url> out.png
```

Until that lands, use `fetch_and_render` for HTML/CSS rendering only and use `xtask js test262` for
JS language conformance.

---

## Current limitations (be explicit)

The JS workstream is intentionally staged. Today, important missing/unsupported pieces include:

- `BrowserDocumentDom2::from_html(...)` does not execute author `<script>` elements yet (HTML parser
  integration is staged; see [`docs/html_script_processing.md`](html_script_processing.md))
- no module scripts (`type="module"`), no import maps, no dynamic `import()`
- no `document.write()` / parser re-entry
- no CSP/SRI/CORS nuances for scripts
- no full DOM/Web API surface exposed to JS yet (bindings are still being built out)

As you add new behavior, prefer spec-shaped plumbing (HTML/DOM/WebIDL) over ad-hoc shortcuts. The
goal is “correct, bounded subset” rather than “mostly works, unbounded”.
