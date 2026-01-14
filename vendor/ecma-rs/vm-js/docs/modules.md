# ECMAScript modules (embedding guide)

This document describes how to embed **ECMAScript modules** (static `import`, dynamic `import()`,
`import.meta`, and top-level `await`) when using `vm-js`.

`vm-js` intentionally separates:

1. **The ECMAScript module algorithms** (module records, linking, evaluation, dynamic import state
   machines), implemented in Rust, from
2. **The host environment** responsibilities (fetching/reading modules, scheduling Promise jobs as
   microtasks, and providing `import.meta`).

If you are embedding `vm-js` in a browser/runtime, this page is the “one stop” overview of what you
must implement and the recommended end-to-end flow.

## Core types

### `ModuleGraph`

[`ModuleGraph`](crate::ModuleGraph) is an embedding-owned container holding
[`SourceTextModuleRecord`](crate::SourceTextModuleRecord)s and their resolved dependency edges.

Important properties:

- It is **not GC-managed**. The VM may hold a raw pointer to it (see [`Vm::set_module_graph`](crate::Vm::set_module_graph)).
- It caches several JS values via **persistent roots** (module environments, module namespaces,
  cached `import.meta`, error values, evaluation promise capabilities, …). If you reuse a heap
  across many graphs, call [`ModuleGraph::teardown`](crate::ModuleGraph::teardown) when done.
- It also maintains spec `[[LoadedModules]]` memoization for **Script** and **Realm** referrers
  (used by `FinishLoadingImportedModule` for dynamic `import()` initiated from classic scripts or
  host callbacks). These per-script/per-realm caches are cleared by `teardown`. If your embedding
  can determine that a `ScriptId` / `RealmId` will never be used again, you can proactively drop
  those caches via:
  - [`ModuleGraph::clear_loaded_modules_for_script`](crate::ModuleGraph::clear_loaded_modules_for_script)
  - [`ModuleGraph::clear_loaded_modules_for_realm`](crate::ModuleGraph::clear_loaded_modules_for_realm)

### `SourceTextModuleRecord`

[`SourceTextModuleRecord`](crate::SourceTextModuleRecord) is `vm-js`’s representation of an ECMA-262
Source Text Module Record.

In practice, hosts usually:

- Parse module source text using one of the `parse_*` helpers (for example
  [`SourceTextModuleRecord::parse_source_with_vm`](crate::SourceTextModuleRecord::parse_source_with_vm)),
- Insert it into a [`ModuleGraph`](crate::ModuleGraph) via [`ModuleGraph::add_module`](crate::ModuleGraph::add_module),
- And let the module loader/linker/evaluator fill in:
  - `[[LoadedModules]]` edges (via `FinishLoadingImportedModule`),
  - module environments,
  - module namespace objects, etc.

### `ModuleRequest`

[`ModuleRequest`](crate::ModuleRequest) is the spec-shaped record `(specifier, attributes)`.

- `specifier` is an arbitrary host string (URL, path, bare specifier, …).
- `attributes` are **import attributes** / “import assertions” (e.g. `{ with: { type: "json" } }`),
  represented as a list of [`ImportAttribute`](crate::ImportAttribute) records.

`ModuleRequest::new` canonicalizes the attribute list (sorts deterministically) so `Eq`/`Hash`
behave like spec `ModuleRequestsEqual`.

### `ModuleReferrer`

[`ModuleReferrer`](crate::ModuleReferrer) is the identity of the referrer passed through
`HostLoadImportedModule`/`FinishLoadingImportedModule`:

- `Script(ScriptId)`
- `Module(ModuleId)`
- `Realm(RealmId)`

This is an **identity token**, safe to store across async boundaries.

### `ModuleLoadPayload`

[`ModuleLoadPayload`](crate::ModuleLoadPayload) is the **opaque** `_payload_` parameter passed to the
host’s module-loading hook.

Per ECMA-262 it represents either:

- a `GraphLoadingState` continuation (static module graph loading), or
- a `PromiseCapability` continuation (dynamic `import()`).

The host must:

- treat it as opaque,
- store/clone it if needed,
- and pass it back *unchanged* when calling
  [`Vm::finish_loading_imported_module`](crate::Vm::finish_loading_imported_module).

## Host hooks you must implement

All module integration is routed through [`VmHostHooks`](crate::VmHostHooks):

### `host_load_imported_module`

[`VmHostHooks::host_load_imported_module`](crate::VmHostHooks::host_load_imported_module) is the
host entry point for module fetching/instantiation.

The VM calls it when it needs a module record for a `(referrer, module_request)` pair. The host is
responsible for:

1. Resolving `module_request.specifier` + `module_request.attributes` (whatever that means for your
   embedding),
2. Obtaining source bytes (network, filesystem, in-memory map, …),
3. Parsing the source into a [`SourceTextModuleRecord`](crate::SourceTextModuleRecord),
4. Inserting it into the shared [`ModuleGraph`](crate::ModuleGraph) to obtain a `ModuleId`,
5. Completing the load by calling [`Vm::finish_loading_imported_module`](crate::Vm::finish_loading_imported_module)
   exactly once, with:
   - the original `referrer`,
   - the original `module_request`,
   - the original `payload`,
   - and `result = Ok(module_id)` or `Err(error)`.

**Re-entrancy is allowed (and common):** calling `finish_loading_imported_module` may immediately
continue the module-loading algorithm and synchronously trigger nested
`host_load_imported_module` calls.

### `host_enqueue_promise_job` (microtasks)

[`VmHostHooks::host_enqueue_promise_job`](crate::VmHostHooks::host_enqueue_promise_job) is how the VM
schedules Promise jobs.

This hook is critical for modules because:

- module evaluation returns a **Promise** (even for non-async modules),
- dynamic `import()` promises are settled via Promise jobs,
- and top-level `await` resumes evaluation via Promise jobs.

**FIFO ordering is required.** ECMA-262 requires `HostEnqueuePromiseJob` to be FIFO per agent.
If you dequeue microtasks out-of-order, top-level await and promise chains will misbehave.

For lightweight embeddings/tests you can use [`MicrotaskQueue`](crate::MicrotaskQueue), which
implements `VmHostHooks` and preserves FIFO ordering.

### `host_get_import_meta_properties` / `host_finalize_import_meta`

These hooks control `import.meta`:

- [`VmHostHooks::host_get_import_meta_properties`](crate::VmHostHooks::host_get_import_meta_properties)
  returns an initial list of properties (often things like `{ url }`).
- [`VmHostHooks::host_finalize_import_meta`](crate::VmHostHooks::host_finalize_import_meta) runs
  after those properties are defined and lets the host do any final customization.

`vm-js` caches one `import.meta` object per module record. If your `import.meta` data depends on
mutable state (unusual), keep in mind this cache.

### `host_get_supported_import_attributes`

[`VmHostHooks::host_get_supported_import_attributes`](crate::VmHostHooks::host_get_supported_import_attributes)
returns the list of attribute keys your host supports (e.g. `["type"]`).

It is used to implement spec `AllImportAttributesSupported`:

- For **static imports**, unsupported keys produce a thrown **SyntaxError** (during module graph
  loading).
- For **dynamic `import()`**, unsupported keys produce a **TypeError** (rejects the import()
  promise).

## Recommended flow: static module loading + evaluation

This section describes the recommended sequence to:

1. parse a root module,
2. load all of its static dependencies using host hooks,
3. evaluate it, and
4. drive microtasks until the evaluation promise settles.

### 0. Precondition: you need a Realm / intrinsics

Module loading and evaluation create Promise objects, errors, and functions. This requires VM
intrinsics, which are installed when creating a [`Realm`](crate::Realm).

If you are using [`JsRuntime`](crate::JsRuntime), it creates a Realm for you.

If you are not using `JsRuntime`, you must ensure the VM has intrinsics (via [`Realm::new`](crate::Realm::new)).

Note: [`ModuleGraph::set_global_lexical_env`](crate::ModuleGraph::set_global_lexical_env) can be
used to wire modules into an embedding’s global lexical environment (so modules can “see” global
lexical bindings). If you do not have a `GcEnv` for your global lexical environment, leaving it
unset is still useful: modules will still be able to resolve **global object properties** (built-in
constructors, `globalThis`, host APIs installed as properties, …).

### 1. Parse and register the root module

Parse source text into a `SourceTextModuleRecord` and add it to the graph:

```rust,ignore
use vm_js::{Heap, HeapLimits, ModuleGraph, Realm, SourceText, SourceTextModuleRecord, Vm, VmOptions};

let mut vm = Vm::new(VmOptions::default());
let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

// Create a realm (installs intrinsics + global object).
let realm = Realm::new(&mut vm, &mut heap)?;
let realm_id = realm.id();
let global_object = realm.global_object();

let mut modules = ModuleGraph::new();
// Optional (if you have a global lexical env): modules.set_global_lexical_env(...);

let source = SourceText::new_charged_arc(&mut heap, "file:///main.js", "import './dep.js';")?;
let record = SourceTextModuleRecord::parse_source_with_vm(&mut vm, &mut heap, source)?;
let root = modules.add_module(record)?;
```

### 2. Start loading the static import graph

Call [`load_requested_modules`](crate::load_requested_modules) (or the host-aware
`*_with_host_and_hooks` variant). This implements spec `LoadRequestedModules` and starts the graph
loading state machine.

It returns a **Promise** that is fulfilled once the entire static import graph has been loaded.

```rust,ignore
use vm_js::{HostDefined, load_requested_modules_with_host_and_hooks};

let mut scope = heap.scope();
let loading_promise = load_requested_modules_with_host_and_hooks(
  &mut vm,
  &mut scope,
  &mut modules,
  /* host_ctx */ &mut my_host_ctx,
  /* hooks    */ &mut my_hooks,
  root,
  HostDefined::default(),
)?;
```

At this point the VM will call your [`host_load_imported_module`](crate::VmHostHooks::host_load_imported_module)
hook for any missing dependencies.

### 3. Finish each `host_load_imported_module` by calling `Vm::finish_loading_imported_module`

For every `host_load_imported_module` call, your host must eventually call
[`Vm::finish_loading_imported_module`](crate::Vm::finish_loading_imported_module) exactly once.

In a synchronous “modules are stored in a map” embedding, this is often done immediately (re-entrantly):

```rust,ignore
fn host_load_imported_module(
  &mut self,
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  referrer: ModuleReferrer,
  request: ModuleRequest,
  _host_defined: HostDefined,
  payload: ModuleLoadPayload,
) -> Result<(), VmError> {
  // 1) Resolve specifier -> source text (host-specific).
  let src = self.fetch_as_string(&request.specifier)?;

  // 2) Parse.
  let source = SourceText::new_charged_arc(scope.heap_mut(), &request.specifier, &src)?;
  let record = SourceTextModuleRecord::parse_source_with_vm(vm, scope.heap_mut(), source)?;

  // 3) Insert into graph.
  let module_id = modules.add_module(record)?;

  // 4) Complete the load.
  vm.finish_loading_imported_module(
    scope,
    modules,
    self,
    referrer,
    request,
    payload,
    Ok(module_id),
  )
}
```

For async hosts (network fetch, filesystem I/O, …), store `referrer/request/payload` and call
`finish_loading_imported_module` later when the fetch completes.

### 4. Evaluate and drive microtasks until the evaluation promise settles

Once all modules are loaded, call [`ModuleGraph::evaluate`](crate::ModuleGraph::evaluate). It always
returns a **Promise** (ECMA-262’s “evaluation promise”), even if evaluation completes synchronously.

```rust,ignore
let eval_promise = modules.evaluate(
  &mut vm,
  &mut heap,
  global_object,
  realm_id,
  root,
  &mut my_host_ctx,
  &mut my_hooks,
)?;
```

To “wait” for completion, you must drive microtasks until the promise is settled:

```rust,ignore
use vm_js::PromiseState;

let Value::Object(promise_obj) = eval_promise else { /* invariant violation */ };
loop {
  match heap.promise_state(promise_obj)? {
    PromiseState::Fulfilled => break,
    PromiseState::Rejected => break,
    PromiseState::Pending => {}
  }

  // Drive a microtask checkpoint in your host event loop.
  // If you use `MicrotaskQueue`, this is `perform_microtask_checkpoint(...)`.
  self.run_microtasks_until_empty(&mut vm, &mut heap)?;
}
```

The promise’s final value is available via [`Heap::promise_result`](crate::Heap::promise_result).

## Dynamic `import()` flow

Dynamic import is a two-phase protocol:

1. **Start** (`EvaluateImportCall`) creates and returns the import() promise and calls the host’s
   `host_load_imported_module` with a `ModuleLoadPayload` that represents the dynamic import
   continuation.
2. The host completes loading by calling `finish_loading_imported_module`, which dispatches into
   **ContinueDynamicImport** and ultimately resolves/rejects the import() promise.

In `vm-js` the entry point is:

- [`start_dynamic_import_with_host_and_hooks`](crate::start_dynamic_import_with_host_and_hooks) (or
  the dummy-host wrapper [`start_dynamic_import`](crate::start_dynamic_import)).

### Interaction with `vm.module_graph_ptr`

Dynamic import is settled via Promise reactions (microtasks). Those callbacks may execute at a time
when you do *not* have a convenient `&mut ModuleGraph` available.

To handle this, the VM stores an **optional raw pointer** to the active module graph:

- [`Vm::set_module_graph`](crate::Vm::set_module_graph)
- [`Vm::module_graph_ptr`](crate::Vm::module_graph_ptr)

Footgun: **the pointed-to `ModuleGraph` must outlive any queued microtasks** that may perform
dynamic import resolution/evaluation.

Recommended patterns:

- Store the graph next to the VM for the lifetime of the runtime (like [`JsRuntime`](crate::JsRuntime) does), or
- If you manage multiple graphs, ensure you clear/restore the pointer correctly and never drop a
  graph while any module evaluation or dynamic import is still pending.

## Top-level `await` (TLA)

### Evaluation promise lifecycle

`ModuleGraph::evaluate` returns the spec-visible evaluation promise:

- If the module graph contains no top-level await (and no async SCC dependencies), evaluation will
  generally complete synchronously and the promise will be fulfilled/rejected immediately.
- If any module in the relevant SCC uses top-level `await`, evaluation becomes **asynchronous**:
  - the promise remains **pending**,
  - evaluation progress is resumed via Promise jobs (microtasks),
  - and the promise is settled only once the async module graph finishes executing.

### When to use `abort_tla_evaluation`

Some embeddings can only support “microtask-only” async (e.g. `await Promise.resolve()`), but do not
have a full event loop (timers, I/O, network, …).

In those environments it is common to:

1. call `ModuleGraph::evaluate`,
2. drain microtasks until the queue is empty,
3. and if the evaluation promise is still pending, abort deterministically:

```rust,ignore
modules.abort_tla_evaluation(&mut vm, &mut heap, root_module_id);
```

`abort_tla_evaluation`:

- rejects any in-progress module evaluation promises with a stable `TypeError` (when possible),
- tears down module-owned async continuation state so persistent roots don’t leak,
- and restores `vm.module_graph_ptr` so stale async callbacks become no-ops.

## Common footguns (read this twice)

1. **You need a Realm / intrinsics.**
   Module loading/evaluation and dynamic import create Promise objects and errors. Without a Realm
   you’ll get `VmError::Unimplemented("… requires intrinsics")`.

2. **`vm.module_graph_ptr` must remain valid across async jobs.**
   Do not put `ModuleGraph` on the stack and then run microtasks later. Keep it alive as long as
   there are pending evaluation promises or dynamic imports.

3. **Microtasks must be FIFO.**
   If your `host_enqueue_promise_job` implementation runs jobs out-of-order, promises and top-level
   await ordering will be wrong.

4. **Always call `finish_loading_imported_module` exactly once per load request.**
   `ModuleLoadPayload` can hold persistent roots; abandoning loads without finishing can leak.
   If you must abandon loading mid-flight (process shutdown, request canceled, …), prefer tearing
   down the whole runtime:
   [`Vm::teardown_microtasks`](crate::Vm::teardown_microtasks) + [`ModuleGraph::teardown`](crate::ModuleGraph::teardown),
   or drop the entire [`Heap`](crate::Heap).
