# JavaScript execution budgets (host + VM)

FastRender treats JavaScript as **hostile input**. Preventing hangs and unbounded memory growth
requires multiple layers of limits that work together:

These limits primarily matter for the JS-capable runtime containers (e.g. `api::BrowserTab` and
`api::BrowserDocumentJs`) and tooling that embeds them (for example `fetch_and_render --js`,
`render_pages --js`, `render_fixtures --js`, and `pageset_progress run --js`). For a map of which
public containers include JavaScript + an event loop, see [`docs/runtime_stacks.md`](runtime_stacks.md).

## 1) OS/process-level caps

When running renderer binaries, use:

- `bash scripts/run_limited.sh --as 64G …`

This applies an address-space (`RLIMIT_AS`) cap, acting as a last-resort safety net against runaway
allocations (including ones outside the JS subsystem).

## 2) Renderer-wide cooperative deadline

FastRender has a renderer-level cooperative deadline mechanism:

- [`crate::render_control::RenderDeadline`]

This limits overall pipeline work (parse/style/layout/paint *and* any JS-host work that checks the
deadline). The JS event loop integrates with this via `render_control::check_active(...)`.

## 3) Host event loop limits (tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`)

The HTML event loop model implemented in `src/js/event_loop.rs` has two kinds of limits:

- **Queue limits** ([`fastrender::js::QueueLimits`])
  - Bounds how many pending tasks/microtasks/timers and callback queues (`requestAnimationFrame`, `requestIdleCallback`) may be queued at once.
  - Prevents unbounded memory growth from scripts that continuously schedule work.
- **Run limits** ([`fastrender::js::RunLimits`])
  - Bounds how much work may execute in a single call to `run_until_idle` / `spin_until`:
    - max tasks
    - max microtasks
    - optional wall-time budget
  - Prevents `while(true){ queueMicrotask(...) }` style loops from monopolizing the host forever.

These are configured via [`fastrender::js::JsExecutionOptions`].

### Deterministic stepping for test harnesses / embeddings

Some embeddings (notably the offline WPT DOM runner) need to drive the host event loop in **small,
budgeted steps** to avoid unbounded microtask loops while still remaining deterministic.

FastRender exposes a stateful stepping API for this use-case:

- [`fastrender::js::RunState`] (counters + limits, reusable across calls)
- [`fastrender::js::EventLoop::perform_microtask_checkpoint_limited`]
- [`fastrender::js::EventLoop::run_next_task_limited`]

Unlike the unbounded helpers (`perform_microtask_checkpoint`, `run_next_task`), these limited
variants stop with a [`fastrender::js::RunUntilIdleStopReason`] when budgets are exhausted **without
dropping the next queued task/microtask** (limits are enforced before popping).

## 4) VM budgets (instruction / heap / stack)

Some limits must be enforced inside the JS VM:

- instruction counting / interrupt checks
- VM heap size limits
- stack depth limits

`JsExecutionOptions` exposes these as fields (`max_instruction_count`, `max_vm_heap_bytes`,
`max_stack_depth`). When FastRender is built against the `vm-js` backend (such as
`WindowHost`/`WindowRealm`), these are enforced:

- `max_instruction_count` → per-run `vm_js::Budget::fuel`
- per-run wall-time deadline → minimum of:
  - the remaining time in the active root [`crate::render_control::RenderDeadline`] (when configured)
  - `JsExecutionOptions.event_loop_run_limits.max_wall_time`
- `max_vm_heap_bytes` → `vm_js::HeapLimits` (hard heap cap)
- `max_stack_depth` → `vm_js::VmOptions::max_stack_depth`

FastRender applies a fresh `vm_js::Budget` at each JS entrypoint (scripts, callbacks, and Promise
jobs) using [`JsExecutionOptions::vm_js_budget_now`]. Entry points call `vm.tick()` once before
running user code so that already-expired deadlines are observed immediately and loops like
`for(;;){}` cannot run unbounded.

## How to think about layering

In a robust embedding:

1. OS limits ensure the process cannot OOM the machine.
2. `RenderDeadline` ensures "the whole render" cannot run forever.
3. Event loop queue + run limits ensure "JS-host scheduling" cannot grow without bound or spin
   forever in one `run_until_idle`.
4. VM instruction/heap/stack limits ensure `while(true){}` cannot hang inside the VM.

No single layer is sufficient by itself.
