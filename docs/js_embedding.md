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

- **All cargo commands:** use `bash scripts/cargo_agent.sh`, and always wrap with `timeout -k`
  (tests/builds can hang).
- **Any renderer binary execution:** run under OS limits (`bash scripts/run_limited.sh --as 64G -- ...`)
  and wrap with `timeout -k` (pages/scripts can hang).

Examples:

```bash
# Build (scoped) under a RAM cap:
timeout -k 10 600 bash scripts/cargo_agent.sh build --release

# Run a renderer binary under OS caps:
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- <args...>
```

Scoped test examples (don’t run unscoped `cargo test`):

```bash
# Run only the library tests in the main crate, filtered to JS-related tests:
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::event_loop

# Run an xtask integration test (one test binary):
timeout -k 10 600 bash scripts/cargo_agent.sh test -p xtask --test js_test262_smoke
```

---

## What the JS-enabled “tab” API is

FastRender’s JS embedding is designed around a **tab-like** host object that owns:

1. a mutable DOM (`dom2::Document`),
2. a JS runtime instance (used by WebIDL bindings and, later, page scripts),
3. an HTML-shaped event loop (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`),
4. a `<script>` scheduler that follows the HTML Standard (classic + module + import maps).

If you’re deciding between the various public “document/tab” containers (which ones have JS, event
loop, navigation, etc), see [`docs/runtime_stacks.md`](runtime_stacks.md).

### Current state (what exists today)

FastRender currently exposes **two** “tab-like” host containers:

- `fastrender::BrowserTab` (implementation: `src/api/browser_tab.rs`)
  - owns a live `dom2` document via `BrowserDocumentDom2`,
  - owns an HTML-shaped `EventLoop` plus an HTML-like `<script>` scheduler (`HtmlScriptScheduler`),
  - executes classic + module scripts through a host-supplied `BrowserTabJsExecutor` trait (engine-agnostic),
    with the production `vm-js` implementation in `src/api/browser_tab_vm_js_executor.rs`
    (`VmJsBrowserTabExecutor`).

- `fastrender::api::BrowserDocumentJs` (implementation: `src/api/browser_document_js.rs`)
  - couples `BrowserDocumentDom2` with the `vm-js`-backed `VmJsRuntime` used for WebIDL scaffolding,
  - exposes an `EventLoop<BrowserDocumentJs>` (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`) and a `run_until_stable(...)` driver,
  - does **not** (by itself) execute the HTML `<script>` processing model yet.

`BrowserTab` is the intended integration point for “HTML + DOM + JS + rendering”. The executor trait
lets tests stub script execution deterministically today and lets a real JS engine (ecma-rs) plug in
later without changing the public API.

What `BrowserTab` does today:

- for `BrowserTab::from_html(...)` / `BrowserTab::navigate_to_html(...)` / `BrowserTab::navigate_to_url(...)`,
  drives a script-aware streaming parser so **parser-inserted** classic `<script>` elements execute
  at parse time (scripts observe a partially-built DOM). Parsing runs in bounded slices and yields
  back to the event loop based on `JsExecutionOptions.dom_parse_budget`. In addition to
  `max_pump_iterations`, `ParseBudget` supports `max_input_bytes_per_task` to cap the amount of HTML
  (UTF-8 bytes) fed into the streaming parser per task, which makes yields more predictable on
  inputs like a single very-large text chunk. This lets “as soon as possible” scripts (`async` /
  ordered-asap modules) interleave ahead of later parser work,
- fetches external scripts through the document’s `ResourceFetcher`,
- executes classic scripts plus module/import-map scripts when `JsExecutionOptions.supports_module_scripts`
  is enabled (including dynamic `import()` and top-level `await`),
- runs microtask checkpoints after script execution,
- rerenders when DOM mutations invalidate layout/paint (`render_if_needed` / `render_frame`).

What it does **not** do yet (important gaps):

- fully spec-correct HTML script processing in all edge cases (task-source partitioning, preload
  scanning, full Fetch/CORS/SRI nuance, etc.), though `BrowserTab` now parses under a per-task
  `ParseBudget` (`JsExecutionOptions.dom_parse_budget`) so async-ready work can interleave with
  parsing (see `max_input_bytes_per_task` if you need more predictable yields based on consumed
  input, not just pump iteration count),
- full Web platform coverage: the DOM/WebIDL/Web API surface exposed to JS is still a subset.

Module support status:

- `<script type="module">`, import maps, dynamic `import()`, and top-level await are supported by the
  production `vm-js` executor when module loading is enabled via
  `JsExecutionOptions { supports_module_scripts: true, .. }`.
- When module loading is disabled (the default `JsExecutionOptions`), dynamic `import()` rejects with
  a `TypeError` and module scripts are skipped/treated as non-executable.

### Privileged chrome UI realms (renderer-chrome)

The JS embedding layer also supports the idea of a **trusted “chrome UI” realm** (for renderer-chrome:
browser UI rendered by FastRender in the browser process). Those realms may be granted additional
capabilities via a privileged `globalThis.chrome` object.

This privileged bridge must **never** be installed in untrusted content realms. See
[`docs/chrome_js_bridge.md`](chrome_js_bridge.md) for the API surface and installation model.

Renderer-chrome also reserves privileged internal URL schemes (`chrome://` assets and
`chrome-action:` actions); see [`docs/renderer_chrome_schemes.md`](renderer_chrome_schemes.md).

### Minimal Rust example (create doc → run loop → render)

This example uses the production `vm-js` runtime via the convenience constructors on `BrowserTab`
(no custom executor required):

```rust,no_run
use fastrender::{BrowserTab, RenderOptions, Result};

fn main() -> Result<()> {
    // 1) Create a vm-js-backed tab from HTML.
    let html = r#"<!doctype html>
        <html><body>
          <script>
            document.body.setAttribute("data-ok", "1");
          </script>
          <h1>Hello</h1>
        </body></html>"#;

    let mut tab = BrowserTab::from_html_with_vmjs(
        html,
        RenderOptions::new().with_viewport(800, 600),
    )?;

  // 2) Drive the JS event loop until stable (bounded by default JS budgets).
  let _ = tab.run_until_stable(/* max_frames */ 10)?;

  // 3) Render a frame.
  let pixmap = tab.render_frame()?;
  pixmap.save_png("out.png")?;
  Ok(())
}
```

If you are embedding FastRender in a **live / interactive** setting (continuous event-driven loop),
see [`docs/live_rendering_loop.md`](live_rendering_loop.md) for:

- `run_event_loop_until_idle` (tasks/microtasks/timers + `requestIdleCallback`; no rAF; no render),
- `tick_frame` (step-wise; returns a `Pixmap` when pixels change),
- `run_until_stable` (deterministic convergence: drains tasks/microtasks/timers/idle callbacks + rAF + renders),
- how to wake a sleeping host when background threads queue work via `ExternalTaskQueueHandle`,
- and why `requestAnimationFrame` callbacks run on the **frame schedule**, not during
  `run_event_loop_until_idle`.

---

## Where the host environment lives

The JS “host environment” is everything ECMAScript expects the embedding to provide: globals, host
hooks, task/microtask/timer scheduling, and Web APIs.

### Public embedding surface (`src/api/*`)

The public API types that “own the embedding state” live in `src/api/`:

- `src/api/browser_document_dom2.rs`: `BrowserDocumentDom2` (live `dom2` document + multi-frame rendering)
- `src/api/browser_tab.rs`: `BrowserTab` (script scheduling + event loop + rendering integration)
- `src/api/browser_tab_vm_js_executor.rs`: `VmJsBrowserTabExecutor` (production `vm-js` executor for `BrowserTab`)
- `src/api/browser_document_js.rs`: `BrowserDocumentJs` (adds `VmJsRuntime` + `EventLoop` + `run_until_stable`)

### Host-side JS plumbing (`src/js/*`)

Key modules:

- `src/js/event_loop.rs`
  - HTML-shaped `EventLoop`
  - `RunLimits` (max tasks/microtasks/wall-time) + `QueueLimits` (caps queued work)
  - integrates with renderer cancellation via `render_control::check_active(...)`
- `src/js/html_script_scheduler.rs`
  - `HtmlScriptScheduler` → produces `HtmlScriptSchedulerAction` values (start classic fetch / start module graph fetch /
    block parser / execute now / queue task / queue script event task)
  - Supports classic scripts, module scripts, and import maps. In production, `BrowserTabHost`
    (`src/api/browser_tab.rs`) drives the scheduler by interpreting actions in
    `BrowserTabHost::apply_scheduler_actions(...)` (start fetches, block/resume parsing, execute
    scripts, queue event-loop tasks, dispatch `load`/`error` events, etc).
- `src/js/html_script_pipeline.rs`
  - test harness / lightweight orchestrator that connects `StreamingHtmlParser` yields to `HtmlScriptScheduler` actions
- `src/js/streaming.rs`, `src/js/streaming_dom2.rs`
  - parse-time helpers for building `ScriptElementSpec` (base URL timing + attrs + inline text)
- `src/js/import_maps/`
  - WHATWG HTML import map parsing/normalization (`parse_import_map_string`, `create_import_map_parse_result`)
  - host-side state + merging + resolution (`ImportMapState`, `register_import_map`, `resolve_module_specifier`, ...)
  - `resolve_imports_match` helper (throws `ImportMapError` for blocked cases)
  - see [`docs/import_maps.md`](import_maps.md)
- `src/js/orchestrator.rs`
  - host bookkeeping for `Document.currentScript` (spec-shaped, `dom2`-backed)
- `src/js/document_lifecycle.rs`
  - `DocumentLifecycle` state machine (`document.readyState`, `DOMContentLoaded`, `load`)
- `src/js/vmjs/window_timers.rs`, `src/js/vmjs/window_animation_frame.rs`, `src/js/time.rs`, `src/js/url.rs`
  - early “web platform” primitives used by tests and eventual page execution

#### Promise jobs and microtasks (`vm-js` host hooks)

`vm-js` models ECMAScript host hooks (e.g. `HostEnqueuePromiseJob`) via the `VmHostHooks` trait
(`vendor/ecma-rs/vm-js/src/jobs.rs`).
FastRender implements these hooks by routing Promise jobs into the host-owned HTML-like
`EventLoop` microtask queue:

- `src/js/vmjs/window_timers.rs`: `VmJsEventLoopHooks` implements `VmHostHooks::host_enqueue_promise_job`
  by queueing an `EventLoop` microtask that runs the `vm-js::Job`.

`VmJsEventLoopHooks` is also where module loading hooks live. `vm-js` routes both static module
loading and dynamic `import()` through the embedder’s `VmHostHooks`; FastRender’s implementation
bridges those requests into the per-realm module loader (import maps + fetch) while preserving the
HTML-like task/microtask model.

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
> state into native bindings (e.g. `DOM_SOURCES` in `src/js/vmjs/window_realm.rs`, and previously
> `EVENT_LOOP_STACK` in `src/js/vmjs/runtime.rs`). These were pragmatic stopgaps while `vm-js` lacked
> an ergonomic way to pass embedding state into both script and job execution.
>
> The event-loop “current event loop” mechanism used to be implemented via TLS
> (`EVENT_LOOP_STACK` in `src/js/vmjs/runtime.rs`), but this has been removed: the active
> `EventLoop<Host>` is now threaded explicitly through the vm-js boundary via the hook payload
> (`webidl_vm_js::VmJsHostHooksPayload::set_event_loop`).

This keeps Promise jobs and `queueMicrotask(...)` in the same FIFO-ordered microtask queue, and
ensures Promise jobs enqueued by other Promise jobs run in the same microtask checkpoint.

#### Module scripts, top-level `await`, and completion callbacks

Module evaluation can be **asynchronous** because of top-level `await`. To model this, the HTML
integration (`BrowserTabHost`) treats module `<script>` execution as a two-phase operation:

- `BrowserTabJsExecutor::execute_module_script(...)` returns
  `ModuleScriptExecutionStatus::{Completed, Pending}` (defined in `src/api/browser_tab.rs`).
- If `Pending` is returned, the host keeps ordered module queues blocked until the executor later
  reports completion.

Completion is reported back into the HTML-like event loop via an explicit task:
`BrowserTabHost::on_module_script_evaluation_complete(...)` (`src/api/browser_tab.rs`).

The production `vm-js` executor wires this up in `src/api/browser_tab_vm_js_executor.rs` by:

1. Storing the module’s evaluation Promise when evaluation returns `Pending`.
2. Polling pending evaluation promises from `BrowserTabJsExecutor::after_microtask_checkpoint(...)`
   (which `BrowserTabHost` calls after draining microtasks).
3. When a promise settles, queueing a `TaskSource::Script` task that invokes
   `on_module_script_evaluation_complete(...)` so the host can:
   - dispatch `<script>` `load`/`error` events, and
   - unblock ordered module execution (including dynamic `async=false` insertion ordering).

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

For the consolidated WebIDL crate layout and ownership boundaries (what belongs in `vendor/ecma-rs/`
vs `src/js/`), see [`docs/webidl_stack.md`](webidl_stack.md).

- `src/webidl/*`
  - `src/webidl/generated/mod.rs` is committed, deterministic IDL metadata (updated via xtask)
- `xtask/src/webidl/*` + `xtask/src/webidl_codegen.rs`
  - extract/resolve/generate the committed snapshot
- `xtask/src/webidl_bindings_codegen.rs` (`timeout -k 10 600 bash scripts/cargo_agent.sh xtask webidl-bindings`)
  - generates committed Rust glue from the snapshot world:
    - `src/js/webidl/bindings/generated/mod.rs` (`vm-js` realm WebIDL bindings; default backend `vmjs`)
    - `src/js/webidl/bindings/generated_legacy.rs` (legacy heap-only runtime bindings wrappers; backend `legacy`)
    - `src/js/legacy/dom_generated.rs` (deprecated `VmJsRuntime` DOM scaffold; backend `legacy`)
    - (DOM bindings for the current `vm-js` realm are not generated by this xtask; they are
      implemented directly in `src/js/legacy/vm_dom.rs`)

Note: in the consolidated layout, the only FastRender-local crate under `crates/` should be
`crates/js-wpt-dom-runner` (tooling). All generic JS/WebIDL infrastructure should live in
`vendor/ecma-rs/`.

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
- scheduler actions: `src/js/html_script_scheduler.rs`
- event loop: `src/js/event_loop.rs`

Details and spec anchors: [`docs/html_script_processing.md`](html_script_processing.md).

---

## How execution is bounded (non-negotiable)

### 1) Process-level caps (OS guardrails)

Use OS address-space caps when running renderer binaries:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
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

- The legacy heap-only WebIDL runtime adapter (`vendor/ecma-rs/webidl-runtime`, Cargo package
  `webidl-js-runtime`, library crate name `webidl_runtime`, imported as `webidl_js_runtime` in
  FastRender) builds on `vm-js`, which supports `HeapLimits`.
- The runtime constructs a `Heap` with conservative fixed limits by default (see
  `VmJsRuntime::new` / `VmJsRuntime::with_limits` in
  `vendor/ecma-rs/webidl-runtime/src/ecma_runtime.rs`).

As JS execution is wired into page rendering, these heap limits should become part of the tab-level
budgeting story (alongside renderer stage budgets and OS caps).

---

## Running JS conformance suites

### `timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262` (language semantics)

1) Initialize submodules:

```bash
git submodule update --init vendor/ecma-rs/test262-semantic/data
```

2) Run the curated suite (recommended wrapper):

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262
```

Notes:

- `scripts/cargo_agent.sh xtask ...` uses Cargo aliases (see `.cargo/config.toml`). Still wrap with
  `timeout -k`.
- The suite runner lives in the vendored `vendor/ecma-rs`; `xtask` just drives it.
- See [`docs/js_test262.md`](js_test262.md) for flags and interpreting results.

### `timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom` (Web API behavior)

FastRender includes a minimal offline WPT (`testharness.js`) runner:

- `crates/js-wpt-dom-runner`

Run the full curated corpus under `tests/wpt_dom/tests`:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom
```

Run only the smoke subset:

```bash
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js wpt-dom --suite smoke --fail-on none
```

By default it writes a JSON report to `target/js/wpt_dom.json` and classifies known gaps via
`tests/wpt_dom/expectations.toml`. Use `--filter` and `--shard` to run smaller subsets while
iterating.

### `fetch_and_render --js` (experimental page execution)

`fetch_and_render` can optionally execute author `<script>` elements when `--js` is provided.

Current behavior (still experimental / not fully spec-correct):

- Uses the `vm-js`-backed `BrowserTab` runtime (same embedding surface as library consumers).
- Drives the script-aware streaming parser so **parser-inserted** scripts run at `</script>`
  boundaries against a partially-built DOM.
- Fetches and executes external classic scripts (`<script src=...>`) and module scripts (including
  static import graphs, dynamic `import()`, and top-level `await`).
- Supports **inline** `<script type="importmap">` and applies import maps for module specifier
  resolution.
- Runs scripts under the renderer’s JS execution budgets (`JsExecutionArgs`) and cooperative render
  deadlines.

Example:

```bash
timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
  bash scripts/cargo_agent.sh run --release --bin fetch_and_render -- --js <url> out.png
```

For spec-correct script processing and richer Web APIs, use the library embedding (`BrowserTab`) and
the parse-time script scheduler described in [`docs/html_script_processing.md`](html_script_processing.md).

---

## Current limitations (be explicit)

The JS workstream is intentionally staged. Today, important missing/unsupported pieces include:

- `BrowserDocumentDom2::from_html(...)` does not execute author `<script>` elements by itself (script
  execution is hosted by `BrowserTab`; see [`docs/html_script_processing.md`](html_script_processing.md))
- module scripts/import maps/dynamic `import()`/top-level await are opt-in (disabled in
  `JsExecutionOptions::default` for hostile-input safety). Enable module loading via
  `JsExecutionOptions { supports_module_scripts: true, .. }` when you want modern module behavior.
- external import maps (`<script type="importmap" src=...>`) are intentionally unsupported; only
  inline `<script type="importmap">` is supported (see [`docs/import_maps.md`](import_maps.md))
- `document.write()` support is limited:
  - it can inject into an active streaming parse (parser re-entry) for parser-blocking scripts
    executed during `BrowserTab`'s streaming HTML parse,
  - it is treated as a no-op when no streaming parser is active (deterministic subset; no destructive
    post-load writes / implicit `document.open()`).
- CSP/CORS/SRI support exists for common cases, but is still conservative/incomplete (notably CSP
  `strict-dynamic` trust propagation, and full Fetch mode/credentials nuance).
- no full DOM/Web API surface exposed to JS yet (bindings are still being built out)

As you add new behavior, prefer spec-shaped plumbing (HTML/DOM/WebIDL) over ad-hoc shortcuts. The
goal is “correct, bounded subset” rather than “mostly works, unbounded”.
