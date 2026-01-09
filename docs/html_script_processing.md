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
FastRender has the **core building blocks** for a streaming, parse-time classic `<script>` pipeline
(pause/resume parsing at `</script>`, schedule parser-blocking/`async`/`defer` scripts, and keep
observable document state like `Document.currentScript` correct). Some of the plumbing is still
being wired together, so treat this section as a “where is the real code?” map.

- **HTML parsing (pause/resume at `</script>`):**
  - `src/html/pausable_html5ever.rs`: wraps html5ever so the host can observe
    `TokenizerResult::Script` suspension points (html5ever’s built-in driver currently loops past
    them).
  - `src/html/streaming_parser.rs`: streaming parser driver built on `PausableHtml5everParser`,
    responsible for incremental feeding, pausing at script boundaries, and resuming from the exact
    input offset.
  - (Legacy/bridge) `src/dom/scripting_parser.rs`: an incremental html5ever parser that yields at
    `<script>` boundaries and snapshots the partial DOM to `crate::dom::DomNode` (useful for tests
    and incremental adoption; not `dom2`-backed).
- **DOM construction:**
  - `src/dom2/`: mutable DOM (`dom2::Document`) used by JS bindings and script-visible mutations.
  - **dom2 TreeSink:** the html5ever `TreeSink` implementation for `dom2::Document` lives under
    `src/dom2/` (search for the `impl html5ever::tree_builder::TreeSink`).
- **Script scheduling / host orchestration:**
  - `src/js/script_scheduler.rs`: classic-script ordering (parser-blocking vs `async` vs `defer`)
    integrated with the HTML-shaped `EventLoop`.
  - `src/js/orchestrator.rs`: host-side `Document.currentScript` bookkeeping around “execute the
    script block”.
  - `src/js/event_loop.rs`: task + microtask queues with explicit microtask checkpoint draining.
  - `src/js/streaming.rs`: helpers for building `ScriptElementSpec` at parse time (including base
    URL timing).
  - `src/js/html_scripting.rs`: integration harness used by the end-to-end pipeline tests (Task
    129).
  - (Higher-level host runtime) `src/api/browser_document_js.rs`: couples `dom2`, a JS runtime, an
    HTML-shaped `EventLoop`, and `currentScript` bookkeeping.
- **Legacy tooling (deprecated for execution):**
  - `src/js/dom_scripts.rs` / `extract_script_elements()`: post-parse DOM scanning for tooling only
    (not spec-correct for execution).

### How to run tests
The unit + integration tests for the streaming pipeline live in the `fastrender` crate’s `--lib`
tests. Run them (scoped) with:

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
- CORS / SRI (`crossorigin`, `integrity`) and fetch mode nuances for scripts

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

**Home:** `src/html/streaming_parser.rs`.

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

**TreeSink:** `dom2` includes an `html5ever::tree_builder::TreeSink` implementation backed by
`dom2::Document`. This is the bridge between the tokenizer/tree-builder and our mutable DOM.

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

**Home:** `src/html/base_url_tracker.rs`.

**Interface:**

- `BaseUrlTracker::new(document_url: Option<&str>)`
- `BaseUrlTracker::current_base_url() -> Option<String>`
- `BaseUrlTracker::on_element_inserted(tag_name, namespace, attrs, in_head, in_foreign_namespace, in_template)`
- `BaseUrlTracker::resolve_script_src(raw_src)` — resolve `<script src>` using the base URL in effect
  at preparation time.

### 4) `ScriptScheduler` (state machine + external fetch integration)
**Responsibility:** implement the classic-script subset of the HTML processing model:

- classify scripts (classic/module/importmap/unknown) and ignore non-executable types,
- resolve `src` against the base URL *at preparation time*,
- fetch external scripts using the engine’s fetcher,
- decide whether parsing must block, or whether execution is deferred/async,
- enqueue script execution into the event loop and run microtask checkpoints afterward.

**Home:** `src/js/script_scheduler.rs`.

**Inputs:**

- script element node id (from `dom2` TreeSink) and accessors for its attributes/text,
- current base URL (from `BaseUrlTracker`),
- a fetch interface (initially `crate::resource::ResourceFetcher` in `src/resource.rs`).

**Outputs:**

- “parser blocked/unblocked” signals for the streaming parser driver,
- tasks queued into `EventLoop` for async/defer script execution.

**State machine sketch (classic scripts only):**

- `pending_parsing_blocking: Option<ScriptId>`
- `defer_queue: Vec<ScriptId>` (document order)
- `async_ready_queue: VecDeque<ScriptId>` (run ASAP)
- `parsing_complete: bool`

Where `ScriptId` is an internal handle to a prepared script record:

- inline text (already available) OR fetched source bytes/text
- resolved URL (if external)
- flags: `async`, `defer`, `parser_inserted`

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
1. Streaming parser builds nodes into `dom2::Document` via the TreeSink.
2. When a `<script>` end tag is processed, the parser driver calls into `ScriptScheduler` with:
   - node id for the `<script>` element,
   - the element’s current attributes (`src`, `async`, `defer`, `type`/`language`),
   - the accumulated inline text content (if no `src`).
3. `ScriptScheduler` performs the spec’s “prepare the script element” steps relevant to v1:
   - determine script type (`classic` vs others),
   - compute whether the script is external,
   - resolve `src` URL against `BaseUrlTracker::current_base_url()`.
4. Scheduler decides:
   - **parsing-blocking** → return “block parser”, execute now, then “unblock parser”,
   - **async** → start fetch, return “continue parsing”,
   - **defer** → start fetch, enqueue into defer list, return “continue parsing”.

### B) Executing a classic script
When it is time to run a script (immediately, async-ready, or deferred):

1. Run the script body in the document’s JS realm (engine integration; out-of-scope for this doc).
2. Call `EventLoop::perform_microtask_checkpoint()` to drain microtasks.
3. Continue:
   - for parsing-blocking scripts: resume parsing,
   - for async scripts: parser may be interrupted again later,
   - for deferred scripts: continue draining the deferred queue at end-of-parse.

### C) End of parsing
When the streaming parser reaches end-of-input:

1. Mark parsing complete.
2. Execute deferred scripts in document order, each followed by a microtask checkpoint.
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
