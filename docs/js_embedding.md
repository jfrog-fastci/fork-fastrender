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
- **Any renderer binary execution:** run under OS limits (`bash scripts/run_limited.sh --as 64G`)

Examples:

```bash
# Build (scoped) under a RAM cap:
bash scripts/cargo_agent.sh build --release

# Run a renderer binary under OS caps:
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- <args...>
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

FastRender currently exposes **two** “tab-like” host containers:

- `fastrender::BrowserTab` (implementation: `src/api/browser_tab.rs`)
  - owns a live `dom2` document via `BrowserDocumentDom2`,
  - owns an HTML-shaped `EventLoop` plus a classic `<script>` scheduler (`ScriptScheduler`),
  - executes classic scripts through a host-supplied `BrowserTabJsExecutor` trait (engine-agnostic).

- `fastrender::api::BrowserDocumentJs` (implementation: `src/api/browser_document_js.rs`)
  - couples `BrowserDocumentDom2` with the `vm-js`-backed `VmJsRuntime` used for WebIDL scaffolding,
  - exposes an `EventLoop<BrowserDocumentJs>` and a `run_until_stable(...)` driver,
  - does **not** (by itself) execute the HTML `<script>` processing model yet.

`BrowserTab` is the intended integration point for “HTML + DOM + JS + rendering”. The executor trait
lets tests stub script execution deterministically today and lets a real JS engine (ecma-rs) plug in
later without changing the public API.

What `BrowserTab` does today:

- for `BrowserTab::from_html(...)` / `BrowserTab::navigate_to_html(...)` / `BrowserTab::navigate_to_url(...)`,
  drives a script-aware streaming parser so **parser-inserted** classic `<script>` elements execute
  at parse time (scripts observe a partially-built DOM),
- fetches external scripts through the document’s `ResourceFetcher`,
- runs microtask checkpoints after script execution,
- rerenders when DOM mutations invalidate layout/paint (`render_if_needed` / `render_frame`).

What it does **not** do yet (important gaps):

- fully spec-correct parser/event-loop interleaving (e.g. “async-ready” scripts interrupting parsing),
- module scripts / import maps,
- a production author-script JS runtime + full DOM/WebIDL exposure (still being built out).

### Minimal Rust example (create doc → run loop → render)

This is intentionally minimal and shows the *shape* of the embedding. It uses a no-op script
executor; real integrations will wire this to a JS engine.

```rust
use fastrender::{BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions, Result};
use fastrender::dom2::NodeId;
use fastrender::js::{EventLoop, ScriptElementSpec};

#[derive(Default)]
struct NoopExecutor;

impl BrowserTabJsExecutor for NoopExecutor {
    fn execute_classic_script(
        &mut self,
        _script_text: &str,
        _spec: &ScriptElementSpec,
        _current_script: Option<NodeId>,
        _document: &mut fastrender::BrowserDocumentDom2,
        _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
        Ok(())
    }
}

fn main() -> Result<()> {
    // 1) Create a tab from HTML.
    let mut tab = BrowserTab::from_html(
        "<!doctype html><html><body><h1>Hello</h1></body></html>",
        RenderOptions::new().with_viewport(800, 600),
        NoopExecutor::default(),
    )?;

    // 2) Drive the event loop + rerender until stable (bounded by default JS limits).
    let _ = tab.run_until_stable(/* max_frames */ 10)?;

    // 3) Render a frame.
    let pixmap = tab.render_frame()?;
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
- `src/api/browser_tab.rs`: `BrowserTab` (script scheduling + event loop + rendering integration)
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
- `src/js/streaming.rs`, `src/js/streaming_dom2.rs`
  - parse-time helpers for building `ScriptElementSpec` (base URL timing + attrs + inline text)
- `src/js/orchestrator.rs`
  - host bookkeeping for `Document.currentScript` (spec-shaped, `dom2`-backed)
- `src/js/window_timers.rs`, `src/js/window_animation_frame.rs`, `src/js/time.rs`, `src/js/url.rs`
  - early “web platform” primitives used by tests and eventual page execution

#### Promise jobs and microtasks (`vm-js` host hooks)

`vm-js` models ECMAScript host hooks (e.g. `HostEnqueuePromiseJob`) via the `VmHostHooks` trait.
FastRender implements these hooks by routing Promise jobs into the host-owned HTML-like
`EventLoop` microtask queue:

- `src/js/window_timers.rs`: `VmJsEventLoopHooks` implements `VmHostHooks::host_enqueue_promise_job`
  by queueing an `EventLoop` microtask that runs the `vm-js::Job`.

Script execution that needs correct Promise/microtask behavior must ensure Promise jobs are routed
through *host hooks* instead of the VM-owned microtask queue.

Historically this doc recommended the hook-only `vm-js` entry points:
`exec_script_with_hooks(...)` / `exec_script_source_with_hooks(...)`. These route Promise jobs
through `VmHostHooks`, but they execute with a **dummy** `VmHost` (`()`), so native bindings cannot
downcast the embedder host context to reach embedding state (document/window/tab/etc.). This makes
them unsuitable for real Web API bindings.

`vm-js` now provides **host+hooks** entry points on `vm_js::JsRuntime`:
`exec_script_with_host_and_hooks(...)` / `exec_script_source_with_host_and_hooks(...)`.
These take both:

- an embedder host context (`&mut dyn VmHost`) that native bindings can downcast, and
- a hook implementation (`&mut dyn VmHostHooks`) so Promise jobs are enqueued onto the embedding’s
  microtask queue.

FastRender embeddings should prefer the host+hooks APIs so:

- scripts run with correct microtask semantics (HTML-style microtask queue), **and**
- Web API native bindings can access embedder state via `VmHost` without global registries.

Migration strategy: thread a single “host context” object (e.g. `WindowHostState` /
`BrowserTabHost`) through **both** script execution *and* Promise-job execution contexts so that
native callbacks invoked later (from microtasks/timers/etc.) still see the same embedder state.

Concretely, this means host-side script execution entry points (e.g. `WindowHost::exec_script`,
`WindowHostState::{exec_script_in_event_loop, exec_script_with_name_in_event_loop}`, and the
event-loop’s Promise job runner) should eventually call `vm-js` with the same host context object
instead of the hook-only `exec_script_*_with_hooks(...)` path.

> **Why not TLS?** FastRender historically used thread-local registries/stacks to smuggle embedding
> state into native bindings (e.g. `DOM_SOURCES` in `src/js/window_realm.rs` and `EVENT_LOOP_STACK`
> in `src/js/runtime.rs`). These were pragmatic stopgaps while `vm-js` lacked an ergonomic way to
> pass embedding state into both script and job execution. The long-term goal is to delete these
> TLS workarounds and rely on explicit `VmHost` plumbing.

This keeps Promise jobs and `queueMicrotask(...)` in the same FIFO-ordered microtask queue, and
ensures Promise jobs enqueued by other Promise jobs run in the same microtask checkpoint.

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
- `xtask/src/webidl_bindings_codegen.rs` (`cargo xtask webidl-bindings`)
  - generates committed Rust glue under `src/js/bindings/` from the snapshot world:
    - `src/js/bindings/generated/mod.rs` (Window-facing bindings wrappers)
    - `src/js/bindings/dom_generated.rs` (temporary `vm-js` DOM scaffold; controlled by
      `tools/webidl/bindings_allowlist.toml`)
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
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- <args...>
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
- **Queue caps:** `js::QueueLimits { max_pending_tasks, max_pending_microtasks, max_pending_timers, max_pending_animation_frame_callbacks }`

These prevent untrusted scripts from queueing unbounded work or spinning forever.

### 4) VM memory budgets

The long-term plan is to plumb renderer-level budgets into the JS VM.

What exists today:

- `crates/webidl-js-runtime` builds on `vendor/ecma-rs/vm-js`, which supports `HeapLimits`.
- The adapter currently constructs a `Heap` with conservative fixed limits (see
  `crates/webidl-js-runtime/src/ecma_runtime.rs`).

As JS execution is wired into page rendering, these heap limits should become part of the tab-level
budgeting story (alongside renderer stage budgets and OS caps).

---

## Running JS conformance suites

### `bash scripts/cargo_agent.sh xtask js test262` (language semantics)

1) Initialize submodules:

```bash
git submodule update --init vendor/ecma-rs/test262-semantic/data
```

2) Run the curated suite (recommended wrapper):

```bash
bash scripts/cargo_agent.sh xtask js test262
```

Notes:

- `bash scripts/cargo_agent.sh xtask ...` uses Cargo aliases (see `.cargo/config.toml`).
- The suite runner lives in the vendored `vendor/ecma-rs`; `xtask` just drives it.
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
bash scripts/cargo_agent.sh xtask js wpt-dom --suite smoke --fail-on none
```

By default it writes a JSON report to `target/js/wpt_dom.json` and classifies known gaps via
`tests/wpt_dom/expectations.toml`. Use `--filter` and `--shard` to run smaller subsets while
iterating.

### `fetch_and_render --js` (experimental page execution)

`fetch_and_render` can optionally execute a **best-effort subset** of author `<script>` elements when
`--js` is provided.

Current behavior (intentionally limited / not spec-correct):

- Executes **inline classic scripts** discovered by scanning the fully-parsed DOM in document order.
  - This is not the HTML script processing model (no parser pausing, async/defer ordering, base URL
    timing, etc.).
- Does **not** fetch external scripts (`<script src=...>` is skipped).
- Exposes a minimal `window` realm (`window`/`self`/`document`/`location`) and a small DOM shim
  surface used by real pages (for example: `document.documentElement.className`).
- Runs scripts under the renderer's JS execution budgets (`JsExecutionArgs` + render deadlines).

Example:

```bash
bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- --js <url> out.png
```

For spec-correct script processing and richer Web APIs, use the library embedding (`BrowserTab`) and
the parse-time script scheduler described in [`docs/html_script_processing.md`](html_script_processing.md).

---

## Current limitations (be explicit)

The JS workstream is intentionally staged. Today, important missing/unsupported pieces include:

- `BrowserDocumentDom2::from_html(...)` does not execute author `<script>` elements by itself (script
  execution is hosted by `BrowserTab`; see [`docs/html_script_processing.md`](html_script_processing.md))
- no module scripts (`type="module"`), no import maps, no dynamic `import()`
- no `document.write()` / parser re-entry
- no CSP/SRI/CORS nuances for scripts
- no full DOM/Web API surface exposed to JS yet (bindings are still being built out)

As you add new behavior, prefer spec-shaped plumbing (HTML/DOM/WebIDL) over ad-hoc shortcuts. The
goal is “correct, bounded subset” rather than “mostly works, unbounded”.
