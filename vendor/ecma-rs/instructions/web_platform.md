# Workstream: Web Platform Integration (host hooks, modules)

---

**STOP. All code in this project is hostile. Read [`../AGENTS.md`](../AGENTS.md) first.**

**Every command requires `timeout -k` — module loading can trigger infinite JS:**

```bash
# ALWAYS use this format (no exceptions):
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl-vm-js --lib
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh build -p runtime-js

# NEVER run without timeout:
bash vendor/ecma-rs/scripts/cargo_agent.sh test ...  # WRONG
cargo test  # WRONG
```

If a command times out, that's a bug to investigate — not a limit to raise.

---

This workstream owns the **integration points between vm-js and browser hosts**: host hooks, module loading, Promise job scheduling, and WebIDL support.

**This is FastRender's second-highest priority ecma-rs workstream.** These integration points are how FastRender connects the JS engine to the DOM and Web APIs.

## The job

Make vm-js a **clean, embeddable JavaScript engine** that browsers can integrate with their own event loops, DOM implementations, and Web APIs.

## What counts

A change counts if it lands at least one of:

- **Host hook implementation**: A host hook is added or improved.
- **Module loading**: Module resolution or loading is improved.
- **Promise integration**: Promise job scheduling works correctly with host event loops.
- **WebIDL support**: WebIDL type conversion or binding generation is improved.

## Scope

### Owned by this workstream

**Host hooks (ECMA-262 §8.6):**
- `HostEnqueuePromiseJob` — Promise job scheduling
- `HostResolveImportedModule` — Module resolution
- `HostImportModuleDynamically` — Dynamic import()
- `HostGetImportMetaProperties` — import.meta
- `HostFinalizeImportMeta` — import.meta finalization
- `HostMakeJobCallback` / `HostCallJobCallback` — Job callbacks

**VmHostHooks trait:**
- Define clean trait for host integration
- Default implementations where sensible
- Documentation for implementors

**Module loading:**
- Module record types (Source Text Module, Synthetic Module)
- Module resolution algorithm
- Module graph and caching
- Circular dependency handling
- Top-level await

**Promise job queue:**
- Job enqueueing API
- GC safety for queued jobs (jobs must root their captured values)
- Integration with host microtask queues

**WebIDL support:**
- Type conversion (JS ↔ IDL types)
- Exception handling (DOMException mapping)
- Overload resolution
- Sequence/record/union types
- webidl-vm-js crate

### NOT owned (see other workstreams)

- Core JS execution → `vm_js.md`
- DOM implementation → FastRender `js_dom.md`
- Web APIs → FastRender `js_web_apis.md`

## Priority order (P0 → P1 → P2)

### P0: Core host integration (FastRender can use vm-js)

1. **VmHostHooks trait**
   - Clean, stable trait definition
   - `host_enqueue_promise_job(job, realm)` — queue Promise jobs
   - Default no-op implementations
   - Documentation

2. **Promise job GC safety**
   - Jobs must root captured values until executed
   - `Job::add_root()`, `Job::run()`, `Job::discard()`
   - Values survive GC between enqueue and execution

3. **Script execution with host**
   - `Vm::exec_script_with_host_and_hooks()` — run script with host context
   - Host context available to native functions
   - Hooks available for Promise jobs

4. **Basic module support**
   - `HostResolveImportedModule` hook
   - Static import resolution
   - Module caching

### P1: Full module support

5. **Module graph**
   - Module record representation
   - Link and evaluate phases
   - Circular dependency handling
   - Error handling during module loading

6. **Dynamic import**
   - `import()` expression support
   - `HostImportModuleDynamically` hook
   - Promise-based loading

7. **Import meta**
   - `import.meta` object
   - `HostGetImportMetaProperties` hook
   - `import.meta.url`, `import.meta.resolve()`

8. **Top-level await**
   - Async module evaluation
   - Proper handling in module graph

### P2: WebIDL and advanced integration

9. **webidl-vm-js completeness**
   - All IDL type conversions
   - Sequence types
   - Record types
   - Union types
   - Callback types
   - Overload resolution

10. **Exception mapping**
    - DOMException → JS Error
    - JS Error → DOMException
    - Stack trace preservation

11. **Synthetic modules**
    - Non-source-text modules
    - JSON modules
    - CSS modules (future)

## Implementation notes

### VmHostHooks trait

```rust
// vm-js/src/host.rs
pub trait VmHostHooks {
    /// Queue a Promise job for later execution.
    /// The job MUST root any captured Values until run/discard.
    fn host_enqueue_promise_job(&mut self, job: Job, realm: RealmId);
    
    /// Resolve a module specifier to a module.
    fn host_resolve_imported_module(
        &mut self,
        referrer: &Module,
        specifier: &str,
    ) -> Result<Module, JsError>;
    
    /// Handle dynamic import().
    fn host_import_module_dynamically(
        &mut self,
        referrer: &Module,
        specifier: &str,
    ) -> Result<Promise, JsError>;
    
    // ... other hooks
}
```

### Job GC safety

Promise jobs capture JavaScript values. These values must survive garbage collection:

```rust
pub struct Job {
    callback: Value,      // Must be rooted
    arguments: Vec<Value>, // Must be rooted
    roots: Vec<GcRoot>,   // Prevents GC
}

impl Job {
    pub fn run(self, vm: &mut Vm) -> Result<Value, JsError> {
        // Roots are released after execution
    }
    
    pub fn discard(self) {
        // Roots are released without execution
    }
}
```

FastRender's event loop owns queued jobs and must ensure they're properly rooted.

### Script execution with host

```rust
// Execution entry points that support host context
impl Vm {
    /// Execute script with host context and hooks.
    /// Native functions can downcast the host to access embedder state.
    pub fn exec_script_with_host_and_hooks<H: VmHost>(
        &mut self,
        source: &str,
        host: &mut H,
        hooks: &mut dyn VmHostHooks,
    ) -> Result<Value, JsError>;
}
```

### Testing

```bash
# Run host integration tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p vm-js --lib -- host

# Run module tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p vm-js --lib -- module

# Run webidl-vm-js tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p webidl-vm-js --lib
```

## Key files

```
vm-js/
├── src/
│   ├── host.rs             — VmHostHooks trait
│   ├── job.rs              — Promise job type
│   ├── module.rs           — Module records
│   └── ...

webidl-vm-js/
├── src/
│   ├── lib.rs              — WebIDL runtime
│   ├── convert.rs          — Type conversions
│   └── ...
```

## FastRender integration

FastRender implements `VmHostHooks` in `src/js/vmjs/window_timers.rs`:

```rust
// FastRender's host hooks route Promise jobs to the HTML event loop
impl VmHostHooks for VmJsEventLoopHooks<'_> {
    fn host_enqueue_promise_job(&mut self, job: Job, _realm: RealmId) {
        self.event_loop.queue_microtask(Microtask::PromiseJob(job));
    }
    
    fn host_resolve_imported_module(...) -> Result<Module, JsError> {
        // Delegate to FastRender's module loader
    }
}
```

## Success criteria

Web platform integration is **done** when:
- VmHostHooks trait is stable and well-documented
- Promise jobs are GC-safe (no use-after-free, no leaks)
- Module loading works (static imports, dynamic import())
- FastRender can integrate vm-js with its event loop
- webidl-vm-js handles common WebIDL patterns
