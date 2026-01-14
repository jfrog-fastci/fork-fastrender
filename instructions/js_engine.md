# Workstream: JavaScript Engine (vm-js)

---

**STOP. JavaScript is hostile input. Read [`AGENTS.md`](../AGENTS.md) first.**

**Every command requires `timeout -k` — JS execution can `while(true){}` forever:**

```bash
# ALWAYS use this format (no exceptions):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh build -p vm-js

# NEVER run without timeout (tests execute arbitrary JS):
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib  # WRONG
cargo test  # WRONG
```

If a command times out, that's a bug to investigate — not a limit to raise.

### vm-js generator/yield integration smoke tests

CI runs a focused subset of vm-js integration tests that stress generator + `yield` correctness.
Run the same suite locally with:

```bash
bash scripts/run_vm_js_generator_yield_tests.sh
```

This script is safe to run directly: it wraps each `cargo test` invocation in `timeout -k`.

This executes (at minimum):
- `generators_yield_operators`
- `generators_delete_yield`
- `generators_binary_ops_yield`
- `generators_destructuring_assignment_yield`

### Build speed matters

vm-js builds are relatively fast (separate workspace), but still:
- Use `cargo check -p vm-js` for validation when possible
- Use `--lib` to avoid building binaries you don't need
- See [`docs/build_performance.md`](../docs/build_performance.md) for general guidance

### CI guardrail: no unresolved merge conflict markers

Before pushing changes that touch vendored JS engine crates, run:

```bash
bash scripts/check_no_conflict_markers.sh
```

This check is enforced in CI and scans the repo’s tracked Rust sources (including `vendor/ecma-rs/{vm-js,parse-js,semantic-js,test262-semantic}/src`).
Known upstream fixtures under `vendor/ecma-rs/parse-js/tests/TypeScript/**` are allowed to contain conflict-marker strings.

---

This workstream owns the **vm-js JavaScript runtime**: execution, garbage collection, built-in objects, and ECMA-262 spec compliance.

**This is FastRender's highest priority JavaScript workstream.** vm-js is the engine that powers browser script execution.

## The job

Make vm-js a **production-quality JavaScript engine** that can execute real-world browser scripts reliably and efficiently. Not a toy. Not "mostly works." A real engine.

## Relationship to ecma-rs

FastRender **owns ecma-rs** (vendored at `vendor/ecma-rs/`). This workstream drives vm-js development directly. Make whatever changes are needed. Don't wait for upstream. Move fast.

Key directories:
- `vendor/ecma-rs/vm-js/` — The JavaScript runtime
- `vendor/ecma-rs/parse-js/` — JavaScript parser
- `vendor/ecma-rs/semantic-js/` — Semantic analysis

## What counts

A change counts if it lands at least one of:

- **Spec compliance**: A JavaScript feature now works per ECMA-262.
- **Performance**: Execution is measurably faster.
- **Robustness**: A crash, hang, or incorrect behavior is fixed.
- **Safety**: Execution budgets, interrupts, or memory limits are improved.
- **test262 progress**: More test262 tests pass.

## Scope

### Owned by this workstream

**Execution:**
- Call stack and execution contexts
- Function calls (regular, method, constructor)
- Closures and scope chains
- `this` binding rules
- Strict mode
- Evaluation (direct/indirect eval)

**Memory management:**
- Garbage collection
- Heap management
- Memory limits and OOM handling
- Object allocation

**Built-in objects:**
- Object, Array, Function
- String, Number, Boolean, Symbol, BigInt
- Map, Set, WeakMap, WeakSet
- Date, RegExp, Math, JSON
- Error types (Error, TypeError, RangeError, etc.)
- Typed arrays and ArrayBuffer
- Promise (execution model)
- Proxy and Reflect

**Language features:**
- Variables (var, let, const, hoisting, TDZ)
- Functions (declaration, expression, arrow, generator, async)
- Classes (declaration, expression, inheritance, private fields)
- Destructuring and spread
- Iterators and for...of
- Template literals
- Optional chaining and nullish coalescing
- Modules (import/export syntax support)

**Safety:**
- Fuel ("tick") budgets (`Budget.fuel`, defaults via `VmOptions.default_fuel`)
- Wall-clock deadlines (`Budget.deadline`, defaults via `VmOptions.default_deadline`)
- Interrupt/cancel mechanism
- Stack overflow prevention
- No resource leaks on termination

### NOT owned (see other workstreams)

- DOM APIs (document, window, etc.) → `js_dom.md`
- Web APIs (fetch, URL, timers) → `js_web_apis.md`
- HTML script processing (<script>, modules, event loop) → `js_html_integration.md`

## Priority order (P0 → P1 → P2)

### P0: Reliability (scripts don't crash the browser)

1. **Execution budgets**
   - Fuel ("tick") limits (`Budget.fuel` / `VmOptions.default_fuel`)
   - Wall-clock deadlines (`Budget.deadline` / `VmOptions.default_deadline`)
   - Clean interrupt mechanism
   - No resource leaks on termination

2. **Memory safety**
   - Heap size limits (`HeapLimits`)
   - GC triggers under memory pressure
   - No unbounded allocations from JS
   - Clean OOM handling (throw, don't panic)

3. **Error handling**
   - All JS errors are catchable (no Rust panics from JS)
   - Accurate stack traces
   - Helpful error messages
   - Proper exception propagation

4. **Core language correctness**
   - Variables work correctly (var hoisting, let/const TDZ)
   - Functions work correctly (this binding, closures)
   - Objects work correctly (prototype chain, property access)
   - Arrays work correctly (length, holes, methods)

### P1: Completeness (real scripts work)

5. **Classes**
   - Class declarations and expressions
   - Constructors and super()
   - Static and instance methods
   - Getters and setters
   - Private fields and methods (#field)
   - Inheritance (extends)

6. **Async/await and Promises**
   - Promise construction (new Promise, Promise.resolve/reject)
   - Promise chaining (.then, .catch, .finally)
   - Promise combinators (all, race, allSettled, any)
   - async functions
   - await expressions
   - Top-level await (in modules)

7. **Iterators and generators**
   - Iterator protocol
   - for...of loops
   - Spread operator in arrays and calls
   - Generator functions (function*)
   - yield and yield*
   - Async generators

8. **Built-in completeness**
   - Array: all methods (map, filter, reduce, find, flat, etc.)
   - String: all methods (slice, split, replace, match, etc.)
   - Object: keys, values, entries, assign, fromEntries, etc.
   - RegExp: full spec compliance
   - Date: construction, methods, formatting
   - Math: all methods
   - JSON: parse, stringify with replacer/reviver

### P2: Advanced features

9. **Proxies and Reflect**
   - All Proxy traps
   - Reflect methods
   - Revocable proxies

10. **WeakRef and FinalizationRegistry**

11. **Realms and compartments** (for browser security)

12. **Performance optimizations**
    - Inline caching
    - Hidden classes / shapes
    - Polymorphic inline caches
    - (JIT compilation is longer-term)

## test262 as oracle

ECMA-262 test262 is the authoritative conformance suite.

### Running test262

```bash
# Initialize submodule
git submodule update --init vendor/ecma-rs/test262-semantic/data

 # Run curated suite via FastRender xtask
 timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262
 
 # Some environments may require a larger OS stack to run the full suite without crashing:
 # LIMIT_STACK=64M timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262
 
 # Or run directly
 timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p test262-semantic --lib
 ```

### Tracking progress

- Track total passing tests
- Track pass rate per feature area
- No regressions without explicit justification
- Target: 95%+ of "core" test262 tests passing

## Implementation notes

### Key vm-js types

Key public API entry points live in `vendor/ecma-rs/vm-js/src/lib.rs`:

- `Vm`, `VmOptions` — VM + construction-time configuration (default budgets, interrupts, max stack depth, …)
- `Budget` — per-run execution budget (fuel and/or wall-clock deadline)
- `Heap`, `HeapLimits`, `Scope`, `RootId` — GC heap + rooting
- `JsRuntime` — convenience wrapper around `{ vm, heap }` with `exec_script*` entry points
- `Value`, `GcObject`, `GcString`, … — JS values/handles

#### Budgets (fuel + deadline)

```rust
use std::time::{Duration, Instant};
use vm_js::{Budget, Vm, VmOptions};

// Defaults for a long-lived VM (applied by `Vm::reset_budget_to_default()`).
let mut vm = Vm::new(VmOptions {
  default_fuel: Some(1_000_000),
  // `default_deadline` is relative to when the budget is reset.
  default_deadline: Some(Duration::from_millis(50)),
  check_time_every: 100,
  ..VmOptions::default()
});

// Per-task override (absolute `Instant` deadline).
vm.set_budget(Budget {
  fuel: Some(100_000),
  deadline: Some(Instant::now() + Duration::from_millis(10)),
  check_time_every: 1,
});

// ... run a script/job ...

// Restore the construction defaults (refreshes the deadline relative to "now").
vm.reset_budget_to_default();
```

### Key files

```
vendor/ecma-rs/vm-js/
├── src/
│   ├── lib.rs              — Public API
│   ├── vm.rs               — VM struct, execution entry points
│   ├── heap.rs             — GC heap
│   ├── value.rs            — JavaScript values
│   ├── object.rs           — JavaScript objects
│   ├── builtins/           — Built-in objects (Array, String, etc.)
│   ├── execution/          — Execution contexts, call stack
│   └── ...
├── tests/
│   └── ...
```

### RegExp `/v` Unicode string properties

vm-js uses generated Unicode “property of strings” tables (emoji sequences) to support RegExp `/v`
escapes such as `\p{RGI_Emoji}`. See:

- Overview + regeneration workflow:
  - `vendor/ecma-rs/vm-js/docs/regexp_unicode.md`
- Deep dive (Unicode properties + case folding maintenance notes):
  - `vendor/ecma-rs/vm-js/docs/regexp_unicode_properties.md`

### Host hooks

vm-js uses `VmHostHooks` for browser integration:
- `host_enqueue_promise_job` — Promise job queue
- `host_resolve_imported_module` — Module loading

See `js_html_integration.md` for how FastRender implements these.

### Testing

```bash
# Run vm-js tests
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib

# Run specific test
timeout -k 10 300 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib -- test_name
```

## Success criteria

The JavaScript engine is **done** when:
- 95%+ of test262 core tests pass (language features, built-ins)
- No crashes or hangs from valid JavaScript
- Execution budgets reliably prevent runaway scripts
- Memory limits prevent OOM from JavaScript allocations
- Real-world browser scripts execute correctly
- Performance is acceptable for interactive use
