# Workstream: vm-js Runtime (FastRender browser JS)

---

**STOP. Read [`../AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

JavaScript is hostile input. **Any script can hang, explode memory, or refuse to terminate.**

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- Memory limits via `cargo_agent.sh` wrapper
- Scoped test runs (`-p <crate>`, `--test <name>`)

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands (from ecma-rs root)
- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh ...` for commands from FastRender root

---

This workstream owns the **vm-js JavaScript runtime**: execution, garbage collection, built-in objects, and spec compliance.

**This is FastRender's highest priority ecma-rs workstream.** vm-js is the JavaScript engine that powers browser script execution.

## The job

Make vm-js a **production-quality JavaScript engine** that can execute real-world browser scripts reliably and efficiently.

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
- Variables (var, let, const, hoisting)
- Functions (declaration, expression, arrow, generator, async)
- Classes (declaration, expression, inheritance, private fields)
- Destructuring and spread
- Iterators and for...of
- Template literals
- Optional chaining and nullish coalescing
- Modules (import/export syntax support)

**Safety:**
- Instruction count budgets
- Wall-clock time limits
- Interrupt/cancel mechanism
- Stack overflow prevention

### NOT owned (see other workstreams)

- Host hooks and module loading → `web_platform.md`
- TypeScript type checking → `ts_typecheck.md`
- Native AOT compilation → `native_aot.md`

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

From FastRender root:
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

## Key files

```
vm-js/
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

## Success criteria

vm-js is **done for FastRender** when:
- 95%+ of test262 core tests pass
- No crashes or hangs from valid JavaScript
- Execution budgets reliably prevent runaway scripts
- Memory limits prevent OOM from JavaScript allocations
- Real-world browser scripts execute correctly
- Performance is acceptable for interactive use
