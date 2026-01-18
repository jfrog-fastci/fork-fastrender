# HTML `<script>` processing + parser integration (classic + module + import maps)

## Purpose
FastRender’s JavaScript support needs to follow the WHATWG HTML **script processing model** so that:

1. Scripts can run **during parsing** (observing a partially-built DOM).
2. `async` / `defer` ordering matches browser behavior.
3. Script execution is integrated with an HTML-shaped **event loop** (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`).
4. Relative `src` URLs resolve against the **base URL in effect at script preparation time**.

This document is a spec-mapped design for that integration. It is written to prevent future
implementers from having to “rediscover” scattered HTML Standard details when extending and refining
support for module scripts and end-to-end import map integration.

If you’re trying to pick the right public API container (document vs tab, JS vs no JS), start with:
[`docs/runtime_stacks.md`](runtime_stacks.md).

## Status in this repository (reality check)
FastRender has an end-to-end, spec-shaped, streaming `<script>` pipeline for **classic scripts**,
**module scripts**, and **import maps**. In the production `vm-js` embedding (`api::BrowserTab` +
`VmJsBrowserTabExecutor`), module scripts support **dynamic `import()`** and **top-level await**
(`ModuleScriptExecutionStatus::Pending`), with evaluation progress integrated into FastRender’s
HTML-like `EventLoop` + microtask checkpoints.

Note: module scripts are opt-in for hostile-input safety. The default `JsExecutionOptions` disables
module loading; enable it with `JsExecutionOptions { supports_module_scripts: true, .. }` when you
want `<script type="module">` / `import()` / import maps.

There is now an end-to-end “tab” integration point (`api::BrowserTab`) that ties together the live
`dom2` document, classic script scheduling, an HTML-shaped event loop, script-blocking stylesheet
tracking, and rendering invalidation.
When loading HTML strings (`BrowserTab::from_html` / `BrowserTab::navigate_to_html`), it uses the
script-aware streaming parser (`StreamingHtmlParser`) so parser-inserted scripts execute at `</script>`
boundaries against a partially-built DOM. URL navigations (`BrowserTab::navigate_to_url`) now use the
same streaming parser driver so parser-inserted classic scripts execute during parsing (instead of
best-effort post-parse `<script>` discovery).

In production, parsing is driven from the HTML-like `EventLoop` in **bounded slices**:
`BrowserTabHost::parse_until_blocked(...)` pumps the `StreamingHtmlParser` a limited number of times
per task (configured by `JsExecutionOptions.dom_parse_budget`). When the budget is exhausted it
snapshots the current parser DOM into the host document and yields back to the event loop. This is
how “as soon as possible” scripts (`async` / ordered-asap modules) can run before parsing continues
in a single-threaded integration.

What exists today (in-tree):

- **HTML parsing hooks (pause at `</script>`):**
  - `src/html/pausable_html5ever.rs`: wraps html5ever so the host can observe
    `TokenizerResult::Script` suspension points (html5ever’s built-in driver currently loops past
    them).
  - `src/html/streaming_parser.rs`: streaming parser driver that builds a live `dom2::Document`,
    pauses at parser-inserted `</script>` boundaries, supports `document.write`-style input
    injection, and tracks the parse-time base URL.
  - (Legacy/testing utility) `src/dom/scripting_parser.rs`: `parse_html_with_scripting(...)` pauses
    at `</script>` boundaries and yields a `ScriptToken` plus a partial DOM snapshot (backed by
    `markup5ever_rcdom`).
- **Parse-time base URL tracking:**
  - `src/html/base_url_tracker.rs`: `BaseUrlTracker` tracks `<base href>` as the parser progresses
    so `<script src>` resolution uses the base URL *at script preparation time*.
- **Script element normalization at parse time:**
  - `src/js/mod.rs`: `ScriptType` + `ScriptElementSpec` (flattened `<script>` record).
  - `src/js/streaming.rs`, `src/js/streaming_dom2.rs`: helpers for building `ScriptElementSpec` at the
    moment a `<script>` finishes parsing.
- **Import maps algorithms + integration:**
  - `src/js/import_maps/`: spec-mapped import map parsing + state + merging + resolution:
    - parsing/normalization: `parse_import_map_string(...)` + `create_import_map_parse_result(...)` (`parse.rs`)
    - host-side state: `ImportMapState` + resolved-module-set record types (`types.rs`)
    - registration + merging: `register_import_map(...)` / `merge_existing_and_new_import_maps(...)` (`merge.rs`)
    - full module specifier resolution: `resolve_module_specifier(...)` + `add_module_to_resolved_module_set(...)` (`resolve.rs`)
    - helper: `resolve_imports_match(...)` (`resolve.rs`) — implements the spec’s "resolve an imports match"
      algorithm and throws `ImportMapError` for blocked cases
    - Import maps are integrated into:
      - `BrowserTab`'s streaming `<script>` pipeline (via `HtmlScriptScheduler` +
        `BrowserTabJsExecutor::execute_import_map_script`), and
      - tooling module execution (`fetch_and_render --js`, `VmJsModuleLoader` in
        `src/js/vmjs/module_loader.rs`).
  - Design/spec mapping: [`docs/import_maps.md`](import_maps.md).
- **Script scheduling + event loop:**
  - `src/js/html_script_scheduler.rs`: HTML `<script>` ordering (parser-blocking vs `async` vs
    `defer`, plus module scripts and import maps) implemented as an action-based scheduler
    (`HtmlScriptSchedulerAction`).
  - `src/js/html_script_pipeline.rs`: lightweight orchestrator that connects `StreamingHtmlParser`
    yields to `HtmlScriptScheduler` actions (used by unit tests and some harnesses).
  - `src/js/event_loop.rs`: task + microtask queues, explicit microtask checkpoints, timers, run
    limits (`RunLimits`), and queue caps (`QueueLimits`).
  - `src/js/script_blocking_stylesheets.rs`: `ScriptBlockingStyleSheetSet` used by `BrowserTab` to
    delay parser-blocking scripts until render-blocking stylesheets finish loading.
- **Host-side execution bookkeeping:**
  - `src/js/orchestrator.rs`: host-side `Document.currentScript` bookkeeping around “execute the
     script block” (classic scripts).
- **JS-enabled host container (early embedding surface):**
  - `src/api/browser_tab.rs`: `BrowserTab` couples `BrowserDocumentDom2` + `EventLoop` +
     `HtmlScriptScheduler` + `ScriptOrchestrator` and re-renders after DOM mutations. For HTML-string
     loads and URL navigations, it drives `StreamingHtmlParser` so parser-inserted scripts execute
    during parsing.
  - `src/api/browser_tab_vm_js_executor.rs`: `VmJsBrowserTabExecutor` implements
    `BrowserTabJsExecutor` using `vm-js` and provides the real window/document environment used by
    `BrowserTab::from_html_with_vmjs*` constructors (including module loading, Promise jobs, and
    timers).
  - `src/api/browser_document_js.rs`: `BrowserDocumentJs` couples a live `dom2` document, a JS
    runtime adapter, an HTML-shaped `EventLoop`, and `currentScript` bookkeeping.
- **Document lifecycle (`readyState`, `DOMContentLoaded`, `load`):**
  - `src/js/document_lifecycle.rs`: `DocumentLifecycle` state machine + scheduling helpers.
  - `src/api/browser_tab.rs`: integrates lifecycle gates with deferred scripts + load blockers
    (scripts + render-blocking stylesheets) and dispatches events via the active executor.
- **Mutable DOM for bindings (`dom2`):**
  - `src/dom2/`: mutable DOM (`dom2::Document`) intended for JS bindings and script-visible
    mutations.
  - `src/dom2/html5ever_tree_sink.rs`: `dom2::Dom2TreeSink` (`html5ever::TreeSink`) implementation
    that incrementally builds `dom2::Document` during parsing (and wires in parse-time
    `<base href>` tracking via `BaseUrlTracker`).
  - `src/dom2/import.rs`: bridge for constructing `dom2::Document` from the renderer’s immutable
    `crate::dom::DomNode` (useful for incremental adoption / existing pipelines).
- **End-to-end harness (not a full HTML parser):**
  - `src/js/html_scripting.rs`: a small harness used by unit tests to exercise script/style
    interaction and event loop semantics (Task 129).
- **Legacy tooling (deprecated for execution):**
  - `src/js/dom_scripts.rs::extract_script_elements()`: post-parse DOM scanning for tooling only
    (not spec-correct for execution).
- **`vm-js` host hooks (Promise jobs + module loading):**
  - `src/js/vmjs/window_timers.rs`: `VmJsEventLoopHooks` implements `vm-js` host hooks by routing:
    - Promise jobs into FastRender’s `EventLoop` microtask queue, and
    - module loading / dynamic `import()` requests through the per-realm module loader.

### How to run tests
The relevant unit tests live in the `fastrender` crate’s `--lib` test binary. Run them (scoped) with:

`timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib`

Some end-to-end scheduling/currentScript coverage lives in integration tests (example):

`timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration js_current_script`

---

## What we implement (core behaviors)
This section describes the core script execution model implemented by `HtmlScriptScheduler` and the
`BrowserTab` streaming pipeline.

### 1) Parser-inserted classic scripts
- `<script>` elements encountered by the HTML parser are treated as **parser-inserted**.
- Inline classic scripts execute **synchronously** when their end tag (`</script>`) is seen.
- External classic scripts (`src=...`) execute using the ordering rules below.

### 2) `async` / `defer` ordering (classic scripts)
For **external classic scripts**:

- **No `async`, no `defer` (parser-inserted external classic script):**
  - Parsing is **blocked** until the script is fetched + executed.
- **`async` present:**
  - Fetch in parallel with parsing; execute when ready, independent of parser progress.
- **`defer` present and `async` absent:**
  - Fetch in parallel with parsing; execute **after parsing completes**, in document order.

For **inline classic scripts**, `async`/`defer` are effectively ignored because the content is
already available; they execute when encountered.

### 3) Stylesheet-blocking scripts (render-blocking stylesheets)
HTML requires that certain scripts delay execution until all render-blocking stylesheets have
loaded, so scripts observe the correct computed styles.

FastRender implements an MVP subset for streaming parsing via `api::BrowserTab`:

- Parser-blocking scripts (inline classic scripts, and external classic scripts without `async` or
  `defer`) wait for the current `ScriptBlockingStyleSheetSet` to become empty before executing.
- Scripts in the HTML “list of scripts that will execute when the document has finished parsing”
  (classic external `defer` scripts, and parser-inserted non-`async` module scripts) also wait until
  the `ScriptBlockingStyleSheetSet` is empty before their execution tasks are queued.
- `async` scripts are **not** delayed by script-blocking stylesheets.
- Stylesheets in inert `<template>` contents do not register as script-blocking.

See:
- `src/js/script_blocking_stylesheets.rs`
- `src/api/browser_tab.rs` unit tests: `script_blocking_*`

### 4) Microtask checkpoints (Promises/jobs)
After running a script, run a **microtask checkpoint** (drain the microtask queue) **only if the
JavaScript execution context stack is empty** (HTML “clean up after running script”).

In practice:
- For top-level script execution, the JS stack becomes empty immediately after the script returns,
  so microtasks are drained right away.
- For nested/re-entrant script execution (e.g. event dispatch while a script is still on the stack),
  microtasks must not be drained until the outermost script returns.

In code, this maps to `src/js/event_loop.rs`:
- `EventLoop::run_next_task()` always follows a task with a checkpoint.
- Parser-driven synchronous execution must explicitly call
  `EventLoop::perform_microtask_checkpoint()` after running a script.

In the `vm-js` execution path, Promise jobs enter the host microtask queue via `vm_js::VmHostHooks`
(`vendor/ecma-rs/vm-js/src/jobs.rs`). FastRender implements these hooks in
`src/js/vmjs/window_timers.rs` (`VmJsEventLoopHooks`), which enqueues each `vm_js::Job` into the
host-owned `EventLoop` microtask queue. This keeps `Promise.then(...)` reactions and
`queueMicrotask(...)` callbacks in the same FIFO microtask checkpoint.

### 5) Base URL timing (script preparation time)
Relative script URLs must be resolved using the document base URL **as of the moment the script is
prepared**, not “whatever the final `<base href>` was after parsing”.

This requires tracking the base URL while parsing (see `BaseUrlTracker` below).

---

## Known gaps / conservative behavior (still true)
These features exist in the HTML/FETCH/CSP specs and matter for web-compat, but FastRender is still
intentionally conservative in some areas:

- **Module scripts are opt-in:** `<script type="module">`, dynamic `import()`, and import maps work
  when `JsExecutionOptions::supports_module_scripts` is enabled and the executor provides module
  loading (e.g. `VmJsBrowserTabExecutor`). The default options disable module loading for
  hostile-input safety.
- **Content Security Policy (CSP):** partially implemented for scripts in `api::BrowserTab`
  - Enforces `script-src` / `default-src` for external `<script src=...>` URL allowlisting.
  - Enforces nonce/hash-based allowlisting for inline scripts (`nonce=` + `'nonce-...'`, and
    `'sha256-...'`).
  - `strict-dynamic` is recognized but handled conservatively (no trust propagation).
- **`document.write()` is a bounded subset**, not full HTML semantics:
  - FastRender implements a limited streaming-parse re-entry subset (`src/html/document_write.rs`):
    `document.write()`/`writeln()` inject into the active streaming parser input stream during
    parser-blocking script execution.
  - When no streaming parser is active, `document.write()` is treated as a no-op (deterministic
    subset; no implicit `document.open()` / destructive post-load writes).
- **Fetch integration remains incomplete:** `BrowserTab` enforces key script checks (MIME sanity,
  basic CORS gating, SRI for classic/module scripts, referrer policy propagation), but does not yet
  model the full Fetch/HTML surface (streaming network, full credentials/mode nuance, service
  workers, CORP/COEP, etc.).
- **DOM/Web APIs are still a subset:** script ordering/lifecycle is now largely spec-shaped, but the
  JS-visible platform surface is still being built out, so many real pages will fail due to missing
  WebIDL bindings or Web APIs rather than incorrect `<script>` ordering.

When adding any of the above later, treat the HTML Standard as the source of truth and extend the
state machine; do not “patch in” ad-hoc behavior.

---

## Spec anchors (local WHATWG HTML copy)
The HTML Standard’s requirements are scattered, but the following sections are the “spine” of
script processing. All references below are to the local submodule file:

`specs/whatwg-html/source`

### Core algorithms
- **Script processing model (script element state):**
  - `id="script-processing-model"` (also see `id="non-blocking"`)
  - Grep: `rg -n 'id="script-processing-model"' specs/whatwg-html/source`
- **Prepare a script** (“prepare the script element”):
  - `id="prepare-a-script"`
  - Grep: `rg -n 'id="prepare-a-script"' specs/whatwg-html/source`
- **Execute the script block** (“execute the script element”):
  - `id="execute-the-script-block"`
  - Grep: `rg -n 'id="execute-the-script-block"' specs/whatwg-html/source`
- **Import maps** (parse + register):
  - Grep: `rg -n 'create an import map parse result' specs/whatwg-html/source`
  - Grep: `rg -ni 'register an import map' specs/whatwg-html/source`
- **Module graph fetch (external)**:
  - Grep: `rg -n 'fetch an external module script graph' specs/whatwg-html/source`

### `async` / `defer` conditions overview
- The narrative summary for classic scripts lives near the `async`/`defer` attribute definitions,
  followed by the processing model section:
  - `rg -n 'attr-script-async' specs/whatwg-html/source`
  - `rg -n 'attr-script-defer' specs/whatwg-html/source`
  - `rg -n 'id="script-processing-model"' specs/whatwg-html/source`

---

## Architecture overview (FastRender components)
The design is intentionally split into **parser**, **DOM**, **scheduler**, and **event loop**.
Keeping these boundaries crisp is what makes later module/import map work tractable.

### 1) Streaming HTML parser driver (pause/resume at `</script>`)
**Responsibility:** drive tokenization/tree building incrementally so the engine can:

- pause parsing when a parser-inserted script becomes eligible to run (at `</script>`),
- execute that script (which can mutate the DOM),
- then resume parsing from the exact byte offset.

**Home (current):**

- `src/html/streaming_parser.rs` (`StreamingHtmlParser`)
- `src/html/pausable_html5ever.rs` (`PausableHtml5everParser`)
- (Legacy/testing utility) `src/dom/scripting_parser.rs` (`ScriptingHtmlParser`,
  `parse_html_with_scripting`)

**Key operations (current API; see `StreamingHtmlParser`):**

- `push_str(chunk)` / `push_front_str(chunk)` → supply decoded input (including `document.write`-style
  injection).
- `pump()` → advances parse state until:
  - it yields `Script { script, base_url_at_this_point }`, or
  - it yields `NeedMoreInput`, or
  - it yields `Finished { document }`.
- After handling a yielded `Script`, call `pump()` again to resume parsing (there is no separate
  `resume()` API).

**Important integration point:** async/ordered-asap scripts can become ready *while parsing is still
in progress*. The production `BrowserTab` integration handles this by parsing in event-loop-driven
**slices**:

- `JsExecutionOptions.dom_parse_budget` bounds how many `StreamingHtmlParser::pump()` iterations are
  performed per parse task.
- When the budget is exhausted, `BrowserTabHost` snapshots the parser DOM into the host document and
  yields back to the event loop, giving any queued script-execution tasks a chance to run.
- `BrowserTabHost` also detects already-ready “as soon as possible” scripts and yields immediately
  so their tasks run *before* parsing continues (“still blocks at its execution point” in HTML).

### 2) `dom2` TreeSink + mutable DOM invariants
**Responsibility:** build a mutable document tree *as the parser runs*, so scripts can observe and
mutate it.

FastRender’s legacy DOM (`crate::dom::DomNode` in `src/dom.rs`) is immutable and built after parsing,
so it cannot support correct parser-time script execution.

**Existing home:** `src/dom2/` (`dom2::Document`, `NodeId`, `NodeKind`).

**TreeSink home:** `src/dom2/html5ever_tree_sink.rs` (`dom2::Dom2TreeSink`).

This is the bridge between html5ever’s tokenizer/tree-builder and our mutable DOM. It incrementally
builds a live `dom2::Document` during parsing and wires parse-time `<base href>` tracking by calling
into `BaseUrlTracker` as elements are inserted.

**Mutable DOM invariants that must always hold:**

- `node.parent` must be consistent with the parent’s `children` list.
- Child order must match insertion order (this affects DOM APIs and script ordering).
- Template contents must remain present but be marked inert (`Node::inert_subtree`) to match
  FastRender’s existing “skip template contents” behavior in traversals.

### 3) `BaseUrlTracker`
**Responsibility:** track the document base URL **as parsing progresses**, including:

- default base URL = document URL (or base hint),
- first `<base href>` in the document’s `<head>` that has a valid href updates the base,
- `<base>` elements inside inert/template/foreign content must not affect the base.

**Why this exists:** `src/html/mod.rs::document_base_url()` computes the base URL from a completed
DOM. That is correct for post-parse utilities, but wrong for parser-inserted script `src`
resolution timing.

**Home:** `src/html/base_url_tracker.rs` (`BaseUrlTracker`).

**Interface (current):**

- `BaseUrlTracker::new(document_url: Option<&str>)`
- `BaseUrlTracker::current_base_url() -> Option<String>`
- `BaseUrlTracker::on_element_inserted(...)` — called by the parser/tree-sink when elements are
  inserted, so the tracker can react to `<base href>` in `<head>`.
- `BaseUrlTracker::resolve_script_src(raw_src)` — resolve `<script src>` using the base URL in effect
  at preparation time.

### 4) Script scheduling (state machine + external fetch integration)
**Responsibility:** implement HTML’s script scheduling model for classic scripts, module scripts, and
import maps:

- classify scripts (classic/module/importmap/unknown) and ignore non-executable types,
- resolve `src` against the base URL *at preparation time*,
- fetch external scripts using the engine’s fetcher,
- decide whether parsing must block, or whether execution is deferred/async,
- enqueue script execution into the event loop and run microtask checkpoints afterward.

**Home:** `src/js/html_script_scheduler.rs` (`HtmlScriptScheduler`).

This scheduler is intentionally action-based so it can be driven by a streaming parser integration
that needs explicit "block parser" signals.

**Orchestration helpers:**

- `src/js/html_script_pipeline.rs`: lightweight harness/orchestrator used by unit tests.
- `src/api/browser_tab.rs`: production “tab” integration (streaming parsing + script scheduling +
  DOM mutation + rendering).
  - `BrowserTabHost::apply_scheduler_actions(...)` is the main “action interpreter”: it starts
    fetches, queues execution tasks, performs synchronous `ExecuteNow` work, and dispatches
    `<script>` load/error element tasks.

**Inputs:**

- `ScriptElementSpec` snapshots built at parse time (or dynamic insertion time),
- current base URL (from `BaseUrlTracker` / streaming parser state),
- a fetch interface (host-provided via action handling).

**Outputs (scheduler actions):**

- `StartClassicFetch { ... }` for external classic scripts.
- `StartModuleGraphFetch { ... }` / `StartInlineModuleGraphFetch { ... }` for module scripts.
- `BlockParserUntilExecuted { ... }` for parser-blocking scripts.
- `ExecuteNow { ... }` for synchronous script-boundary work.
- `QueueTask { ... }` for async/defer/in-order execution tasks.
- `QueueScriptEventTask { ... }` for `<script>` load/error event dispatch.

### 5) `EventLoop` + microtask checkpoint points
**Responsibility:** provide HTML-style scheduling primitives:

- a task queue (script tasks, networking tasks later),
- a microtask queue (promise jobs / `queueMicrotask`),
- an explicit microtask checkpoint algorithm.

**Existing home:** `src/js/event_loop.rs`.

**Checkpoint points we must honor for correctness:**

1. **before preparing/executing a parser-inserted script** at a `</script>` boundary when the
   JavaScript execution context stack is empty (this allows already-queued microtasks to run before
   the next parser-inserted script),
2. after running a script, if the JavaScript execution context stack is empty (HTML “clean up after
   running script”),
3. after running any event loop task (already handled by `run_next_task()`),
4. at “end of parsing” milestones (after running deferred scripts; before ready-state changes later).

---

## End-to-end flow (classic scripts)
This section ties the components together. The goal is to make the parser/scheduler/event-loop
boundaries explicit.

### A) Parsing, encountering `<script>`, and pausing at `</script>`
1. Streaming parser builds nodes into a live `dom2::Document` via the `dom2` html5ever TreeSink
   (`Dom2TreeSink`).
2. When a `<script>` end tag is processed, the parser driver builds a `ScriptElementSpec` for that
   element *at this parse position* (see `src/js/streaming.rs`), using:
   - element attributes (`src`, `async`, `defer`, `type`/`language`),
   - accumulated inline text content (if no `src`),
   - the current base URL from `BaseUrlTracker`.
3. The parser driver feeds that spec into the action-based scheduler:
   `HtmlScriptScheduler::discovered_parser_script(spec, node_id, base_url_at_discovery)`.
4. The scheduler returns a `HtmlDiscoveredScript { id, actions }`, where `actions` can include:
   - `StartClassicFetch { script_id, url, ... }` (external classic script),
   - `StartModuleGraphFetch { ... }` / `StartInlineModuleGraphFetch { ... }` (module scripts),
   - `BlockParserUntilExecuted { ... }` (parser-blocking script),
   - `ExecuteNow { ... }` (synchronous boundary work like import maps),
   - `QueueTask { ... }` (async/defer/in-order execution tasks).
5. The orchestrator applies these actions:
   - starts fetches in the host networking layer,
   - pauses/resumes the parser as directed,
   - executes scripts and runs required microtask checkpoints.

### B) Executing a classic script
When it is time to run a script (via `ExecuteNow` or `QueueTask`):

1. Run the script body in the document’s JS realm (engine + WebIDL bindings; out-of-scope here).
2. Run a microtask checkpoint:
   - for `ExecuteNow`, the orchestrator must call `EventLoop::perform_microtask_checkpoint()`
     immediately after execution **if the JS execution context stack is empty**.
   - for `QueueTask`, the event loop itself runs a checkpoint after the task (see
     `EventLoop::run_next_task()`), which satisfies the HTML requirement.
3. Continue:
   - for parser-blocking scripts: resume parsing once the scheduler’s “block parser” condition is
     cleared,
   - for async scripts: parsing may be interrupted by async-ready scripts (depending on how often
     the parser yields),
   - for deferred scripts: run in order after parsing completes.

### C) End of parsing
When the streaming parser reaches end-of-input:

1. Notify the scheduler (`HtmlScriptScheduler::parsing_completed()`).
2. Apply any returned actions, typically queueing deferred scripts as tasks in document order.
3. Then allow later lifecycle steps (DOMContentLoaded/readyState changes) to be scheduled (future).

---

## Module scripts + import maps
FastRender’s scheduler/pipeline also implements key spec-correct behavior for:

- module scripts (`<script type="module">`)
- import maps (`<script type="importmap">`)
- `nomodule` gating for classic scripts

The source of truth is still the HTML Standard’s `prepare-a-script` + `execute-the-script-block`
algorithms (see the spec anchors above).

### 1) `type="module"` scheduling rules (async vs default-defer; dynamic insertion ordering)
The key differences vs classic scripts are:

- **Parser-inserted module scripts are never parser-blocking by default.**
  - If the `async` attribute is **present**, the module script is in the document’s
    **"set of scripts that will execute as soon as possible"**: fetch the entire module graph in
    parallel with parsing; execute once ready (potentially before parsing completes).
  - Otherwise (`async` **absent**), the module script behaves like **defer-by-default**: it is
    appended to the document’s **"list of scripts that will execute when the document has finished
    parsing"** (the same list used by classic `defer` scripts), and executes after parsing completes
    in document order. (The `defer` attribute has **no effect** on modules.)
- **Dynamically-inserted (not parser-inserted) module scripts** that are not `async` participate in a
  separate ordering list: the document’s **"list of scripts that will execute in order as soon as
  possible"**. This ensures deterministic **insertion order** execution for dynamically inserted
  modules when `async` is not set.
- **Inline module scripts never execute synchronously at the `</script>` boundary.**
  Even if they have no dependencies, the spec queues a task before executing (see the HTML note
  under the inline-module branch of `prepare-a-script`).

### 2) `nomodule`
In `prepare-a-script`, if a `<script>` has a `nomodule` content attribute and its type is
`classic`, then it is skipped entirely. This is how browsers implement the modern "modules + legacy"
pattern:

```html
<script type="module" src="modern.js"></script>
<script nomodule src="legacy.js"></script>
```

Spec note: specifying `nomodule` on a module script has no effect; the algorithm continues.

### 3) `<script type="importmap">` parsing + registration timing
Import maps are *data blocks* that execute (register) synchronously, but they are not classic scripts:

- **No `src`:** if an import map `<script>` has a `src` attribute, the spec queues an `error` event
  task and returns (external import maps are intentionally unsupported).
- **Parsing:** `prepare-a-script` creates an **import map parse result** from the inline text and the
  script’s base URL.
- **Registration timing:** because the result is immediately available, `prepare-a-script` then
  **immediately executes the script element**, which (for `type=importmap`) runs `register an import
  map` synchronously.
  - This means import maps take effect at the `</script>` boundary, before later scripts are
    prepared.

### 4) `Document.currentScript` (modules + import maps)
Only classic scripts participate in `Document.currentScript`:

- During module script execution, HTML asserts `document.currentScript` is **null**.
- Import map scripts never set `Document.currentScript`.
  - This means an import map runs with whatever value was already present: typically **null** during
    parsing, but potentially a currently executing classic script element during nested/re-entrant
    execution (import maps can execute immediately even if another script is already executing).

So, once module scripts/import maps are integrated, the host-side bookkeeping must treat
`currentScript` as **classic-only**.

### 5) Module graph fetch + caching responsibilities (module map + resolved module set)
Adding module scripts makes the engine responsible for spec-shaped module caching and specifier
resolution:

- **Module map (per `Document`):** cache module graph fetch results in the document’s **module map**
  (keyed by `(URL, module type)`), including in-flight deduplication ("fetching" sentinel entries).
  This is the cache consulted by `fetch an external module script graph`.
- **Resolved module set (per global object):** cache specifier resolution results in the global
  object’s **resolved module set** so repeated resolution for the same `(referrer, specifier)` pair
  is stable.
   - When registering an import map, the spec merges it into the global import map *while ensuring it
     does not retroactively affect already-resolved modules* (rules that would impact them are
     ignored).

### 6) Dynamic `import()` and import maps
When module scripts are enabled, dynamic `import(specifier)` is supported from both classic scripts
and module scripts (see `tests/js/js_html_integration.rs` P2 tests).

At a high level:

- Module specifier resolution goes through the same per-document import map state used for static
  `import` declarations.
- The embedder module loader is host-owned and lives in `src/js/realm_module_loader.rs`.

### 7) Module evaluation and top-level `await` (`Pending` completion)
ECMAScript module evaluation returns a Promise (to model top-level `await`). HTML therefore treats
module script execution as potentially asynchronous.

FastRender models this explicitly in the `BrowserTab` integration:

- `BrowserTabJsExecutor::execute_module_script(...)` returns
  `ModuleScriptExecutionStatus::{Completed, Pending}` (`src/api/browser_tab.rs`).
- If `Pending` is returned, the host must *not* finalize the `<script>` yet; it waits for the module
  evaluation promise to settle before:
  - dispatching `<script>` `load`/`error` events, and
  - unblocking ordered module execution queues (dynamic `async=false` ordering and post-parse modules).
- Completion is reported by queueing an event-loop task that calls
  `BrowserTabHost::on_module_script_evaluation_complete(...)` (`src/api/browser_tab.rs`).
  The production `vm-js` executor (`src/api/browser_tab_vm_js_executor.rs`) does this by polling
  pending evaluation promises from `BrowserTabJsExecutor::after_microtask_checkpoint(...)` and
  queueing a `TaskSource::Script` task when a promise settles.

### 8) Code map (where this lives)
The integrated module-script/import-map pipeline lives in:

- **Streaming parser driver:** `src/html/streaming_parser.rs` (pause/resume at `</script>`)
- **Base URL timing:** `src/html/base_url_tracker.rs` (`BaseUrlTracker`)
- **Import maps algorithms:** `src/js/import_maps/` (parsing + state + merge/register + resolution; see
  [`docs/import_maps.md`](import_maps.md))
- **Scheduler/orchestration:**
  - `src/js/html_script_scheduler.rs`
  - `src/js/html_script_pipeline.rs` (test harness)
  - `src/api/browser_tab.rs` (production “tab” integration)
