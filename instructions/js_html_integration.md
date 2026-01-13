# Workstream: JavaScript HTML Integration

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

**Every command requires `timeout -k` — script loading can trigger infinite JS:**

```bash
# ALWAYS use this format (no exceptions):
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::event_loop
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl-vm-js --lib

# NEVER run without timeout:
bash scripts/cargo_agent.sh test ...  # WRONG
cargo test  # WRONG
```

If a command times out, that's a bug to investigate — not a limit to raise.

### Build speed matters

See [`docs/build_performance.md`](../docs/build_performance.md):
- Use `cargo check` for validation
- Scope tests: `cargo test --lib js::event_loop` not `cargo test`
- For ecma-rs work: `bash vendor/ecma-rs/scripts/cargo_agent.sh check -p vm-js`

---

This workstream owns how **JavaScript integrates with HTML**: script loading, module execution, the event loop, host hooks, and the execution lifecycle.

For which public API containers currently include JavaScript execution + an event loop (and which are
render-only documents), see [`docs/runtime_stacks.md`](../docs/runtime_stacks.md).

## The job

Make scripts run **at the right time, in the right order, with the right context**—exactly as the HTML Standard specifies. Make vm-js a **clean, embeddable JavaScript engine** that integrates with FastRender's DOM and event loop.

## What counts

A change counts if it lands at least one of:

- **Processing model compliance**: Script execution follows HTML Standard ordering.
- **Module support**: ES modules work (static and dynamic import).
- **Event loop correctness**: Tasks and microtasks run in correct order.
- **Lifecycle correctness**: DOMContentLoaded, load, beforeunload fire correctly.
- **Host hook implementation**: A vm-js host hook is added or improved.
- **Promise integration**: Promise job scheduling works correctly with event loop.
- **WebIDL support**: WebIDL type conversion or binding generation is improved.

## Scope

### Owned by this workstream

**Script processing model (HTML Standard):**
- `<script>` element processing (classic and module)
- Parser-inserted vs script-inserted scripts
- `async` and `defer` attributes
- Script execution ordering
- `document.currentScript`
- `document.write()` during parsing

**Module scripts:**
- `<script type="module">`
- Static `import` declarations
- Dynamic `import()`
- Module resolution and loading
- Import maps (`<script type="importmap">`)
- Module caching

**Event loop (HTML Standard):**
- Task queues (DOM manipulation, user interaction, networking, timers)
- Microtask queue (Promises, queueMicrotask)
- Microtask checkpoints
- Rendering opportunities

**Host hooks (ECMA-262 §8.6):**
- `HostEnqueuePromiseJob` — Promise job scheduling
- `HostResolveImportedModule` — Module resolution
- `HostImportModuleDynamically` — Dynamic import()
- `HostGetImportMetaProperties` — import.meta
- `HostMakeJobCallback` / `HostCallJobCallback` — Job callbacks

**Promise job queue:**
- Job enqueueing API
- GC safety for queued jobs (jobs must root their captured values)
- Integration with microtask queue

**Page lifecycle:**
- `DOMContentLoaded` event
- `load` event (window.onload)
- `beforeunload` event
- `unload` event
- `visibilitychange` event
- `pagehide`/`pageshow` events

**Execution context:**
- Window global object
- Realm setup
- `this` binding in scripts
- Strict mode handling

**WebIDL support:**
- Type conversion (JS ↔ IDL types)
- Exception handling (DOMException mapping)
- Overload resolution
- Sequence/record/union types
- webidl-vm-js crate

For the consolidated WebIDL crate layout and ownership boundaries (what belongs in `vendor/ecma-rs/`
vs `src/js/`), see [`docs/webidl_stack.md`](../docs/webidl_stack.md).

### NOT owned (see other workstreams)

- JavaScript language execution → `js_engine.md`
- DOM APIs → `js_dom.md`
- Web APIs (fetch, timers) → `js_web_apis.md`

## Priority order (P0 → P1 → P2 → P3)

### P0: Scripts execute (basic page JS works)

1. **Inline classic scripts**
   - `<script>code</script>` executes at parse time
   - Scripts see the DOM up to their position
   - `document.currentScript` is set correctly
   - Errors don't break parsing

2. **External classic scripts**
   - `<script src="url">` fetches and executes
   - Parser blocks until script completes
   - Correct base URL for relative references
   - Error handling (network failure, parse error)

3. **Event loop basics**
   - Task queue processes tasks in order
   - Microtask queue drains after each task
   - Promise `.then()` runs as microtasks
   - `queueMicrotask()` works

4. **VmHostHooks trait (vm-js side)**
   - Clean, stable trait definition
   - `host_enqueue_promise_job(job, realm)` — queue Promise jobs
   - Default no-op implementations
   - Documentation

5. **Promise job GC safety**
   - Jobs must root captured values until executed
   - Values survive GC between enqueue and execution
   - No use-after-free, no leaks

6. **DOMContentLoaded**
   - Fires after parsing completes
   - Fires after all deferred scripts run
   - `document.readyState` transitions correctly

### P1: Script ordering (complex pages work)

7. **async scripts**
   - `<script async src="url">` doesn't block parser
   - Executes as soon as fetched (unordered relative to other async)
   - Still blocks at its execution point

8. **defer scripts**
   - `<script defer src="url">` doesn't block parser
   - Executes after parsing, before DOMContentLoaded
   - Executes in document order

9. **Dynamic script insertion**
   - `document.createElement('script')` + appendChild
   - Dynamic scripts are async by default
   - `script.async = false` preserves insertion order

10. **document.write()**
    - Works during parsing (inserts into token stream)
    - No-op or warning after parsing
    - Handles nested document.write()

11. **Basic module support**
    - `HostResolveImportedModule` hook
    - Static import resolution
    - Module caching

### P2: Modules (modern JS works)

12. **Module scripts (static)**
    - `<script type="module">` runs as module
    - Modules are always deferred
    - Top-level await support
    - Strict mode by default

13. **Module graph**
    - Module record representation
    - Link and evaluate phases
    - Circular dependency handling
    - Error handling during module loading

14. **Dynamic imports**
    - `import('./module.js')` returns Promise
    - Works from classic and module scripts
    - `HostImportModuleDynamically` hook

15. **Import meta**
    - `import.meta` object
    - `HostGetImportMetaProperties` hook
    - `import.meta.url`, `import.meta.resolve()`

16. **Import maps**
    - `<script type="importmap">` parsed and applied
    - Bare specifier resolution (`import 'lodash'`)
    - Scoped mappings
    - Integrity hashes

### P3: Advanced

17. **WebIDL completeness**
    - All IDL type conversions
    - Sequence/record/union types
    - Callback types
    - Overload resolution

18. **Exception mapping**
    - DOMException → JS Error
    - JS Error → DOMException
    - Stack trace preservation

19. **load event**
    - `window.onload` / `addEventListener('load')`
    - Fires after all resources (images, stylesheets) loaded

20. **beforeunload/unload**
    - Navigation interception
    - Cleanup handlers

21. **Error handling**
    - `window.onerror` / `addEventListener('error')`
    - `unhandledrejection` event for Promises

## Implementation notes

### VmHostHooks trait (vm-js)

```rust
// vendor/ecma-rs/vm-js/src/host.rs
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
    callback: Value,       // Must be rooted
    arguments: Vec<Value>, // Must be rooted
    roots: Vec<GcRoot>,    // Prevents GC
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

### FastRender host hooks implementation

```rust
// src/js/vmjs/window_timers.rs
impl VmHostHooks for VmJsEventLoopHooks<'_> {
    fn host_enqueue_promise_job(&mut self, job: Job, _realm: RealmId) {
        self.event_loop.queue_microtask(Microtask::PromiseJob(job));
    }
    
    fn host_resolve_imported_module(&mut self, referrer: &Module, specifier: &str) 
        -> Result<Module, JsError> {
        // 1. Apply import maps
        // 2. Resolve URL relative to referrer
        // 3. Check module cache
        // 4. Fetch and parse if needed
    }
}
```

### Event loop

The event loop follows HTML Standard terminology:

```rust
pub struct EventLoop<Host> {
    // Runnable work:
    // - task queues (TaskSource::*),
    // - microtask queue (Promise jobs, queueMicrotask),
    // - timers (setTimeout/setInterval) promoted into tasks when due,
    // - requestIdleCallback callbacks (dispatched as tasks when the loop is otherwise idle).
    //
    // Frame callbacks (requestAnimationFrame) are queued separately and must be driven by the host
    // container's frame/tick loop (e.g. BrowserTab::tick_frame / run_until_stable).
    task_queues: ...,
    microtask_queue: ...,
    timers: ...,
    animation_frame_callbacks: ...,
    idle_callbacks: ...,
}

impl<Host> EventLoop<Host> {
    fn run_until_idle(&mut self, host: &mut Host, limits: RunLimits) {
        loop {
            // 1. Promote due timers / idle callbacks into tasks
            // 2. Run one task turn (a task + post-task microtask checkpoint)
            // 3. If the task queue is empty, the loop is idle
        }
    }
}
```

### Script scheduler

The script scheduler produces actions for the parser:

```rust
pub enum ScriptSchedulerAction {
    FetchExternal { url, ... },
    BlockParser,
    UnblockParser,
    ExecuteNow { script },
    QueueForLater { script },
}
```

### Architecture

```
src/js/
  html_script_processing.rs  — HTML script processing model
  script_scheduler.rs        — Script scheduling and ordering
  html_classic_scripts.rs    — Classic script handling
  streaming.rs               — Parse-time script handling
  event_loop.rs              — tasks/microtasks/timers + rAF + requestIdleCallback queues
  import_maps/               — Import map parsing and resolution
  realm_module_loader.rs     — Module loading (resolution + fetch + budgets; BrowserTab + vm-js ModuleGraph)
  vmjs/module_loader.rs      — vm-js embedding glue (`VmJsModuleLoader`)

vendor/ecma-rs/
  vm-js/src/host.rs          — VmHostHooks trait
  vm-js/src/job.rs           — Promise job type
  vm-js/src/module.rs        — Module records
  webidl-vm-js/              — WebIDL runtime and type conversions
```

### Testing

```bash
# Script processing tests
# Script-processing tests are unit tests (live in `src/`), so run them via `--lib` with a
# module-qualified filter.
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::html_script_processing

# If you need to run a true integration test that exercises script processing end-to-end, it must
# live under the unified integration-test binary (`tests/integration.rs`):
# timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --test integration <filter>

# Event loop tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::event_loop

# Module tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::import_maps

# Host integration tests (vm-js)
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib -- host

# WebIDL tests
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p webidl-vm-js --lib
```

### Key documents

- `docs/html_script_processing.md` — Detailed script processing design
- `docs/import_maps.md` — Import map support
- `docs/js_embedding.md` — Overall JS embedding guide

## Success criteria

HTML integration is **done** when:
- Scripts execute in correct order (parser-blocking, async, defer, modules)
- VmHostHooks trait is stable and well-documented
- Promise jobs are GC-safe (no use-after-free, no leaks)
- Microtasks run at correct checkpoints (after scripts, after tasks)
- DOMContentLoaded and load fire at correct times
- ES modules work with static and dynamic imports
- Import maps resolve bare specifiers
- webidl-vm-js handles common WebIDL patterns
- Real-world sites with complex script loading work correctly
