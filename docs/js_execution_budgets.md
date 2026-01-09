# JavaScript execution budgets (host + VM)

FastRender treats JavaScript as **hostile input**. Preventing hangs and unbounded memory growth
requires multiple layers of limits that work together:

## 1) OS/process-level caps

When running renderer binaries, use:

- `scripts/run_limited.sh --as 64G …`

This applies an address-space (`RLIMIT_AS`) cap, acting as a last-resort safety net against runaway
allocations (including ones outside the JS subsystem).

## 2) Renderer-wide cooperative deadline

FastRender has a renderer-level cooperative deadline mechanism:

- [`crate::render_control::RenderDeadline`]

This limits overall pipeline work (parse/style/layout/paint *and* any JS-host work that checks the
deadline). The JS event loop integrates with this via `render_control::check_active(...)`.

## 3) Host event loop limits (tasks + microtasks + timers)

The HTML event loop model implemented in `src/js/event_loop.rs` has two kinds of limits:

- **Queue limits** ([`fastrender::js::QueueLimits`])
  - Bounds how many pending tasks/microtasks/timers may be queued at once.
  - Prevents unbounded memory growth from scripts that continuously schedule work.
- **Run limits** ([`fastrender::js::RunLimits`])
  - Bounds how much work may execute in a single call to `run_until_idle` / `spin_until`:
    - max tasks
    - max microtasks
    - optional wall-time budget
  - Prevents `while(true){ queueMicrotask(...) }` style loops from monopolizing the host forever.

These are configured via [`fastrender::js::JsExecutionOptions`].

## 4) VM budgets (instruction / heap / stack)

Some limits must be enforced inside the JS VM:

- instruction counting / interrupt checks
- VM heap size limits
- stack depth limits

`JsExecutionOptions` exposes these as fields (`max_instruction_count`, `max_vm_heap_bytes`,
`max_stack_depth`) but they are currently **placeholders** until the ecma-rs VM exposes budgeting
hooks.

## How to think about layering

In a robust embedding:

1. OS limits ensure the process cannot OOM the machine.
2. `RenderDeadline` ensures "the whole render" cannot run forever.
3. Event loop queue + run limits ensure "JS-host scheduling" cannot grow without bound or spin
   forever in one `run_until_idle`.
4. VM instruction/heap/stack limits ensure `while(true){}` cannot hang inside the VM.

No single layer is sufficient by itself.

