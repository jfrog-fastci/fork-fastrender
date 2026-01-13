# Live rendering loop API (`BrowserTab`)

FastRender’s JS-enabled host container is [`fastrender::BrowserTab`](../src/api/browser_tab.rs). It
couples:

- a live `dom2` document + renderer cache (`BrowserDocumentDom2`),
- an HTML-shaped event loop (`js::EventLoop`: tasks + microtasks + timers),
- an HTML-like `<script>` scheduler,
- (optionally) rendering on DOM invalidation.

This doc describes how to **drive a long-lived, event-driven tab loop** and clarifies which helper
methods run **tasks** vs **animation frames** vs **renders**.

If you’re first trying to decide which public container type to use (document vs tab, JS vs no JS),
start with [`docs/runtime_stacks.md`](runtime_stacks.md).

---

## Three “drivers”: tasks-only vs step-wise vs converge-to-stable

### 1) `BrowserTab::run_event_loop_until_idle(...)` (tasks/microtasks/timers only)

`run_event_loop_until_idle(limits)` executes **runnable** event-loop work until no runnable work
remains or a limit is hit:

- task queue(s) (`TaskSource::*`),
- microtask queue (Promise jobs, `queueMicrotask`, etc),
- timers that are **already due** (timer *scheduling* is part of the event loop).

It intentionally does **not**:

- render,
- run `requestAnimationFrame` (rAF) callbacks.

That means:

- after calling `run_event_loop_until_idle`, you typically follow up with
  [`BrowserTab::render_if_needed`](../src/api/browser_tab.rs) if you want pixels,
- `requestAnimationFrame` callbacks will remain pending forever unless you drive the **frame**
  schedule (see below).

```rust,no_run
use fastrender::{BrowserTab, RenderOptions, Result};
use fastrender::js::RunLimits;

fn main() -> Result<()> {
    let mut tab = BrowserTab::from_html_with_vmjs("<!doctype html><p>hi</p>", RenderOptions::new())?;

    // Drain tasks/microtasks/timers (bounded).
    let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

    // Rendering is explicit.
    let _frame = tab.render_if_needed()?;
    Ok(())
}
```

> **Important:** rAF callbacks are **not** tasks. `run_event_loop_until_idle` will never run them.

### 2) `BrowserTab::tick_frame()` (one step; returns a frame if pixels changed)

`tick_frame()` is the intended primitive for **live / incremental** embedding. Each call executes
at most one “turn” of work and returns pixels if that turn invalidated rendering:

- if microtasks are pending: run a **microtask checkpoint** only,
- otherwise: run exactly **one task turn** (one task + its post-task microtask checkpoint),
- commit any pending navigation,
- render *if needed* and return `Some(Pixmap)` when a new frame is produced.

This is designed for hosts that want to interleave:

- input delivery (mouse/keyboard),
- JS task execution,
- frame production,
- sleeping until the next wake-up.

#### rAF and `tick_frame`

In the HTML Standard, `requestAnimationFrame` is part of the **frame rendering steps** (“update the
rendering”), which occur *after* running a task and performing a microtask checkpoint (commonly
described as “after step 10” of the processing model).

FastRender models this separation too:

- rAF callbacks are queued separately from tasks/microtasks,
- driving tasks to idle does not run rAF.

Today, `BrowserTab::tick_frame()` runs tasks/microtasks and renders, but does **not** yet run a
`requestAnimationFrame` turn by itself. You can use `run_until_stable(...)` (below) to include rAF,
or drive `EventLoop::run_animation_frame(...)` in lower-level embeddings.

### 3) `BrowserTab::run_until_stable(...)` (drains tasks + rAF + renders until convergence)

`run_until_stable(max_frames)` is a deterministic “settle the world” helper:

1. drain tasks/microtasks/timers until idle (bounded by JS run limits),
2. run one animation frame turn (drain rAF callbacks),
3. run the **microtask checkpoint after rAF**,
4. render if needed,
5. repeat until:
   - no rendering invalidation remains,
   - the event loop is idle,
   - and no rAF callbacks are queued,
   - or `max_frames` is exhausted.

This is useful for:

- “render after load” style workflows,
- deterministic tests (“run up to N frames and then snapshot”),

and is **not** a great fit for real-time UI loops (it tries to converge; live apps don’t).

---

## Implementing a live, event-driven loop

The host loop is responsible for:

- presenting frames as they are produced,
- sleeping until the next timer/frame deadline,
- waking on external events (user input, network, etc).

Intended shape (pseudo-code):

```rust,ignore
loop {
    if let Some(frame) = tab.tick_frame()? {
        present(frame);
    }

    // `next_wake_time` is the missing “sleep hint” API: it should return the earliest time at
    // which *something* becomes runnable:
    // - the next due timer, or
    // - the next animation frame deadline when rAF/animations are active.
    if let Some(t) = tab.next_wake_time()? {
        sleep_until(t);
    } else {
        // Nothing scheduled. The embedder can block on external inputs, or just idle.
        break;
    }
}
```

### What `next_wake_time()` should mean

The event loop clock is monotonic (`js::Clock::now() -> Duration`). A practical `next_wake_time()`
API should return an **absolute timestamp on that clock** (not “sleep for X”).

It should consider:

- **timers**: the earliest `EventLoop::next_timer_due_time()`,
- **frame callbacks**: if rAF callbacks are queued, the next frame deadline based on the
  **animation frame interval**,
- **immediate runnable work**: if tasks/microtasks are queued, it can return “now”.

If nothing is scheduled (no tasks/microtasks, no pending timers, no pending rAF), it should return
`None`.

---

## Enabling/driving animations

There are two distinct “animation” systems to drive:

1. **JS `requestAnimationFrame`** callbacks (discrete callbacks on a frame schedule),
2. **CSS animations/transitions** (continuous sampling during paint).

### `requestAnimationFrame` (frame schedule, not tasks)

Key rule:

- `requestAnimationFrame` callbacks run on the **frame schedule**, not as normal tasks.
- Therefore they **do not run** during `run_event_loop_until_idle(...)`.

In FastRender, rAF callbacks are executed by `EventLoop::run_animation_frame(...)` (and by
`BrowserTab::run_until_stable(...)`, which calls into it).

To drive a live tab, your outer loop needs a frame cadence (often ~16.67ms). The HTML processing
model defines this cadence as the **animation frame interval** (often referred to as “step 21” of
the rendering update algorithm): it controls when the next rendering opportunity occurs.

### CSS animations/transitions (timeline sampling during paint)

CSS animations are sampled when painting. FastRender supports two approaches:

- **Deterministic sampling**: set an explicit `animation_time` (ms since document load).
  - For `BrowserDocumentDom2` there are helpers like
    `set_animation_time_ms(...)` / `set_animation_time(None)`.
  - When the value changes, the document marks paint dirty.
- **Real-time sampling**: call `set_realtime_animations_enabled(true)` so that, when no explicit
  `RenderOptions.animation_time` override is present, each paint samples animations at the time
  elapsed since the first rendered frame after enabling.

For deterministic tests, you generally want **both** clocks (event loop timers + CSS animation
timeline) to be driven by the same injected clock.

---

## Deterministic tests: `EventLoop::with_clock(...)` + `VirtualClock`

The event loop (timers + rAF timestamp argument) uses an injectable monotonic clock:

- `js::RealClock` (default): backed by `Instant`,
- `js::VirtualClock`: only advances when you call `advance(...)` / `set_now(...)`.

You can inject a `VirtualClock` by constructing the tab with a custom `EventLoop`:

```rust,no_run
use std::sync::Arc;

use fastrender::{BrowserTab, BrowserTabHost, RenderOptions, Result, VmJsBrowserTabExecutor};
use fastrender::js::{EventLoop, VirtualClock};

fn main() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());

    // The event loop clock drives timers and the rAF timestamp argument.
    let event_loop: EventLoop<BrowserTabHost> = EventLoop::with_clock(clock.clone());

    let mut tab = BrowserTab::from_html_with_event_loop(
        "<!doctype html><p>hi</p>",
        RenderOptions::new(),
        VmJsBrowserTabExecutor::default(),
        event_loop,
    )?;

    // In a real deterministic harness you'd:
    // - run some steps,
    // - inspect the next due time,
    // - advance the clock to that time,
    // - then drive another step.
    clock.advance(std::time::Duration::from_millis(10));
    let _ = tab.tick_frame()?;
    Ok(())
}
```

If/when `BrowserTab::next_wake_time()` is available, a deterministic harness can “jump” time:

- `clock.set_now(next_wake_time)`
- drive one tick/frame
- repeat

without ever sleeping in real time.
