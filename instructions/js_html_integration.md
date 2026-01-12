# Workstream: JavaScript HTML Integration

---

**STOP. Read [`AGENTS.md`](../AGENTS.md) BEFORE doing anything.**

### Assume every process can misbehave

**Every command must have hard external limits:**
- `timeout -k 10 <seconds>` — time limit with guaranteed SIGKILL
- `bash scripts/run_limited.sh --as 64G` — memory ceiling enforced by kernel
- Scoped test runs (`-p <crate>`, `--test <name>`) — don't compile/run the universe

**MANDATORY (no exceptions):**
- `timeout -k 10 600 bash scripts/cargo_agent.sh ...` for ALL cargo commands
- `timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- ...` for renderer binaries

---

This workstream owns how **JavaScript integrates with HTML**: script loading, module execution, the event loop, and the execution lifecycle.

## The job

Make scripts run **at the right time, in the right order, with the right context**—exactly as the HTML Standard specifies.

## What counts

A change counts if it lands at least one of:

- **Processing model compliance**: Script execution follows HTML Standard ordering.
- **Module support**: ES modules work (static and dynamic import).
- **Event loop correctness**: Tasks and microtasks run in correct order.
- **Lifecycle correctness**: DOMContentLoaded, load, beforeunload fire correctly.

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

### NOT owned (see other workstreams)

- JavaScript language execution → `js_engine.md`
- DOM APIs → `js_dom.md`
- Web APIs (fetch, timers) → `js_web_apis.md`

## Priority order (P0 → P1 → P2)

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

4. **DOMContentLoaded**
   - Fires after parsing completes
   - Fires after all deferred scripts run
   - `document.readyState` transitions correctly

### P1: Script ordering (complex pages work)

5. **async scripts**
   - `<script async src="url">` doesn't block parser
   - Executes as soon as fetched (unordered relative to other async)
   - Still blocks at its execution point

6. **defer scripts**
   - `<script defer src="url">` doesn't block parser
   - Executes after parsing, before DOMContentLoaded
   - Executes in document order

7. **Dynamic script insertion**
   - `document.createElement('script')` + appendChild
   - Dynamic scripts are async by default
   - `script.async = false` preserves insertion order

8. **document.write()**
   - Works during parsing (inserts into token stream)
   - No-op or warning after parsing
   - Handles nested document.write()

### P2: Modules (modern JS works)

9. **Module scripts (static)**
   - `<script type="module">` runs as module
   - Modules are always deferred
   - Top-level await support
   - Strict mode by default

10. **Static imports**
    - `import { x } from './module.js'`
    - Module graph resolution
    - Circular dependency handling
    - Module caching (execute once)

11. **Dynamic imports**
    - `import('./module.js')` returns Promise
    - Works from classic and module scripts
    - Honors import maps

12. **Import maps**
    - `<script type="importmap">` parsed and applied
    - Bare specifier resolution (`import 'lodash'`)
    - Scoped mappings
    - Integrity hashes

### P3: Advanced lifecycle

13. **load event**
    - `window.onload` / `addEventListener('load')`
    - Fires after all resources (images, stylesheets) loaded

14. **beforeunload/unload**
    - Navigation interception
    - Cleanup handlers

15. **Page visibility**
    - `document.visibilityState`
    - `visibilitychange` event

16. **Error handling**
    - `window.onerror` / `addEventListener('error')`
    - `unhandledrejection` event for Promises

## Implementation notes

### Architecture

```
src/js/
  html_script_processing.rs  — HTML script processing model
  script_scheduler.rs        — Script scheduling and ordering
  html_classic_scripts.rs    — Classic script handling
  streaming.rs               — Parse-time script handling
  event_loop.rs              — Task/microtask queues
  import_maps/               — Import map parsing and resolution
  module_graph_loader.rs     — Module loading
  realm_module_loader.rs     — Module resolution

src/api/
  browser_tab.rs             — Script execution integration
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

The parser/execution loop processes these actions.

### Event loop

The event loop follows HTML Standard terminology:

```rust
pub struct EventLoop {
    task_queues: HashMap<TaskSource, VecDeque<Task>>,
    microtask_queue: VecDeque<Microtask>,
}

impl EventLoop {
    fn run_until_idle(&mut self, limits: RunLimits) {
        loop {
            // 1. Run one task from a task queue
            // 2. Run all microtasks (microtask checkpoint)
            // 3. Update rendering if needed
        }
    }
}
```

### Module loading

Module loading uses host hooks in vm-js:

```rust
impl VmHostHooks for BrowserHost {
    fn host_resolve_imported_module(&mut self, referrer: &Module, specifier: &str) 
        -> Result<Module> {
        // 1. Apply import maps
        // 2. Resolve URL relative to referrer
        // 3. Check module cache
        // 4. Fetch and parse if needed
    }
}
```

### Testing

```bash
# Script processing tests
timeout -k 10 600 bash scripts/cargo_agent.sh test --test html_script_processing

# Event loop tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::event_loop

# Module tests
timeout -k 10 600 bash scripts/cargo_agent.sh test -p fastrender --lib js::import_maps
```

### Key documents

- `docs/html_script_processing.md` — Detailed script processing design
- `docs/import_maps.md` — Import map support
- `docs/js_embedding.md` — Overall JS embedding guide

## Success criteria

HTML integration is **done** when:
- Scripts execute in correct order (parser-blocking, async, defer, modules)
- Microtasks run at correct checkpoints (after scripts, after tasks)
- DOMContentLoaded and load fire at correct times
- ES modules work with static and dynamic imports
- Import maps resolve bare specifiers
- Real-world sites with complex script loading work correctly
