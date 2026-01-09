# HTML `<script>` processing + parser integration (classic scripts first)

## Purpose
FastRender’s JavaScript support needs to follow the WHATWG HTML **script processing model** so that:

1. Scripts can run **during parsing** (observing a partially-built DOM).
2. `async` / `defer` ordering matches browser behavior.
3. Script execution is integrated with an HTML-shaped **event loop** (tasks + microtasks).
4. Relative `src` URLs resolve against the **base URL in effect at script preparation time**.

This document is a spec-mapped design for that integration. It is written to prevent future
implementers from having to “rediscover” scattered HTML Standard details when extending support to
module scripts/import maps later.

## Status in this repository (reality check)
FastRender has the **core building blocks** for a spec-shaped, streaming, parse-time classic
`<script>` pipeline (pause/resume parsing at `</script>`, schedule parser-blocking/`async`/`defer`
scripts, and keep observable document state like `Document.currentScript` correct). Some plumbing is
still evolving, so treat this section as a “where is the real code?” map.

There is not yet a production author-JS VM executing page scripts end-to-end, but the host-side
plumbing is laid out in explicit modules so the remaining integration work can stay spec-shaped.

What exists today (in-tree):

- **HTML parsing hooks (pause at `</script>`):**
  - `src/html/pausable_html5ever.rs`: wraps html5ever so the host can observe
    `TokenizerResult::Script` suspension points (html5ever’s built-in driver currently loops past
    them).
  - `src/dom/scripting_parser.rs`: `parse_html_with_scripting(...)` pauses at `</script>` boundaries
    and yields a `ScriptToken` plus a partial DOM snapshot (currently backed by
    `markup5ever_rcdom`).
  - (Planned home) `src/html/streaming_parser.rs`: a dedicated streaming parser driver that feeds
    input incrementally and pauses/resumes around parser-blocking scripts while building a live
    `dom2` document (via the TreeSink noted below).
- **Parse-time base URL tracking:**
  - `src/html/base_url_tracker.rs`: `BaseUrlTracker` tracks `<base href>` as the parser progresses
    so `<script src>` resolution uses the base URL *at script preparation time*.
- **Script element normalization at parse time:**
  - `src/js/mod.rs`: `ScriptType` + `ScriptElementSpec` (flattened `<script>` record).
  - `src/js/streaming.rs`: helpers for building `ScriptElementSpec` at the moment a `<script>`
    finishes parsing.
- **Script scheduling + event loop:**
  - `src/js/script_scheduler.rs`: classic-script ordering (parser-blocking vs `async` vs `defer`),
    including an action-based scheduler (`ScriptSchedulerAction`) plus a higher-level helper
    (`ClassicScriptScheduler`).
  - `src/js/event_loop.rs`: task + microtask queues, explicit microtask checkpoints, timers, run
    limits (`RunLimits`), and queue caps (`QueueLimits`).
- **Host-side execution bookkeeping:**
  - `src/js/orchestrator.rs`: host-side `Document.currentScript` bookkeeping around “execute the
    script block” (classic scripts).
- **JS-enabled host container (early embedding surface):**
  - `src/api/browser_document_js.rs`: `BrowserDocumentJs` couples a live `dom2` document, a JS
    runtime adapter, an HTML-shaped `EventLoop`, and `currentScript` bookkeeping.
- **Mutable DOM for bindings (intended):**
  - `src/dom2/`: mutable DOM (`dom2::Document`) intended for JS bindings and script-visible
    mutations.
  - `src/dom2/import.rs`: current bridge for constructing `dom2::Document` from the renderer’s
    immutable `crate::dom::DomNode`.
  - **Missing piece:** an `html5ever::tree_builder::TreeSink` implementation backed by `dom2`
    (expected to live under `src/dom2/`), so the parser can build the live DOM directly while
    pausing/resuming at scripts.
- **End-to-end harness (not a full HTML parser):**
  - `src/js/html_scripting.rs`: a small harness used by unit tests to exercise script/style
    interaction and event loop semantics (Task 129).
- **Legacy tooling (deprecated for execution):**
  - `src/js/dom_scripts.rs::extract_script_elements()`: post-parse DOM scanning for tooling only
    (not spec-correct for execution).

### How to run tests
The relevant unit tests live in the `fastrender` crate’s `--lib` test binary. Run them (scoped) with:

`scripts/cargo_agent.sh test -p fastrender --lib`

---

## What we implement in the classic-script milestone (in-scope)
This is the **v1** execution model that should be implemented before modules/import maps.

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

### 3) Microtask checkpoints (Promises/jobs)
After **any** script execution (parser-blocking, async, or deferred), run a **microtask checkpoint**
until the microtask queue is empty.

In code, this maps to `src/js/event_loop.rs`:
- `EventLoop::run_next_task()` always follows a task with a checkpoint.
- Parser-driven synchronous execution must explicitly call
  `EventLoop::perform_microtask_checkpoint()` after running a script.

### 4) Base URL timing (script preparation time)
Relative script URLs must be resolved using the document base URL **as of the moment the script is
prepared**, not “whatever the final `<base href>` was after parsing”.

This requires tracking the base URL while parsing (see `BaseUrlTracker` below).

---

## Explicitly NOT implemented yet (non-goals for v1)
These features exist in the HTML spec and matter for web-compat, but are intentionally deferred so
we can land a correct classic-script core first:

- **Module scripts** (`type="module"`) and the module graph (host hooks, `import`, dynamic import)
- **Import maps** (`type="importmap"`) parsing + registration + interaction with module fetch
- **Content Security Policy (CSP)** checks for inline scripts / external fetch
- The `nomodule` attribute behavior
- `document.write()` and the “ignore-destructive-writes counter”
- **Stylesheet-blocking scripts** (scripts that wait for render-blocking stylesheets)
  - A prototype exists for the harness (`src/js/html_scripting.rs` +
    `src/js/script_blocking_stylesheets.rs`), but it is not yet fully integrated with the real
    streaming parser + scheduler pipeline.
- CORS / SRI (`crossorigin`, `integrity`) and fetch mode nuances for scripts
- End-to-end `Document.currentScript` integration with a real JS VM + WebIDL bindings (host-side
  bookkeeping exists in `src/js/orchestrator.rs`, but it is not yet wired to a production JS runtime)

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

- `src/html/pausable_html5ever.rs` (`PausableHtml5everParser`)
- `src/dom/scripting_parser.rs` (`ScriptingHtmlParser`, `parse_html_with_scripting`)
  
Note: a dedicated streaming parser driver module may be added later (suggested home:
`src/html/streaming_parser.rs`), but the core “pause at TokenizerResult::Script” hook is already
exposed by `PausableHtml5everParser`.

**Key operations (conceptual):**

- `feed(bytes)` → advances parse state until:
  - it needs to block on a parser-inserted script, or
  - end-of-input is reached.
- `resume()` → continues parsing after the scheduler unblocks.

**Important integration point:** for `async` scripts, the parser must periodically yield to the
script scheduler (so “async-ready” scripts can interrupt parsing, as browsers do). In a
single-threaded model this can be “check after each chunk/token”.

### 2) `dom2` TreeSink + mutable DOM invariants
**Responsibility:** build a mutable document tree *as the parser runs*, so scripts can observe and
mutate it.

FastRender’s legacy DOM (`crate::dom::DomNode` in `src/dom.rs`) is immutable and built after parsing,
so it cannot support correct parser-time script execution.

**Existing home:** `src/dom2/` (`dom2::Document`, `NodeId`, `NodeKind`).

**Missing piece (still required for spec-correct parse-time JS):** an
`html5ever::tree_builder::TreeSink` implementation backed by `dom2::Document`. This is the bridge
between the tokenizer/tree-builder and our mutable DOM.

Until this exists, the pausable parser path uses `markup5ever_rcdom` and converts to renderer DOM
snapshots for callbacks; `dom2` documents are typically created by importing those renderer DOM
snapshots via `src/dom2/import.rs`.

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
**Responsibility:** implement the classic-script subset of the HTML processing model:

- classify scripts (classic/module/importmap/unknown) and ignore non-executable types,
- resolve `src` against the base URL *at preparation time*,
- fetch external scripts using the engine’s fetcher,
- decide whether parsing must block, or whether execution is deferred/async,
- enqueue script execution into the event loop and run microtask checkpoints afterward.

**Home:** `src/js/script_scheduler.rs`.

This module contains two layers:

- **Action-based state machine:** `ScriptScheduler<NodeId>` returning `ScriptSchedulerAction` values.
  This is designed for a streaming parser driver that needs explicit "block parser" signals.
- **Host-integrated helper:** `ClassicScriptScheduler<Host>` which executes scripts against an
  `EventLoop` via a `ScriptLoader`/`ScriptExecutor` trait boundary (useful for unit tests and early
  integration).

**Inputs:**

- script element node id (from `dom2` TreeSink) and accessors for its attributes/text,
- current base URL (from `BaseUrlTracker`),
- a fetch interface (initially `crate::resource::ResourceFetcher` in `src/resource.rs`).

**Outputs (for the action-based scheduler):**

- “block parser until executed” signals for the streaming parser driver (as an action),
- tasks queued into `EventLoop` for async/defer script execution (as an action).

**State machine sketch (classic scripts only; action-based scheduler):**

`src/js/script_scheduler.rs::ScriptScheduler` tracks:

- `scripts: HashMap<ScriptId, ExternalScriptEntry<NodeId>>` for external scripts (blocking/async/defer)
  and their fetch/execution readiness
- `defer_queue: Vec<ScriptId>` + `next_defer_to_queue: usize` to preserve document order for deferred
  scripts
- `parsing_completed: bool` to gate when deferred scripts become eligible to run

Parser blocking is represented explicitly via `ScriptSchedulerAction::BlockParserUntilExecuted`
(the orchestrator decides when to resume parsing).

### 5) `EventLoop` + microtask checkpoint points
**Responsibility:** provide HTML-style scheduling primitives:

- a task queue (script tasks, networking tasks later),
- a microtask queue (promise jobs / `queueMicrotask`),
- an explicit microtask checkpoint algorithm.

**Existing home:** `src/js/event_loop.rs`.

**Checkpoint points we must honor for correctness:**

1. after running any script (parser-blocking, async, deferred),
2. after running any event loop task (already handled by `run_next_task()`),
3. at “end of parsing” milestones (after running deferred scripts; before ready-state changes later).

---

## End-to-end flow (classic scripts)
This section ties the components together. The goal is to make the parser/scheduler/event-loop
boundaries explicit.

### A) Parsing, encountering `<script>`, and pausing at `</script>`
1. Streaming parser builds nodes into a mutable DOM (eventually `dom2::Document` via a TreeSink).
2. When a `<script>` end tag is processed, the parser driver builds a `ScriptElementSpec` for that
   element *at this parse position* (see `src/js/streaming.rs`), using:
   - element attributes (`src`, `async`, `defer`, `type`/`language`),
   - accumulated inline text content (if no `src`),
   - the current base URL from `BaseUrlTracker`.
3. The parser driver feeds that spec into the action-based scheduler:
   `ScriptScheduler::discovered_parser_script(...)`.
4. The scheduler returns a `DiscoveredScript { id, actions }`, where `actions` can include:
   - `StartFetch { url }` (external script),
   - `BlockParserUntilExecuted` (parser-blocking external script),
   - `ExecuteNow { source_text }` (inline scripts, or blocking externals after fetch completion),
   - `QueueTask { source_text }` (async/defer execution).
5. The orchestrator applies these actions:
   - starts fetches in the host networking layer,
   - pauses/resumes the parser as directed,
   - executes scripts and runs required microtask checkpoints.

### B) Executing a classic script
When it is time to run a script (via `ExecuteNow` or `QueueTask`):

1. Run the script body in the document’s JS realm (engine + WebIDL bindings; out-of-scope here).
2. Run a microtask checkpoint:
   - for `ExecuteNow`, the orchestrator must call `EventLoop::perform_microtask_checkpoint()`
     immediately after execution.
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

1. Notify the scheduler (`ScriptScheduler::parsing_completed()`).
2. Apply any returned actions, typically queueing deferred scripts as tasks in document order.
3. Then allow later lifecycle steps (DOMContentLoaded/readyState changes) to be scheduled (future).

---

## Notes for future module/import map support (why this design scales)
The classic-script architecture above deliberately isolates:

- **parsing** (how we incrementally build DOM),
- **preparation** (how we classify scripts + resolve URLs),
- **fetching** (network integration),
- **execution** (JS engine + realm),
- **scheduling** (async/defer + event loop).

Modules/import maps extend the same pipeline by adding new “prepare” + “execute” branches:

- `ScriptType::Module` and `ScriptType::ImportMap` already exist in `src/js/mod.rs`.
- The `ScriptScheduler` should become a dispatcher that:
  - runs import map registration at the correct point (before module graph resolution),
  - builds/fetches module graphs using host hooks,
  - preserves async/defer-like ordering for modules (different rules than classic scripts).

Keeping base URL tracking, DOM mutability, and event loop semantics consistent is what keeps these
extensions from becoming a rewrite.
