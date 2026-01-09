# JavaScript support (full JS + web APIs)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

AGENTS.md is the law. These rules are not suggestions. Violating them destroys host machines, wastes hours of compute, and blocks other agents. Non-compliance is unacceptable.

**MANDATORY (no exceptions):**
- Use `scripts/cargo_agent.sh` for ALL cargo commands (build, test, check, clippy)
- Use `scripts/run_limited.sh --as 64G` when executing ANY renderer binary
- Scope ALL test runs (`-p <crate>`, `--test <name>`, `--lib`) — NEVER run unscoped tests

**FORBIDDEN — will destroy the host:**
- `cargo build` / `cargo test` / `cargo check` without wrapper scripts
- `cargo test --all-features` or `cargo check --all-features --tests`
- Unscoped `cargo test` (compiles 300+ test binaries and blows RAM)

If you do not understand these rules, re-read AGENTS.md. There are no exceptions. Ignorance is not an excuse.

---

This workstream turns FastRender into a real browser engine: **HTML + CSS + DOM + JavaScript**.

We will use **`ecma-rs`** (this org's JS/TS tooling repo) as the JavaScript language implementation
and evolve it as-needed for browser-grade execution.

Contributor-facing guide (architecture + workflow): [`docs/js_embedding.md`](../docs/js_embedding.md).

## Repos + resources (keep them local)

### JS engine source

- `engines/ecma-rs/` — git submodule (`https://github.com/wilsonzlin/ecma-rs.git`)
  - Submodule workflow is documented in `instructions/ecma_rs.md`.

### Specs (offline references)

These are optional submodules under `specs/` so you can grep normative text locally:

- `specs/tc39-ecma262/` — ECMAScript spec (language semantics, job queues, modules, host hooks)
- `specs/whatwg-html/` — HTML Standard (script processing model, event loop integration, navigation)
- `specs/whatwg-dom/` — DOM Standard (nodes, mutation, events foundation)
- `specs/whatwg-webidl/` — Web IDL (binding rules, conversions, exposure)
- `specs/whatwg-url/` — URL Standard (URL parsing/serialization, origin, URLSearchParams)
- `specs/whatwg-fetch/` — Fetch (request/response, CORS, caching policy surface)

Initialize top-level submodules with:

```bash
git submodule update --init
```

## What “full JS support” means (scope)

At minimum, “full JS support” means:

- Execute author scripts from `<script>` elements with the **HTML script processing model**:
  - classic scripts, plus module scripts later,
  - `async`/`defer` ordering rules,
  - parser-inserted vs dynamically inserted scripts,
  - microtask checkpoints after script execution.
- A browser “host environment”:
  - realms / globals (`Window`), intrinsics, and host-defined hooks (especially for modules),
  - an event loop with task queues + microtask queue.
- DOM + web APIs exposed to JS via Web IDL bindings:
  - `Window`, `Document`, `Node`, `Element`, `EventTarget`, events
  - timers (`setTimeout`/`setInterval`), `queueMicrotask`, Promises integration
  - URL, fetch/network (incremental)

We do **not** need to match Chrome immediately, but we must be **spec-shaped** and steadily expand
coverage.

## Non-negotiables for JS execution

JavaScript is hostile input. The JS engine must be safe to run:

- **Interruptible**: implement an execution budget (wall-time and/or instruction count) and a
  well-defined interrupt mechanism so `while(true){}` cannot hang the process.
- **Bounded host allocations**: DOM objects, strings, arrays, typed arrays, and caches must be
  bounded or subject to GC/memory limits.
- **Deterministic tests**: conformance fixtures must be offline and stable; avoid “live web” as the
  primary correctness gate.

## Architecture: how JS plugs into the renderer

### A) Keep the pipeline staged, but allow JS to mutate DOM

FastRender today is roughly:

fetch → parse HTML → parse CSS → cascade/compute → box tree → layout → paint

With JS:

- JS runs **during** and **after** parsing, can mutate DOM/styles, and can schedule tasks.
- Therefore the renderer needs a **document model** that can be mutated and can trigger:
  - style invalidation,
  - layout invalidation,
  - paint invalidation.

Start simple:

- Run scripts, allow DOM mutations, then do a full re-style/re-layout/repaint for the next frame.
- Optimize later with incremental invalidation.

### B) Web IDL bindings are mandatory scaffolding

Hand-writing every DOM binding is a trap. Use Web IDL as the source of truth:

- Parse the Web IDL in `specs/whatwg-dom/` (and other IDL sources as we add them).
- Generate:
  - JS-visible class definitions (names, prototypes, attributes/ops),
  - argument conversions (including `optional`, union types, sequences),
  - exception mapping (Web IDL exceptions to JS throws),
  - exposure rules (`[Exposed=Window]`, etc.).

Implementation strategy:

- Add a small **IDL → Rust binding generator** tool in this repo (or in `ecma-rs` if that’s a
  better home), and commit the generated Rust glue deterministically.

Contributor workflow details (WebIDL extraction/codegen, determinism, committed snapshot): see
[`docs/webidl_bindings.md`](../docs/webidl_bindings.md).

### C) Event loop + microtasks

Implement a minimal event loop model aligned with the HTML Standard terminology:

- task queues (at least: “DOM manipulation”, “user interaction”, “networking” buckets later),
- microtask queue (Promises / `queueMicrotask`),
- microtask checkpoint rules (especially after script execution).

This is foundational for correctness on real sites.

## Milestones (pragmatic order)

1. **Engine embed**: compile/link FastRender with an `ecma-rs` crate (start with parsing + a stub
   runtime boundary).
2. **JS runtime MVP** (likely added to `ecma-rs` as new crate(s)):
   - execution of a script body,
   - basic types/objects, property model, functions/closures,
   - exception handling, stack traces (basic),
   - interrupts/budgeting.
3. **HTML `<script>` classic execution**:
   - fetch/extract script text,
   - run in the document realm,
   - ordering + `defer`/`async` rules (incrementally),
   - microtask checkpoints.
4. **DOM core bindings**:
   - `EventTarget`, `Node`, `Element`, `Document`, `Window`,
   - events + dispatch,
   - basic mutation + re-render loop integration.
5. **Promises + timers**:
   - promise jobs/microtasks,
   - `setTimeout`/`setInterval` and task scheduling.
6. **Modules**:
   - host hooks (`HostResolveImportedModule`, dynamic import),
   - module graph + caching,
   - import maps later (optional).
7. **Fetch/URL integration** (enough for real sites):
   - `fetch()`, `Request`, `Response`, `Headers`,
   - URL parsing/serialization consistent with WHATWG URL.

## Scaffolding we should add (repo tools + structure)

To keep velocity high and avoid hand-written glue, plan to add:

### In FastRender (this repo)

- **JS host integration module** (suggested home: `src/js/`):
  - script fetching/decoding glue (`<script src>` / inline),
  - realm/global wiring (e.g. `Window` object),
  - event loop/task queue + microtask checkpoint integration,
  - DOM/Web API “host functions” implemented in Rust.
- **IDL-driven bindings pipeline**:
  - a small tool (likely under `xtask/` or `tools/`) that:
    - parses Web IDL from `specs/whatwg-*/`,
    - generates deterministic Rust glue (traits + dispatch tables),
    - keeps generated output stable for easy diffs.
- **Curated JS conformance runners** (prefer `cargo xtask …` subcommands):
  - `xtask js test262 …` to run a curated subset (fast, offline),
  - `xtask js wpt-dom …` to run a curated set of WPT `testharness.js` DOM tests
    (once we have a minimal harness).

### In `ecma-rs` (engine repo)

`ecma-rs` already provides parsing/semantics infrastructure; for browser execution we will likely
add:

- **A runtime/VM crate** (e.g. `vm-js`):
  - bytecode or AST interpreter,
  - GC/object model,
  - built-ins + intrinsics,
  - interrupts/budgeting hook (required).
- **A web host adapter crate** (e.g. `host-web`):
  - module loading hooks,
  - timers/job queue integration,
  - `TextDecoder`/encoding utilities (later),
  - bridging to FastRender’s DOM types (via traits or FFI-safe handles).

## Testing strategy (avoid bottlenecks, still be rigorous)

- **ECMAScript core**: use `test262` as the primary oracle for language semantics.
  - Start with a curated subset; scale up.
- **Web APIs**: use a curated subset of WPT (`testharness.js`) for DOM/event-loop behaviors.
  - Do not try to run all of WPT early; pick targeted tests that map to the primitives you’re
    implementing.

### Running the curated test262 semantics suite

See [`docs/js_test262.md`](../docs/js_test262.md) for the full workflow.

Quickstart:

```bash
git submodule update --init engines/ecma-rs
git -C engines/ecma-rs submodule update --init test262-semantic/data

# Use the mandatory cargo wrapper (AGENTS.md):
scripts/cargo_agent.sh xtask js test262
```

The point of tests is to prevent regressions, not to build heavy harness infrastructure.
