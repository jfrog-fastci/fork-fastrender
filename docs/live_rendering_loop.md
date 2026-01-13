# Live rendering loop API (`BrowserTab`)

FastRender’s JS-enabled host container is [`fastrender::BrowserTab`](../src/api/browser_tab.rs). It
couples:

- a live `dom2` document + renderer cache (`BrowserDocumentDom2`),
- an HTML-shaped event loop (`js::EventLoop`: tasks + microtasks + timers + `requestAnimationFrame` + `requestIdleCallback`),
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
- `requestIdleCallback` callbacks (dispatched as tasks when the event loop is otherwise idle).

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
- after the task turn (HTML “step 10”), if any `requestAnimationFrame` callbacks are queued, run at
  most **one rAF turn** and then the **microtask checkpoint after rAF**,
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

**Current behavior:** `tick_frame()` runs at most one rAF “turn” when callbacks are pending **and the
next animation frame is due** (paced by `JsExecutionOptions.animation_frame_interval`). It then
drains microtasks queued by rAF before rendering. It does **not** enforce a wall-clock frame cadence
by itself; the embedder is expected to call `tick_frame()` on its chosen frame schedule (see the
live-loop discussion below for wake/sleep strategy).

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

### Waking on external events (`ExternalTaskQueueHandle`)

Some Web APIs (WebSocket, Storage events across windows, etc) can originate callbacks from
background threads. These callbacks are delivered by queueing work into the tab's event loop via an
[`ExternalTaskQueueHandle`](../src/js/event_loop.rs).

If your embedding sleeps when the tab is idle (for example, when `next_wake_time()` returns `None`),
you **must** install a wake callback so background threads can wake the host when they enqueue
external tasks:

```rust,ignore
use std::sync::Arc;
use std::sync::mpsc;

let (wake_tx, wake_rx) = mpsc::channel::<()>();

// Install once per tab (the callback is preserved across navigations).
tab.set_external_wake_callback(Some(Arc::new(move || {
    let _ = wake_tx.send(());
})));

// Background threads can now wake the host by calling `queue_task(...)` on the handle they hold.
```

The embedding is free to coalesce wakeups (e.g. with an `AtomicBool` pending flag) if desired.

Intended shape (pseudo-code):

```rust,ignore
loop {
    if let Some(frame) = tab.tick_frame()? {
        present(frame);
    }

    // Use `next_wake_time()` as a sleep hint: it returns the earliest event-loop timestamp at which
    // calling `tick_frame()` would make progress (tasks/microtasks, due timers, or an rAF turn).
    //
    // This does not replace external wakeups (input/network); embedders should still wake the loop
    // when external events arrive and then call `tick_frame()` again.
    if let Some(wake_at) = tab.next_wake_time() {
        let now = tab.now();
        sleep_for(wake_at.saturating_sub(now));
    } else {
        break;
    }
}
```

### What `next_wake_time()` returns

The event loop clock is monotonic (`js::Clock::now() -> Duration`). `BrowserTab::next_wake_time()`
returns an **absolute timestamp on that clock** (not “sleep for X”):

It considers:

- **Immediate runnable work**: if tasks/microtasks are runnable now, it returns `Some(now)`.
- (This includes pending `requestIdleCallback` callbacks, which are dispatched as tasks when the
  loop is otherwise idle.)
- **Timers**: if only timers are pending, it returns the next timer due time (clamped to `>= now`).
- **Frame callbacks**: if `requestAnimationFrame` callbacks are pending and nothing else is runnable,
  it returns the next eligible animation-frame time (based on
  `JsExecutionOptions.animation_frame_interval`), clamped to `>= now`.

If nothing is scheduled (no tasks/microtasks, no pending timers, no pending rAF), it returns `None`.

---

## Enabling/driving animations

There are two distinct “animation” systems to drive:

1. **JS `requestAnimationFrame`** callbacks (discrete callbacks on a frame schedule),
2. **CSS animations/transitions** (continuous sampling during paint).

### `requestAnimationFrame` (frame schedule, not tasks)

Key rule:

- `requestAnimationFrame` callbacks run on the **frame schedule**, not as normal tasks.
- Therefore they **do not run** during `run_event_loop_until_idle(...)`.

In FastRender, rAF callbacks are executed by `EventLoop::run_animation_frame(...)` and are driven by
higher-level helpers like `BrowserTab::tick_frame(...)` and `BrowserTab::run_until_stable(...)`.

To drive a live tab, your outer loop needs a frame cadence (often ~16.67ms). The HTML processing
model defines this cadence as the **animation frame interval** (often referred to as “step 21” of
the rendering update algorithm): it controls when the next rendering opportunity occurs.

### CSS animations/transitions (timeline sampling during paint)

CSS animations are sampled when painting. FastRender supports two approaches:

- **Deterministic sampling**: set an explicit `animation_time` (ms since document load).
  - For `BrowserTab`, use `set_animation_time_ms(...)` / `set_animation_time(None)`.
  - When the value changes, the document marks paint dirty.
- **Real-time sampling**: call `BrowserTab::set_realtime_animations_enabled(true)` so that, when no explicit
  `RenderOptions.animation_time` override is present, each paint samples animations at the time
  elapsed since the first rendered frame after enabling.

Real-time sampling does not schedule frames by itself; the embedder still needs a frame cadence (for
example, by driving `tick_frame()` on a fixed cadence or in response to external wakeups), otherwise
the document will paint once and then stay visually frozen.

`BrowserTab` also forwards the document-level animation controls from `BrowserDocumentDom2`,
including:

- `BrowserTab::set_animation_clock(...)` (choose which clock backs the CSS timeline),
- `BrowserTab::set_realtime_animations_enabled(true)` (use the timeline during paint),
- `BrowserTab::set_animation_time{,_ms}(...)` (deterministic sampling).

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
use fastrender::js::{Clock, EventLoop, VirtualClock};

fn main() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());

    // The event loop clock drives timers and the rAF timestamp argument.
    //
    // (The explicit `Arc<dyn Clock>` cast is just to make the trait-object boundary obvious.)
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let event_loop: EventLoop<BrowserTabHost> = EventLoop::with_clock(clock_for_loop.clone());

    let mut tab = BrowserTab::from_html_with_event_loop(
        "<!doctype html><p>hi</p>",
        RenderOptions::new(),
        VmJsBrowserTabExecutor::default(),
        event_loop,
    )?;

    // If you want CSS animation sampling to use the same clock as timers/rAF too, set it
    // explicitly:
    //
    // tab.set_animation_clock(clock_for_loop.clone());
    //
    // And if you want paint-time CSS animations/transitions to advance with that clock:
    //
    // tab.set_realtime_animations_enabled(true);

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

With `BrowserTab::next_wake_time()`, a deterministic harness can “jump” time:

- `clock.set_now(next_wake_time)`
- drive one tick/frame
- repeat

without ever sleeping in real time.
