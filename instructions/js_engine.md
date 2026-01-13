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

### Build speed matters

vm-js builds are relatively fast (separate workspace), but still:
- Use `cargo check -p vm-js` for validation when possible
- Use `--lib` to avoid building binaries you don't need
- See [`docs/build_performance.md`](../docs/build_performance.md) for general guidance

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
- `vendor/ecma-rs/semantics-js/` — Semantic analysis

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
- Instruction count budgets (`RunLimits.max_instructions`)
- Wall-clock time limits (`RunLimits.max_wall_time`)
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
   - Instruction count limits (`RunLimits.max_instructions`)
   - Wall-clock time limits (`RunLimits.max_wall_time`)
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

```rust
// vendor/ecma-rs/vm-js/src/lib.rs
pub struct Vm { ... }          // JavaScript VM instance
pub struct Heap { ... }        // GC heap
pub struct Value { ... }       // JavaScript value
pub struct Object { ... }      // JavaScript object

// Execution
impl Vm {
    pub fn exec_script(&mut self, source: &str) -> Result<Value, ...>;
    pub fn call(&mut self, func: Value, this: Value, args: &[Value]) -> Result<Value, ...>;
}

// Budgets
pub struct RunLimits {
    pub max_instructions: Option<u64>,
    pub max_wall_time: Option<Duration>,
}
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
