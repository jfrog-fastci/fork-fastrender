# Workstream: JavaScript Engine (ecma-rs vm-js)

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

JavaScript is hostile input. **Any script can `while(true){}`, allocate unbounded arrays, or ignore signals.**

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL (SIGTERM can be ignored)
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh ...` for ecma-rs workspace
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

This workstream owns the **JavaScript engine core**: the vm-js runtime in ecma-rs that executes JavaScript code.

## The job

Make vm-js a **production-quality JavaScript runtime** capable of executing real-world scripts reliably. Not a toy. Not "mostly works." A real engine.

## Relationship to ecma-rs

FastRender **owns ecma-rs** (vendored at `vendor/ecma-rs/`). This workstream drives vm-js development directly. Make whatever changes are needed. Don't wait for upstream. Move fast.

Key directories:
- `vendor/ecma-rs/vm-js/` — The JavaScript runtime
- `vendor/ecma-rs/parse-js/` — JavaScript parser
- `vendor/ecma-rs/semantics-js/` — Semantic analysis

## What counts

A change counts if it lands at least one of:

- **Spec compliance**: A JavaScript language feature now works per ECMA-262.
- **Performance**: Execution is measurably faster.
- **Robustness**: A crash, hang, or incorrect behavior is fixed.
- **Safety**: Execution budgets, interrupts, or memory limits are improved.
- **Test262 progress**: More test262 tests pass.

## Scope

### Owned by this workstream

- **Core language features**: Variables, functions, closures, classes, iterators, generators
- **Built-in objects**: Object, Array, String, Number, Boolean, Symbol, BigInt, Map, Set, WeakMap, WeakSet
- **Control flow**: if/else, for, while, do-while, switch, try/catch/finally, throw
- **Operators**: Arithmetic, comparison, logical, bitwise, assignment, spread, optional chaining, nullish coalescing
- **Async**: Promises, async/await, generators
- **Modules**: import/export, dynamic import(), module resolution hooks
- **Execution**: Call stack, execution contexts, closures, this binding
- **Memory**: Garbage collection, heap management, memory limits
- **Safety**: Execution budgets, interrupts, timeouts

### NOT owned (see other workstreams)

- DOM APIs (document, window, etc.) → `js_dom.md`
- Web APIs (fetch, URL, timers) → `js_web_apis.md`
- HTML script processing (<script>, modules) → `js_html_integration.md`

## Priority order (P0 → P1 → P2)

### P0: Core reliability (scripts don't crash/hang)

1. **Execution budgets**
   - Instruction count limits
   - Wall-clock time limits
   - Interrupt/cancel mechanism
   - Clean termination (no resource leaks)

2. **Memory safety**
   - Heap size limits
   - GC under memory pressure
   - No unbounded allocations from JS
   - Clean OOM handling

3. **Error handling**
   - All errors are catchable (no panics from JS)
   - Stack traces are accurate
   - Error messages are helpful

4. **Basic spec compliance**
   - Variables (var, let, const)
   - Functions (declaration, expression, arrow)
   - Objects and arrays
   - Control flow
   - Operators

### P1: Language completeness (real scripts work)

5. **Classes**
   - Class declarations
   - Constructors
   - Methods (static, instance, getter/setter)
   - Inheritance (extends)
   - Private fields/methods

6. **Iterators and generators**
   - for...of loops
   - Spread operator
   - Generator functions
   - yield/yield*

7. **Async/await**
   - Promise creation and chaining
   - async functions
   - await expressions
   - Promise.all/race/allSettled/any

8. **Built-in completeness**
   - Array methods (map, filter, reduce, find, etc.)
   - String methods
   - Object methods (keys, values, entries, assign, etc.)
   - Math, Date, RegExp
   - JSON.parse/stringify
   - Typed arrays and ArrayBuffer

### P2: Advanced features

9. **Modules**
   - Static import/export
   - Dynamic import()
   - Module namespace objects
   - Circular dependencies

10. **Proxies and Reflect**
    - Proxy handlers
    - Reflect methods

11. **WeakRef and FinalizationRegistry**

12. **Performance**
    - Inline caching
    - Hidden classes/shapes
    - JIT compilation (longer term)

## Test262 as oracle

ECMAScript test262 is the authoritative conformance suite.

### Running test262

```bash
# Initialize submodule
git submodule update --init vendor/ecma-rs/test262-semantic/data

# Run curated suite
timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262
```

### Tracking progress

Test262 results should improve monotonically. Track:
- Total passing tests
- Passing tests per feature area
- No regressions without justification

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

### Host hooks

vm-js uses `VmHostHooks` for browser integration:
- `host_enqueue_promise_job` — Promise job queue
- `host_resolve_imported_module` — Module loading

Implement these correctly for HTML integration.

### Testing in ecma-rs

```bash
# Run vm-js tests
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib

# Run specific test
timeout -k 10 300 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib -- test_name
```

## Success criteria

The JavaScript engine is **done** when:
- 95%+ of test262 "core" tests pass (language features, built-ins)
- No known crashes or hangs from valid JavaScript
- Execution budgets reliably prevent runaway scripts
- Memory limits prevent OOM from JavaScript allocations
- Real-world scripts from popular sites execute correctly
