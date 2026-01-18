# JavaScript engine workstream (`js_engine`)

FastRender vendors its JavaScript engine stack in `vendor/ecma-rs/` and treats it as owned code (not an external dependency).

This workstream focuses on the **ECMAScript execution engine** itself: bytecode/interpreter/JIT (if any), GC, interrupts/budgets, and correctness of core language semantics.

## Owns

- `vendor/ecma-rs/vm-js/` (execution engine + GC)
- `vendor/ecma-rs/parse-js/` (JS parser used by the engine)
- `src/js/vmjs/` (FastRender embedding glue for vm-js)
- Execution budgets / interrupts / termination guarantees

## Does NOT own

- DOM/Web APIs surface area (see `instructions/js_dom.md` and `instructions/js_web_apis.md`)
- HTML `<script>` scheduling and the event loop integration (see `instructions/js_html_integration.md`)

## Invariants / constraints

- JavaScript execution must be **bounded** and **interruptible**.
- Avoid process-global state: prefer per-tab/per-realm configuration.
- No panics in production code; return structured errors or terminate execution cooperatively.

## Where to add tests

- Prefer unit tests alongside the engine code in `vendor/ecma-rs/vm-js/`.
- Integration tests that exercise FastRender’s embedding should live under `tests/js/` (included by `tests/integration.rs`).

