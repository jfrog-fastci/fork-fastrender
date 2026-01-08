# JavaScript integration architecture

This document describes how **JavaScript + web APIs** plug into FastRender’s existing staged renderer.
It is intended to be a contributor-facing mental model, not an implementation log.

Workstreams/spec anchors:

- JS support goals + milestones: [`instructions/javascript_support.md`](../instructions/javascript_support.md)
- `ecma-rs` submodule workflow: [`instructions/ecma_rs.md`](../instructions/ecma_rs.md)
- Renderer pipeline overview: [`docs/architecture.md`](architecture.md)
- Conformance matrix (repo reality): [`docs/conformance.md`](conformance.md)

## 1) The pipeline becomes “staged + mutable”

Today the renderer is roughly:

fetch → parse HTML → parse CSS → cascade/compute → box tree → layout → paint

With JS enabled, the pipeline is still staged, but the document can be mutated:

- JS can run **during parsing** (`<script>` processing model) and **after parsing** (event loop tasks).
- JS can mutate DOM, attributes, and (eventually) stylesheets; those mutations must trigger:
  - style invalidation,
  - layout invalidation,
  - paint invalidation.

### Practical first step (intentionally conservative)

The first correct integration point is usually:

1. Run a task (e.g. a script, a timer callback, an async script “ready” task)
2. Run a microtask checkpoint
3. If the document is dirty, do a **full** re-style/re-layout/repaint before the next “frame”

Incremental invalidation can come later; correctness comes first.

## 2) The JS host environment boundary

FastRender’s role is to provide the **host environment** that ECMAScript expects:

- a realm + global object (`Window`-shaped global),
- host hooks for module loading (later),
- task scheduling (timers, async scripts, networking integration),
- Web IDL-backed DOM and web APIs.

The JavaScript language implementation itself should live in `engines/ecma-rs/` (per the workstream).

## 3) `<script>` processing model (HTML Standard)

JavaScript execution must follow the HTML Standard’s script processing model.
The most important early behaviors to preserve:

- **Parser-inserted classic scripts**: pause parsing, fetch/prepare the script, run it, then resume parsing.
- **`defer` classic scripts**: run after parsing completes (before “document ready” milestones).
- **`async` classic scripts**: run when ready, independent of parser progress (scheduled as tasks).
- **Module scripts**: later (requires module graph + host module hooks).

Correctness requirements that fall out of this:

- scripts must be able to observe/modify the partially-built DOM during parsing,
- running a script must be followed by a **microtask checkpoint**,
- script execution must interact with the event loop/task queues (async scripts, network, timers).

## 4) Event loop + microtasks

FastRender needs an HTML-shaped event loop model:

- one or more **task queues** (start with a single queue; split by “task source” later),
- a **microtask queue** for Promise jobs / `queueMicrotask`,
- explicit microtask checkpoint points (not “whenever convenient”).

Minimum semantics to preserve early:

- after running any script or task callback, run a microtask checkpoint until the microtask queue is empty,
- microtasks can schedule more microtasks (drain until stable),
- tasks scheduled during a task run should not run until the next task turn.

## 5) Web IDL bindings: how DOM/web APIs are exposed

Hand-authoring JS bindings does not scale. The binding surface should be **Web IDL-shaped**:

- Parse IDL from spec sources (e.g. WHATWG DOM/HTML/WebIDL).
- Generate deterministic Rust glue:
  - JS-visible prototype chains and property descriptors,
  - argument conversions and overload resolution,
  - exception mapping (Web IDL exceptions → JS throws),
  - exposure rules (`[Exposed=Window]`, etc.).

The goal is that adding a new web API looks like:

1. pick the spec IDL + algorithms,
2. implement host-side behavior in Rust,
3. regenerate bindings,
4. add targeted tests (WPT subset / fixtures).

## 6) Timers, Promises, and the job queue

The initial “web platform” primitives that unlock real sites:

- `setTimeout`/`setInterval` (tasks scheduled into the event loop),
- Promise job queue integration with the microtask queue,
- `queueMicrotask`.

Implementation constraint: timers must be deterministic under tests (time should be controlled by the harness, not wall-clock time).

## 7) URL + fetch (incremental)

FastRender already needs a network stack for document and subresource loading.
JS support adds a second layer:

- expose WHATWG URL parsing/serialization via `URL` / `URLSearchParams`,
- expose Fetch (`fetch()`, `Request`, `Response`, `Headers`) incrementally on top of the existing loader.

Fetch is a large spec; the goal is to stay spec-shaped and grow coverage, not to “fake it” with ad-hoc behavior.

## 8) Safety non-negotiables (JS is hostile input)

These are requirements, not optimizations:

1. **Interrupts/budgets**: JS execution must be interruptible so `while(true){}` cannot hang the process.
   Budgets can be instruction-count based, wall-time based, or both, but must be enforced predictably.
2. **Bounded allocations**: DOM wrappers, strings, arrays/typed arrays, and caches must be bounded or governed
   by the renderer’s resource limits. Avoid unbounded growth from hostile scripts.
3. **Deterministic tests**: conformance tests must be offline and stable; event loop time and scheduling must be controllable.

When tradeoffs are required, prefer a smaller, correct, budgeted subset over an unbounded “mostly works” implementation.

