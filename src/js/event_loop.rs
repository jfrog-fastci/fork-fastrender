use crate::debug::trace::TraceHandle;
use crate::error::{Error, RenderStage, Result};
use crate::render_control::{self, record_stage, StageGuard, StageHeartbeat};
use smallvec::SmallVec;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::{Clock, RealClock};
use super::time::duration_to_ms_f64;
use vm_js::PromiseHandle;

pub type MicrotaskCheckpointHook<Host> = fn(&mut Host, &mut EventLoop<Host>) -> Result<()>;
const MAX_MICROTASK_CHECKPOINT_HOOKS: usize = 8;
type MicrotaskCheckpointHooks<Host> =
  SmallVec<[MicrotaskCheckpointHook<Host>; MAX_MICROTASK_CHECKPOINT_HOOKS]>;

// HTML's idle period computation uses a nominal 50ms budget but clamps it based on pending work.
// FastRender approximates the "pending rendering work" clamp by limiting the idle budget when there
// are queued requestAnimationFrame callbacks.
const DEFAULT_IDLE_BUDGET: Duration = Duration::from_millis(50);
const DEFAULT_FRAME_BUDGET: Duration = Duration::from_millis(16);

/// HTML task sources (WHATWG terminology).
///
/// This enum is intentionally small for now, but designed to be extended as more
/// web APIs are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TaskSource {
  Script,
  Microtask,
  Networking,
  DOMManipulation,
  /// Tasks originating from user interactions or host-driven UI events (e.g. chrome page updates).
  UserInteraction,
  Timer,
  MediaQueryList,
  IdleCallback,
}

const TASK_SOURCE_COUNT: usize = 8;

impl TaskSource {
  #[inline]
  const fn as_usize(self) -> usize {
    // Keep indices aligned with the enum's declared variant order so that any
    // tie-breaking behavior (e.g. if `seq` wraps) stays deterministic.
    match self {
      TaskSource::Script => 0,
      TaskSource::Microtask => 1,
      TaskSource::Networking => 2,
      TaskSource::DOMManipulation => 3,
      TaskSource::UserInteraction => 4,
      TaskSource::Timer => 5,
      TaskSource::MediaQueryList => 6,
      TaskSource::IdleCallback => 7,
    }
  }
}

fn task_source_name(source: TaskSource) -> &'static str {
  match source {
    TaskSource::Script => "Script",
    TaskSource::Microtask => "Microtask",
    TaskSource::Networking => "Networking",
    TaskSource::DOMManipulation => "DOMManipulation",
    TaskSource::UserInteraction => "UserInteraction",
    TaskSource::Timer => "Timer",
    TaskSource::MediaQueryList => "MediaQueryList",
    TaskSource::IdleCallback => "IdleCallback",
  }
}

type Runnable<Host> = Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;
type ExternalRunnable<Host> =
  Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + Send + 'static>;

struct ExternalTask<Host: 'static> {
  source: TaskSource,
  runnable: ExternalRunnable<Host>,
}

struct ExternalTaskQueueState<Host: 'static> {
  queue: VecDeque<ExternalTask<Host>>,
  max_pending_tasks: usize,
  wake: Option<Arc<dyn Fn() + Send + Sync>>,
  closed: bool,
}

/// Thread-safe handle for queueing tasks onto an [`EventLoop`] from other threads.
///
/// This exists for Web APIs like WebSocket where network I/O runs off-thread but callbacks must be
/// delivered through the HTML event loop.
///
/// Tasks queued through this handle are buffered in a thread-safe queue and drained into the event
/// loop's normal task queues when the loop is driven (`run_until_idle`, `run_next_task`, etc).
pub struct ExternalTaskQueueHandle<Host: 'static> {
  inner: Arc<Mutex<ExternalTaskQueueState<Host>>>,
}

impl<Host: 'static> Clone for ExternalTaskQueueHandle<Host> {
  fn clone(&self) -> Self {
    Self {
      inner: Arc::clone(&self.inner),
    }
  }
}

impl<Host: 'static> ExternalTaskQueueHandle<Host> {
  fn new(max_pending_tasks: usize) -> Self {
    Self {
      inner: Arc::new(Mutex::new(ExternalTaskQueueState {
        queue: VecDeque::new(),
        max_pending_tasks,
        wake: None,
        closed: false,
      })),
    }
  }

  fn wake_callback(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
    let lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.wake.clone()
  }

  pub fn set_wake_callback(&self, cb: Option<Arc<dyn Fn() + Send + Sync>>) {
    let mut lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.wake = cb;
  }

  pub fn set_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.set_wake_callback(waker);
  }

  fn set_max_pending_tasks(&self, max_pending_tasks: usize) {
    let mut lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.max_pending_tasks = max_pending_tasks;
    // If the cap shrinks below the current queue length, we keep existing entries; subsequent
    // enqueue attempts will fail until the event loop drains enough work.
  }

  fn close(&self) {
    let mut lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.closed = true;
    lock.queue.clear();
    // Ensure embeddings can release any wake resources once an event loop is dropped/reset.
    lock.wake = None;
  }

  fn is_empty(&self) -> bool {
    let lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.queue.is_empty()
  }

  fn drain(&self) -> Vec<ExternalTask<Host>> {
    let mut lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.queue.drain(..).collect()
  }

  /// Queue a task from any thread.
  ///
  /// This is non-blocking. If the external task buffer is full (or has been closed because the
  /// event loop was dropped/reset), this returns an error.
  pub fn queue_task<F>(&self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + Send + 'static,
  {
    let wake = {
      let mut lock = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      if lock.closed {
        return Err(Error::Other("EventLoop external task queue is closed".to_string()));
      }
      if lock.queue.len() >= lock.max_pending_tasks {
        return Err(Error::Other(format!(
          "EventLoop exceeded max pending external tasks (limit={})",
          lock.max_pending_tasks
        )));
      }
      lock.queue.push_back(ExternalTask {
        source,
        runnable: Box::new(runnable),
      });
      lock.wake.clone()
    };

    // IMPORTANT: do not invoke the wake callback while holding the mutex. Embeddings may re-enter
    // the event loop (draining this queue) from inside the wake callback.
    if let Some(wake) = wake {
      // Best-effort wakeup: never let a wake callback panic propagate into an unrelated producer
      // thread.
      let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (wake)()));
    }
    Ok(())
  }
}

/// A single runnable unit of work (task or microtask).
pub struct Task<Host: 'static> {
  pub source: TaskSource,
  seq: u64,
  runnable: Runnable<Host>,
}

impl<Host: 'static> Task<Host> {
  pub fn new<F>(source: TaskSource, runnable: F) -> Self
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    Self {
      source,
      seq: 0,
      runnable: Box::new(runnable),
    }
  }

  fn new_with_seq<F>(source: TaskSource, seq: u64, runnable: F) -> Self
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    Self {
      source,
      seq,
      runnable: Box::new(runnable),
    }
  }

  fn run(self, host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
    let runnable = self.runnable;
    runnable(host, event_loop)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
  pub max_tasks: usize,
  pub max_microtasks: usize,
  pub max_wall_time: Option<Duration>,
}

impl RunLimits {
  pub fn unbounded() -> Self {
    Self {
      max_tasks: usize::MAX,
      max_microtasks: usize::MAX,
      max_wall_time: None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
  pub max_pending_tasks: usize,
  pub max_pending_microtasks: usize,
  pub max_pending_timers: usize,
  pub max_pending_animation_frame_callbacks: usize,
  pub max_pending_idle_callbacks: usize,
}

impl QueueLimits {
  pub fn unbounded() -> Self {
    Self {
      max_pending_tasks: usize::MAX,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    }
  }
}

impl Default for QueueLimits {
  fn default() -> Self {
    // These are intentionally conservative: the JS runtime is untrusted input, so we cap the
    // amount of queued work to avoid unbounded memory growth.
    Self {
      max_pending_tasks: 100_000,
      max_pending_microtasks: 100_000,
      max_pending_timers: 100_000,
      max_pending_animation_frame_callbacks: 100_000,
      max_pending_idle_callbacks: 100_000,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningTask {
  pub source: TaskSource,
  pub is_microtask: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUntilIdleStopReason {
  MaxTasks { executed: usize, limit: usize },
  MaxMicrotasks { executed: usize, limit: usize },
  WallTime { elapsed: Duration, limit: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUntilIdleOutcome {
  Idle,
  Stopped(RunUntilIdleStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrotaskCheckpointLimitedOutcome {
  /// The microtask queue was fully drained.
  Completed,
  /// The checkpoint stopped early due to a run limit (e.g. max microtasks or wall time).
  Stopped(RunUntilIdleStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunNextTaskLimitedOutcome {
  /// A task was executed (including its post-task microtask checkpoint).
  Ran,
  /// No task was available to run.
  NoTask,
  /// The step stopped early due to a run limit (e.g. max tasks/max microtasks/wall time).
  Stopped(RunUntilIdleStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinOutcome {
  ConditionMet,
  Idle,
  Stopped(RunUntilIdleStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAnimationFrameOutcome {
  Idle,
  Ran { callbacks: usize },
}

/// JS-visible timer ID returned by `setTimeout`/`setInterval`.
///
/// The HTML Standard uses integer handles for timers; we use `i32` so this can be exposed to JS
/// without lossy conversions.
pub type TimerId = i32;

/// JS-visible handle returned by `requestIdleCallback`.
///
/// Like timers, the HTML Standard uses integer handles; we use `i32` so this can be exposed to JS
/// without lossy conversions.
pub type IdleCallbackId = i32;

/// JS-visible handle returned by `requestAnimationFrame`.
///
/// Like timers, the HTML Standard uses integer handles; we use `i32` so this can be exposed to JS
/// without lossy conversions.
pub type AnimationFrameId = i32;

/// Minimal host-side state for `unhandledrejection` / `rejectionhandled` tracking.
///
/// HTML tracks rejected promises per-global using:
/// - an "about-to-be-notified rejected promises list" (strong), and
/// - an "outstanding rejected promises weak set" (weak).
///
/// FastRender's event loop is host-owned (not traced by `vm-js`), so this stores only promise
/// identities. The embedding is responsible for rooting promises while dispatching events.
#[derive(Debug, Default)]
pub(crate) struct PromiseRejectionTrackerState {
  pub(crate) about_to_be_notified: Vec<PromiseHandle>,
  pub(crate) maybe_handled: Vec<PromiseHandle>,
  pub(crate) outstanding_rejected: HashSet<PromiseHandle>,
}

type TimerCallback<Host> = Box<dyn FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

type AnimationFrameCallback<Host> =
  Box<dyn FnMut(&mut Host, &mut EventLoop<Host>, f64) -> Result<()> + 'static>;

type IdleCallback<Host> =
  Box<dyn FnMut(&mut Host, &mut EventLoop<Host>, bool, f64) -> Result<()> + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
  Timeout,
  Interval,
}

struct TimerState<Host: 'static> {
  kind: TimerKind,
  callback: Option<TimerCallback<Host>>,
  interval: Option<Duration>,
  due: Duration,
  schedule_seq: u64,
  nesting_level: u32,
}

struct IdleCallbackState<Host: 'static> {
  callback: Option<IdleCallback<Host>>,
  timeout_at: Option<Duration>,
  schedule_seq: u64,
}

pub struct EventLoop<Host: 'static> {
  clock: Arc<dyn Clock>,
  /// Monotonic timestamp origin used for `requestAnimationFrame` callback timestamps.
  ///
  /// The HTML spec defines rAF timestamps relative to a per-document time origin. FastRender's
  /// `Clock` can be long-lived across navigations, so we store a separate origin that is reset on
  /// navigation to ensure rAF timestamps start near 0ms for each new document.
  raf_time_origin: Duration,
  default_deadline_stage: RenderStage,
  queue_limits: QueueLimits,
  trace: TraceHandle,
  external_task_queue: ExternalTaskQueueHandle<Host>,
  microtask_checkpoint_hooks: MicrotaskCheckpointHooks<Host>,
  pub(crate) promise_rejection_tracker: PromiseRejectionTrackerState,
  task_queues: [VecDeque<Task<Host>>; TASK_SOURCE_COUNT],
  pending_tasks: usize,
  microtask_queue: VecDeque<Task<Host>>,
  next_task_seq: u64,
  timers: HashMap<TimerId, TimerState<Host>>,
  timer_queue: BinaryHeap<Reverse<(Duration, u64, TimerId)>>,
  next_timer_id: TimerId,
  next_timer_seq: u64,
  timer_nesting_level: u32,
  idle_callbacks: HashMap<IdleCallbackId, IdleCallbackState<Host>>,
  idle_callback_queue: VecDeque<IdleCallbackId>,
  /// Shared deadline for the current "idle period" (HTML/requestIdleCallback terminology).
  ///
  /// When the event loop is idle, `requestIdleCallback` callbacks are dispatched with a single
  /// computed deadline. Multiple idle callbacks dispatched within the same idle period should
  /// observe a shrinking `timeRemaining()` budget; they should *not* get a fresh 50ms budget per
  /// callback.
  idle_period_deadline: Option<Duration>,
  next_idle_callback_id: IdleCallbackId,
  next_idle_callback_seq: u64,
  animation_frame_callbacks: HashMap<AnimationFrameId, AnimationFrameCallback<Host>>,
  animation_frame_queue: VecDeque<AnimationFrameId>,
  next_animation_frame_id: AnimationFrameId,
  performing_microtask_checkpoint: bool,
  /// Whether we are currently executing an idle callback task turn (including its microtask
  /// checkpoint).
  ///
  /// This is used to ensure microtasks queued during an idle callback do not reset the shared idle
  /// period deadline. In HTML, idle callbacks are tasks and always end with a microtask checkpoint,
  /// but the idle period deadline is shared across successive idle callbacks within the same idle
  /// period.
  in_idle_callback_turn: bool,
  currently_running_task: Option<RunningTask>,
}

impl<Host: 'static> Default for EventLoop<Host> {
  fn default() -> Self {
    let queue_limits = QueueLimits::default();
    Self {
      clock: Arc::new(RealClock::default()),
      raf_time_origin: Duration::ZERO,
      default_deadline_stage: RenderStage::Script,
      queue_limits,
      trace: TraceHandle::default(),
      external_task_queue: ExternalTaskQueueHandle::new(queue_limits.max_pending_tasks),
      microtask_checkpoint_hooks: SmallVec::new(),
      promise_rejection_tracker: PromiseRejectionTrackerState::default(),
      task_queues: std::array::from_fn(|_| VecDeque::new()),
      pending_tasks: 0,
      microtask_queue: VecDeque::new(),
      next_task_seq: 0,
      timers: HashMap::new(),
      timer_queue: BinaryHeap::new(),
      next_timer_id: 1,
      next_timer_seq: 0,
      timer_nesting_level: 0,
      idle_callbacks: HashMap::new(),
      idle_callback_queue: VecDeque::new(),
      idle_period_deadline: None,
      next_idle_callback_id: 1,
      next_idle_callback_seq: 0,
      animation_frame_callbacks: HashMap::new(),
      animation_frame_queue: VecDeque::new(),
      next_animation_frame_id: 1,
      performing_microtask_checkpoint: false,
      in_idle_callback_turn: false,
      currently_running_task: None,
    }
  }
}

impl<Host: 'static> EventLoop<Host> {
  pub fn new() -> Self {
    Self::default()
  }

  /// Set the microtask checkpoint hook list to contain only `hook`.
  ///
  /// This is a legacy single-hook API. It **clears all** previously registered microtask checkpoint
  /// hooks, which can be surprising when multiple subsystems depend on microtask checkpoints
  /// (promise rejection tracking, executor post-checkpoint notifications, etc).
  ///
  /// Prefer [`Self::register_microtask_checkpoint_hook`] / [`Self::add_microtask_checkpoint_hook`]
  /// to compose multiple hooks without clobbering existing registrations.
  pub fn set_microtask_checkpoint_hook(&mut self, hook: Option<MicrotaskCheckpointHook<Host>>) {
    self.microtask_checkpoint_hooks.clear();
    if let Some(hook) = hook {
      self.microtask_checkpoint_hooks.push(hook);
    }
  }

  pub fn add_microtask_checkpoint_hook(
    &mut self,
    hook: MicrotaskCheckpointHook<Host>,
  ) -> Result<()> {
    self.register_microtask_checkpoint_hook(hook)
  }

  pub fn remove_microtask_checkpoint_hook(&mut self, hook: MicrotaskCheckpointHook<Host>) -> bool {
    let Some(idx) = self
      .microtask_checkpoint_hooks
      .iter()
      .position(|existing| std::ptr::fn_addr_eq(*existing, hook))
    else {
      return false;
    };
    self.microtask_checkpoint_hooks.remove(idx);
    true
  }

  pub fn microtask_checkpoint_hook(&self) -> Option<MicrotaskCheckpointHook<Host>> {
    self.microtask_checkpoint_hooks.first().copied()
  }

  pub fn microtask_checkpoint_hooks(&self) -> &[MicrotaskCheckpointHook<Host>] {
    &self.microtask_checkpoint_hooks
  }

  pub fn register_microtask_checkpoint_hook(
    &mut self,
    hook: MicrotaskCheckpointHook<Host>,
  ) -> Result<()> {
    if self
      .microtask_checkpoint_hooks
      .iter()
      .any(|existing| std::ptr::fn_addr_eq(*existing, hook))
    {
      return Ok(());
    }
    if self.microtask_checkpoint_hooks.len() >= MAX_MICROTASK_CHECKPOINT_HOOKS {
      return Err(Error::Other(format!(
        "EventLoop exceeded max microtask checkpoint hooks (limit={MAX_MICROTASK_CHECKPOINT_HOOKS})"
      )));
    }
    self.microtask_checkpoint_hooks.push(hook);
    Ok(())
  }

  pub fn default_deadline_stage(&self) -> RenderStage {
    self.default_deadline_stage
  }

  pub fn set_default_deadline_stage(&mut self, stage: RenderStage) {
    self.default_deadline_stage = stage;
  }

  pub fn with_stage_guard<T>(&mut self, stage: RenderStage, f: impl FnOnce(&mut Self) -> T) -> T {
    let _guard = render_control::StageGuard::install(Some(stage));
    f(self)
  }

  pub(crate) fn set_trace_handle(&mut self, trace: TraceHandle) {
    self.trace = trace;
  }

  pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
    let mut event_loop = Self::default();
    event_loop.clock = clock;
    event_loop
  }

  pub fn with_clock_and_queue_limits(clock: Arc<dyn Clock>, queue_limits: QueueLimits) -> Self {
    let mut event_loop = Self::default();
    event_loop.clock = clock;
    event_loop.queue_limits = queue_limits;
    event_loop
      .external_task_queue
      .set_max_pending_tasks(queue_limits.max_pending_tasks);
    event_loop
  }

  pub(crate) fn reset_for_navigation(&mut self, trace: TraceHandle, queue_limits: QueueLimits) {
    // Preserve the external wake callback across resets: it is host/embedding state, not document
    // state, and embedding-level wake mechanisms (condvars, UI event-loop proxies, etc) should keep
    // working after navigation swaps the underlying event loop.
    let wake_callback = self.external_task_queue.wake_callback();

    // Close the current external task queue before we overwrite `self`.
    //
    // Embeddings can hold cloned `ExternalTaskQueueHandle`s across navigations (e.g. WebSocket
    // network threads). If we reset the event loop without explicitly closing the old queue, those
    // stale handles can continue enqueueing tasks into an orphaned queue that is never drained.
    // This leaks memory and silently drops events. Closing here ensures all existing clones observe
    // `closed=true` immediately.
    self.external_task_queue.close();

    let clock = Arc::clone(&self.clock);
    // rAF timestamps should restart per-document/navigation instead of using the long-lived clock
    // origin.
    let raf_time_origin = clock.now();
    let hooks = self.microtask_checkpoint_hooks.clone();
    let default_deadline_stage = self.default_deadline_stage;
    let currently_running_task = self.currently_running_task;

    let mut new_event_loop = EventLoop::with_clock_and_queue_limits(clock, queue_limits);
    new_event_loop.set_trace_handle(trace);
    new_event_loop.set_default_deadline_stage(default_deadline_stage);
    new_event_loop.microtask_checkpoint_hooks = hooks;
    new_event_loop.currently_running_task = currently_running_task;
    new_event_loop.raf_time_origin = raf_time_origin;
    new_event_loop.set_external_wake_callback(wake_callback);
    *self = new_event_loop;
  }

  pub fn now(&self) -> Duration {
    self.clock.now()
  }

  pub fn clock(&self) -> Arc<dyn Clock> {
    Arc::clone(&self.clock)
  }

  /// Returns the due time of the next scheduled timer, if any.
  ///
  /// This is primarily intended for deterministic hosts (like the offline WPT runner) that want
  /// to "fast-forward" a virtual clock to the next timer rather than sleeping in real time.
  ///
  /// The timer priority queue can contain stale entries (e.g. cleared timers, or intervals that
  /// have since been rescheduled). This method discards those stale entries before returning.
  pub fn next_timer_due_time(&mut self) -> Option<Duration> {
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      match self.timers.get(&id) {
        Some(timer) if timer.schedule_seq == schedule_seq => return Some(due),
        _ => {
          // Stale queue entry: timer was cleared or rescheduled since it was pushed.
          let _ = self.timer_queue.pop();
        }
      }
    }
    None
  }

  /// Returns the time until the next scheduled timer becomes due, if any.
  ///
  /// Like [`Self::next_timer_due_time`], this discards stale entries from the timer priority queue
  /// before returning.
  pub fn next_timer_due_in(&mut self) -> Option<Duration> {
    let due = self.next_timer_due_time()?;
    Some(due.saturating_sub(self.now()))
  }

  /// Returns the duration until the next scheduled timer becomes due, if any.
  ///
  /// This is a convenience wrapper around [`Self::next_timer_due_time`]. Like that method, it
  /// discards stale heap entries before returning.
  ///
  /// If the next timer is already due, this returns `Some(Duration::ZERO)`.
  pub fn duration_until_next_timer(&mut self) -> Option<Duration> {
    self.next_timer_due_in()
  }

  pub fn queue_limits(&self) -> QueueLimits {
    self.queue_limits
  }

  pub fn set_queue_limits(&mut self, limits: QueueLimits) {
    self.queue_limits = limits;
    self
      .external_task_queue
      .set_max_pending_tasks(limits.max_pending_tasks);
  }

  pub fn currently_running_task(&self) -> Option<RunningTask> {
    self.currently_running_task
  }

  /// Whether there is any runnable work (tasks, microtasks, or idle callbacks) queued.
  ///
  /// This does *not* consider timers that are scheduled but not yet due; see
  /// [`EventLoop::has_pending_timers`] / [`EventLoop::duration_until_next_timer`].
  pub fn is_idle(&self) -> bool {
    self.pending_tasks == 0
      && self.microtask_queue.is_empty()
      && self.external_task_queue.is_empty()
      && self.idle_callback_queue.is_empty()
  }

  /// Whether any timers are currently scheduled (`setTimeout`/`setInterval`).
  ///
  /// This reports whether any timers exist at all, regardless of whether they are due yet.
  /// This is distinct from [`EventLoop::is_idle`], which only considers *runnable* work (tasks and
  /// microtasks) and can be true even when future timers are pending.
  pub fn has_pending_timers(&self) -> bool {
    !self.timers.is_empty()
  }

  /// Returns the number of currently scheduled timers.
  pub fn pending_timer_count(&self) -> usize {
    self.timers.len()
  }

  /// Whether this event loop has no runnable work and no future work scheduled.
  ///
  /// This is stronger than [`EventLoop::is_idle`]: it also requires that no timers remain scheduled
  /// (even if due in the future) and that no `requestAnimationFrame` callbacks remain pending.
  ///
  /// Note: `EventLoop::is_idle` already includes externally queued tasks and pending
  /// `requestIdleCallback` callbacks (the latter are dispatched as tasks when the loop is idle).
  ///
  /// Embeddings use this to deterministically detect when they can no longer make forward progress
  /// (for example, to fail module top-level await that never settles).
  pub fn is_quiescent(&mut self) -> bool {
    // A quiescent loop must not be in the middle of a *task* turn.
    //
    // NOTE: `EventLoop` may run microtask checkpoint hooks while `currently_running_task` is still
    // set to the last drained microtask (implementation detail). Treat that as "between turns" so
    // callers can observe quiescence after the microtask queue is drained.
    if let Some(task) = self.currently_running_task {
      if !task.is_microtask {
        return false;
      }
    }
    if !self.is_idle() {
      return false;
    }
    if self.has_pending_timers() {
      return false;
    }
    if self.has_pending_animation_frame_callbacks() {
      return false;
    }
    true
  }
  pub fn has_pending_animation_frame_callbacks(&self) -> bool {
    !self.animation_frame_callbacks.is_empty()
  }

  /// Clears all queued work from this event loop.
  ///
  /// This removes:
  /// - pending tasks and microtasks,
  /// - scheduled timers (including their priority queue entries), and
  /// - pending `requestIdleCallback` callbacks,
  /// - pending `requestAnimationFrame` callbacks.
  ///
  /// Embeddings can use this when abandoning the current document's execution context (for example
  /// when committing a `window.location` navigation). This should be called when no task is
  /// currently running.
  pub fn clear_all_pending_work(&mut self) {
    for queue in self.task_queues.iter_mut() {
      queue.clear();
    }
    self.pending_tasks = 0;
    self.microtask_queue.clear();
    self.timers.clear();
    self.timer_queue.clear();
    self.timer_nesting_level = 0;
    self.idle_callbacks.clear();
    self.idle_callback_queue.clear();
    self.idle_period_deadline = None;
    self.animation_frame_callbacks.clear();
    self.animation_frame_queue.clear();
    let _ = self.external_task_queue.drain();
  }

  /// Returns a thread-safe handle for queueing tasks from other threads.
  pub fn external_task_queue_handle(&self) -> ExternalTaskQueueHandle<Host> {
    self.external_task_queue.clone()
  }

  /// Installs (or clears) a callback invoked whenever an external task is successfully queued via
  /// [`ExternalTaskQueueHandle::queue_task`].
  ///
  /// Embeddings can use this to wake a sleeping host/UI so externally-driven events (WebSocket,
  /// message channels, etc) still make forward progress even when the host is otherwise idle.
  pub fn set_external_wake_callback(&self, cb: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.external_task_queue.set_wake_callback(cb);
  }

  pub fn set_external_task_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.set_external_wake_callback(waker);
  }

  fn drain_external_tasks(&mut self) -> Result<()> {
    loop {
      // Peek without consuming so we don't lose tasks if enqueuing fails (e.g. queue limit reached,
      // allocation failure during reservation).
      let source = {
        let lock = self
          .external_task_queue
          .inner
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(task) = lock.queue.front() else {
          break;
        };
        task.source
      };

      // External tasks indicate that the event loop is not idle, so discard any active idle period
      // deadline (unless the external task itself is an idle callback task, which is considered
      // part of an idle period).
      if source != TaskSource::IdleCallback {
        self.idle_period_deadline = None;
      }

      // Mirror `queue_task`'s failure modes, but ensure the external task stays queued if we
      // cannot accept more work right now.
      if self.pending_task_count() >= self.queue_limits.max_pending_tasks {
        return Err(Error::Other(format!(
          "EventLoop exceeded max pending tasks (limit={})",
          self.queue_limits.max_pending_tasks
        )));
      }

      // Reserve space up-front so we can safely move the `FnOnce` out of the external queue
      // without risking it being dropped on enqueue failure.
      let reserved_source_idx = source.as_usize();
      self.task_queues[reserved_source_idx]
        .try_reserve(1)
        .map_err(|err| Error::Other(format!("EventLoop task queue allocation failed: {err}")))?;

      let task = {
        let mut lock = self
          .external_task_queue
          .inner
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner());
        lock.queue.pop_front()
      };

      let Some(task) = task else {
        // Raced with `close()` or another drain; nothing left to do.
        break;
      };

      let seq = self.next_task_seq;
      self.next_task_seq = self.next_task_seq.wrapping_add(1);

      let source = task.source;
      let runnable = task.runnable;
      // `EventLoop::queue_task` does not require `Send`, so wrap the Send task in a local closure.
      debug_assert_eq!(
        source.as_usize(),
        reserved_source_idx,
        "external task queue front changed during drain"
      );
      self.task_queues[reserved_source_idx].push_back(Task::new_with_seq(
        source,
        seq,
        move |host, event_loop| runnable(host, event_loop),
      ));
      self.pending_tasks += 1;
    }

    Ok(())
  }

  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    // If new work becomes pending, the event loop is no longer in an idle period, so discard any
    // previously computed idle deadline.
    //
    // Do not clear when queueing idle callback tasks; those are dispatched *within* an idle period.
    if source != TaskSource::IdleCallback {
      self.idle_period_deadline = None;
    }
    if self.pending_task_count() >= self.queue_limits.max_pending_tasks {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending tasks (limit={})",
        self.queue_limits.max_pending_tasks
      )));
    }
    let seq = self.next_task_seq;
    self.next_task_seq = self.next_task_seq.wrapping_add(1);
    let queue = &mut self.task_queues[source.as_usize()];
    queue
      .try_reserve(1)
      .map_err(|err| Error::Other(format!("EventLoop task queue allocation failed: {err}")))?;
    queue.push_back(Task::new_with_seq(source, seq, runnable));
    self.pending_tasks += 1;
    Ok(())
  }

  pub fn queue_microtask<F>(&mut self, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    // Microtasks indicate that the event loop is not idle, so reset any active idle period budget.
    //
    // However, microtasks queued during an idle callback's task turn (including the subsequent
    // microtask checkpoint) should not reset the shared idle period deadline.
    if !self.in_idle_callback_turn {
      self.idle_period_deadline = None;
    }
    if self.microtask_queue.len() >= self.queue_limits.max_pending_microtasks {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending microtasks (limit={})",
        self.queue_limits.max_pending_microtasks
      )));
    }
    let seq = self.next_task_seq;
    self.next_task_seq = self.next_task_seq.wrapping_add(1);
    self.microtask_queue.try_reserve(1).map_err(|err| {
      Error::Other(format!(
        "EventLoop microtask queue allocation failed: {err}"
      ))
    })?;
    self
      .microtask_queue
      .push_back(Task::new_with_seq(TaskSource::Microtask, seq, runnable));
    Ok(())
  }

  pub(crate) fn pending_microtask_count(&self) -> usize {
    self.microtask_queue.len()
  }

  pub fn set_timeout<F>(&mut self, delay: Duration, callback: F) -> Result<TimerId>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    let mut maybe = Some(Box::new(callback) as Runnable<Host>);
    let callback: TimerCallback<Host> = Box::new(move |host, event_loop| {
      let runnable = maybe
        .take()
        .ok_or_else(|| Error::Other("setTimeout callback invoked more than once".to_string()))?;
      runnable(host, event_loop)
    });
    Ok(self.add_timer(TimerKind::Timeout, delay, None, callback)?)
  }

  pub fn set_interval<F>(&mut self, interval: Duration, callback: F) -> Result<TimerId>
  where
    F: FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    Ok(self.add_timer(
      TimerKind::Interval,
      interval,
      Some(interval),
      Box::new(callback),
    )?)
  }

  pub fn clear_timeout(&mut self, id: TimerId) {
    self.timers.remove(&id);
  }

  pub fn clear_interval(&mut self, id: TimerId) {
    self.clear_timeout(id);
  }

  pub fn request_idle_callback<F>(
    &mut self,
    timeout: Option<Duration>,
    callback: F,
  ) -> Result<IdleCallbackId>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>, bool, f64) -> Result<()> + 'static,
  {
    if self.idle_callbacks.len() >= self.queue_limits.max_pending_idle_callbacks {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending idle callbacks (limit={})",
        self.queue_limits.max_pending_idle_callbacks
      )));
    }

    let id: IdleCallbackId = loop {
      if self.next_idle_callback_id == 0 {
        self.next_idle_callback_id = 1;
      }
      let id = self.next_idle_callback_id;
      self.next_idle_callback_id = self.next_idle_callback_id.wrapping_add(1);
      if self.next_idle_callback_id == 0 {
        self.next_idle_callback_id = 1;
      }
      if !self.idle_callbacks.contains_key(&id) {
        break id;
      }
    };

    let now = self.clock.now();
    let timeout_at = timeout.map(|timeout| now.checked_add(timeout).unwrap_or(Duration::MAX));

    let schedule_seq = self.next_idle_callback_seq;
    self.next_idle_callback_seq = self.next_idle_callback_seq.wrapping_add(1);

    let mut maybe =
      Some(Box::new(callback)
        as Box<
          dyn FnOnce(&mut Host, &mut EventLoop<Host>, bool, f64) -> Result<()>,
        >);
    let callback: IdleCallback<Host> = Box::new(move |host, event_loop, did_timeout, remaining_ms| {
      let runnable = maybe.take().ok_or_else(|| {
        Error::Other("requestIdleCallback callback invoked more than once".to_string())
      })?;
      runnable(host, event_loop, did_timeout, remaining_ms)
    });

    self.idle_callbacks.insert(
      id,
      IdleCallbackState {
        callback: Some(callback),
        timeout_at,
        schedule_seq,
      },
    );
    self.idle_callback_queue.push_back(id);
    Ok(id)
  }

  pub fn cancel_idle_callback(&mut self, id: IdleCallbackId) {
    self.idle_callbacks.remove(&id);
    if let Some(idx) = self.idle_callback_queue.iter().position(|queued| *queued == id) {
      let _ = self.idle_callback_queue.remove(idx);
    }
    if self.idle_callbacks.is_empty() {
      // Avoid accumulating stale handles when all callbacks are gone.
      self.idle_callback_queue.clear();
    }
  }

  pub fn request_animation_frame<F>(&mut self, callback: F) -> Result<AnimationFrameId>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>, f64) -> Result<()> + 'static,
  {
    self.maybe_compact_animation_frame_queue();
    if self.animation_frame_callbacks.len()
      >= self.queue_limits.max_pending_animation_frame_callbacks
    {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending requestAnimationFrame callbacks (limit={})",
        self.queue_limits.max_pending_animation_frame_callbacks
      )));
    }

    let id: AnimationFrameId = loop {
      if self.next_animation_frame_id == 0 {
        self.next_animation_frame_id = 1;
      }
      let id = self.next_animation_frame_id;
      self.next_animation_frame_id = self.next_animation_frame_id.wrapping_add(1);
      if self.next_animation_frame_id == 0 {
        self.next_animation_frame_id = 1;
      }
      if !self.animation_frame_callbacks.contains_key(&id) {
        break id;
      }
    };

    let mut maybe =
      Some(Box::new(callback)
        as Box<
          dyn FnOnce(&mut Host, &mut EventLoop<Host>, f64) -> Result<()>,
        >);
    let callback: AnimationFrameCallback<Host> = Box::new(move |host, event_loop, timestamp| {
      let runnable = maybe.take().ok_or_else(|| {
        Error::Other("requestAnimationFrame callback invoked more than once".to_string())
      })?;
      runnable(host, event_loop, timestamp)
    });

    self.animation_frame_callbacks.insert(id, callback);
    self.animation_frame_queue.push_back(id);
    Ok(id)
  }

  pub fn cancel_animation_frame(&mut self, id: AnimationFrameId) {
    self.animation_frame_callbacks.remove(&id);
    if self.animation_frame_callbacks.is_empty() {
      // Avoid accumulating canceled IDs in the scheduling queue.
      self.animation_frame_queue.clear();
      return;
    }
    self.maybe_compact_animation_frame_queue();
  }

  /// Run one animation frame "turn" (draining callbacks queued before the frame started).
  ///
  /// - Callbacks scheduled while executing the frame are deferred to the next frame.
  /// - All callbacks in the same frame observe the same timestamp argument.
  pub fn run_animation_frame(&mut self, host: &mut Host) -> Result<RunAnimationFrameOutcome> {
    self.run_animation_frame_inner(host)
  }

  /// Run one animation frame "turn", treating callback errors as uncaught exceptions.
  ///
  /// If a callback returns an error, it is surfaced via `on_error` and does not abort the frame.
  /// Remaining callbacks for the same frame still run.
  pub fn run_animation_frame_handling_errors<F>(
    &mut self,
    host: &mut Host,
    mut on_error: F,
  ) -> Result<RunAnimationFrameOutcome>
  where
    F: FnMut(Error),
  {
    self.run_animation_frame_inner_with_error_handler(host, Some(&mut on_error))
  }

  /// Perform a microtask checkpoint (HTML Standard terminology).
  ///
  /// - If a checkpoint is already in progress, this is a no-op (reentrancy guard).
  /// - Otherwise, drains the microtask queue until it becomes empty.
  ///
  /// ## Error behavior
  ///
  /// If a microtask returns an error, the checkpoint continues running remaining microtasks (HTML
  /// reports exceptions but does not abort the checkpoint). After draining, the first error (if
  /// any) is returned to the caller.
  pub fn perform_microtask_checkpoint(&mut self, host: &mut Host) -> Result<()> {
    if self.performing_microtask_checkpoint {
      return Ok(());
    }

    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let mut trace_span = self.trace.span("js.microtask_checkpoint", "js");
    trace_span.arg_u64("queued_at_start", self.microtask_queue.len() as u64);
    let mut drained: u64 = 0;
    // Safety: microtasks can re-queue themselves indefinitely (e.g. `queueMicrotask(function f(){ queueMicrotask(f); })`).
    // Browsers can hang on such input; FastRender must remain bounded on hostile pages.
    //
    // Use the *pending microtask cap* as a hard limit for how many microtasks we are willing to
    // drain in a single checkpoint. This is not spec behavior, but it prevents unbounded host work
    // when there is no outer render deadline.
    let drain_limit = self.queue_limits.max_pending_microtasks as u64;
    trace_span.arg_u64("drain_limit", drain_limit);
    let stage_for_deadline = render_control::active_stage().unwrap_or(self.default_deadline_stage);
    let mut deadline_counter: usize = 0;
    let result = (|| {
      let mut first_err: Option<Error> = None;
      loop {
        while !self.microtask_queue.is_empty() {
          if drained >= drain_limit {
            return Err(Error::Other(format!(
              "EventLoop microtask checkpoint exceeded drain limit (drained={drained}, limit={drain_limit}); possible infinite microtask loop"
            )));
          }

          // Integrate renderer-level cancellation/deadlines so microtask checkpoints can't hang the
          // host.
          //
          // IMPORTANT: check before popping so a timeout/cancel does not drop the next queued
          // microtask. Dropping a `vm-js` job without running/discarding it can leak GC roots and
          // trigger debug assertions.
          render_control::check_active_periodic(&mut deadline_counter, 1024, stage_for_deadline)
            .map_err(|err| Error::Render(err))?;

          let Some(task) = self.microtask_queue.pop_front() else {
            break;
          };

          self.currently_running_task = Some(RunningTask {
            source: task.source,
            is_microtask: true,
          });
          if let Err(err) = task.run(host, self) {
            if first_err.is_none() {
              first_err = Some(err);
            }
          }
          drained = drained.saturating_add(1);
        }

        let hooks = self.microtask_checkpoint_hooks.clone();
        for hook in hooks.iter().copied() {
          if let Err(err) = hook(host, self) {
            if first_err.is_none() {
              first_err = Some(err);
            }
          }
        }

        if self.microtask_queue.is_empty() {
          break;
        }
      }
      first_err.map_or(Ok(()), Err)
    })();

    trace_span.arg_u64("drained", drained);
    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  /// Run a single task, if one is queued.
  ///
  /// Returns `Ok(true)` when a task was executed, `Ok(false)` when the task queue was empty.
  /// After executing a task, a microtask checkpoint is performed.
  pub fn run_next_task(&mut self, host: &mut Host) -> Result<bool> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    self.queue_due_timers()?;

    let task = match self.pop_next_task() {
      Some(task) => task,
      None => {
        // No normal tasks are queued. If the event loop is otherwise idle, run the next idle
        // callback (if any) as a task turn.
        if self.queue_next_idle_callback_if_idle()? {
          match self.pop_next_task() {
            Some(task) => task,
            None => return Ok(false),
          }
        } else {
          return Ok(false);
        }
      }
    };

    let mut trace_span = self.trace.span("js.task.run", "js");
    trace_span.arg_str("source", task_source_name(task.source));
    trace_span.arg_u64("seq", task.seq);

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    let previous_idle_callback_turn = self.in_idle_callback_turn;
    self.in_idle_callback_turn =
      self.in_idle_callback_turn || task.source == TaskSource::IdleCallback;

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    // Always clear running-task state so errors don't leave the event loop in a "running" state.
    self.currently_running_task = None;
    // HTML performs a microtask checkpoint at the end of every task. Even if the task threw an
    // exception, queued microtasks must still be drained.
    let microtask_result = self.perform_microtask_checkpoint(host);
    self.in_idle_callback_turn = previous_idle_callback_turn;
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    // Prefer surfacing the task error if both the task and the microtask checkpoint failed: the
    // task error occurred first and is usually the most relevant failure mode.
    match task_result {
      Ok(()) => microtask_result?,
      Err(err) => {
        let _ = microtask_result;
        return Err(err);
      }
    }
    Ok(true)
  }

  /// Perform a microtask checkpoint while respecting the provided run limits.
  ///
  /// This is the bounded counterpart to [`EventLoop::perform_microtask_checkpoint`]. The
  /// `run_state` counters are updated and preserved across calls, allowing embeddings to drive the
  /// event loop one checkpoint at a time.
  pub fn perform_microtask_checkpoint_limited(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> Result<MicrotaskCheckpointLimitedOutcome> {
    match self.perform_microtask_checkpoint_limited_inner(host, run_state) {
      Ok(()) => Ok(MicrotaskCheckpointLimitedOutcome::Completed),
      Err(RunStepError::Stop(reason)) => Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  /// Run a single task turn while respecting the provided run limits.
  ///
  /// This method executes at most one queued task (if any) and always performs the post-task
  /// microtask checkpoint, updating `run_state` counters along the way.
  pub fn run_next_task_limited(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> Result<RunNextTaskLimitedOutcome> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    match self.run_next_task_limited_inner(host, run_state) {
      Ok(true) => Ok(RunNextTaskLimitedOutcome::Ran),
      Ok(false) => Ok(RunNextTaskLimitedOutcome::NoTask),
      Err(RunStepError::Stop(reason)) => Ok(RunNextTaskLimitedOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  /// Construct a [`RunState`] for stepping this event loop with the given limits.
  ///
  /// This is a convenience for embeddings that do not have direct access to the event loop's
  /// internal clock instance (used for wall-time budgeting).
  pub fn new_run_state(&self, limits: RunLimits) -> RunState {
    RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage)
  }

  pub fn run_until_idle(
    &mut self,
    host: &mut Host,
    limits: RunLimits,
  ) -> Result<RunUntilIdleOutcome> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    let mut run_state = RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage);

    match self.run_until_idle_inner(host, &mut run_state) {
      Ok(outcome) => Ok(outcome),
      Err(RunStepError::Stop(reason)) => Ok(RunUntilIdleOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  /// Runs the event loop until it is idle, invoking `hook` after each task turn or standalone
  /// microtask checkpoint.
  ///
  /// This is intended for embeddings that need to interleave extra per-turn work with JS execution
  /// (for example: re-style/re-layout/repaint after JS-driven DOM mutations).
  ///
  /// The hook is called after:
  /// - draining the microtask queue when the event loop starts a microtask checkpoint, and
  /// - executing a single task (including its post-task microtask checkpoint).
  ///
  /// The hook is **not** called when the event loop is already idle (no runnable tasks/microtasks,
  /// no pending idle callbacks, and no externally queued tasks).
  pub fn run_until_idle_with_hook(
    &mut self,
    host: &mut Host,
    limits: RunLimits,
    mut hook: impl FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()>,
  ) -> Result<RunUntilIdleOutcome> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    let mut run_state = RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage);

    match self.run_until_idle_inner_with_hook(host, &mut run_state, &mut hook) {
      Ok(outcome) => Ok(outcome),
      Err(RunStepError::Stop(reason)) => Ok(RunUntilIdleOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  /// Run until there are no more queued tasks/microtasks, but treat task errors as uncaught
  /// exceptions that are surfaced via `on_error` and do not abort the run.
  ///
  /// This matches browser behavior more closely than [`EventLoop::run_until_idle`]: an exception
  /// thrown from an event loop task is reported (e.g. via `console.error`) but does not stop the
  /// event loop from running subsequent tasks.
  pub fn run_until_idle_handling_errors<F>(
    &mut self,
    host: &mut Host,
    limits: RunLimits,
    mut on_error: F,
  ) -> Result<RunUntilIdleOutcome>
  where
    F: FnMut(Error),
  {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    let mut run_state = RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage);

    match self.run_until_idle_handling_errors_inner(host, &mut run_state, &mut on_error) {
      Ok(outcome) => Ok(outcome),
      Err(RunStepError::Stop(reason)) => Ok(RunUntilIdleOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  /// Run until there are no more queued tasks/microtasks, treating task/microtask errors as
  /// uncaught exceptions surfaced via `on_error`, and invoking `hook` after each task turn or
  /// standalone microtask checkpoint.
  ///
  /// This combines the semantics of [`EventLoop::run_until_idle_handling_errors`] (exceptions do
  /// not abort the run) with [`EventLoop::run_until_idle_with_hook`] (embeddings can interleave
  /// per-turn work like rendering/invalidation between turns).
  ///
  /// ## Hook behavior
  ///
  /// The hook is called after:
  /// - draining the microtask queue when the event loop starts a microtask checkpoint, and
  /// - executing a single task (including its post-task microtask checkpoint).
  ///
  /// Errors returned by `hook` are treated as fatal and abort the run (they are **not** passed to
  /// `on_error`).
  pub fn run_until_idle_handling_errors_with_hook<OnError, Hook>(
    &mut self,
    host: &mut Host,
    limits: RunLimits,
    mut on_error: OnError,
    mut hook: Hook,
  ) -> Result<RunUntilIdleOutcome>
  where
    OnError: FnMut(Error),
    Hook: FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()>,
  {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    let mut run_state = RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage);

    match self.run_until_idle_handling_errors_inner_with_hook(
      host,
      &mut run_state,
      &mut on_error,
      &mut hook,
    ) {
      Ok(outcome) => Ok(outcome),
      Err(RunStepError::Stop(reason)) => Ok(RunUntilIdleOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  fn run_until_idle_inner(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> RunStepResult<RunUntilIdleOutcome> {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;
      run_state.check_deadline()?;
      if self.microtask_queue.is_empty() && self.pending_tasks == 0 {
        if self
          .queue_next_idle_callback_if_idle()
          .map_err(RunStepError::Error)?
        {
          continue;
        }
        return Ok(RunUntilIdleOutcome::Idle);
      }

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_limited_inner(host, run_state)?;
        continue;
      }

      if self.run_next_task_limited_inner(host, run_state)? {
        continue;
      }

      return Ok(RunUntilIdleOutcome::Idle);
    }
  }

  fn run_until_idle_inner_with_hook(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    hook: &mut impl FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()>,
  ) -> RunStepResult<RunUntilIdleOutcome> {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;
      run_state.check_deadline()?;
      if self.microtask_queue.is_empty() && self.pending_tasks == 0 {
        if self
          .queue_next_idle_callback_if_idle()
          .map_err(RunStepError::Error)?
        {
          continue;
        }
        return Ok(RunUntilIdleOutcome::Idle);
      }

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_limited_inner(host, run_state)?;
        hook(host, self).map_err(RunStepError::Error)?;
        continue;
      }

      if self.run_next_task_limited_inner(host, run_state)? {
        hook(host, self).map_err(RunStepError::Error)?;
        continue;
      }

      return Ok(RunUntilIdleOutcome::Idle);
    }
  }

  fn run_until_idle_handling_errors_inner<F>(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    on_error: &mut F,
  ) -> RunStepResult<RunUntilIdleOutcome>
  where
    F: FnMut(Error),
  {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;
      run_state.check_deadline()?;
      if self.microtask_queue.is_empty() && self.pending_tasks == 0 {
        if self
          .queue_next_idle_callback_if_idle()
          .map_err(RunStepError::Error)?
        {
          continue;
        }
        return Ok(RunUntilIdleOutcome::Idle);
      }

      if !self.microtask_queue.is_empty() {
        self
          .perform_microtask_checkpoint_limited_handling_errors_inner(host, run_state, on_error)?;
        continue;
      }

      if self.run_next_task_limited_handling_errors_inner(host, run_state, on_error)? {
        continue;
      }

      return Ok(RunUntilIdleOutcome::Idle);
    }
  }

  fn run_until_idle_handling_errors_inner_with_hook<OnError, Hook>(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    on_error: &mut OnError,
    hook: &mut Hook,
  ) -> RunStepResult<RunUntilIdleOutcome>
  where
    OnError: FnMut(Error),
    Hook: FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()>,
  {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;
      run_state.check_deadline()?;
      if self.microtask_queue.is_empty() && self.pending_tasks == 0 {
        if self
          .queue_next_idle_callback_if_idle()
          .map_err(RunStepError::Error)?
        {
          continue;
        }
        return Ok(RunUntilIdleOutcome::Idle);
      }

      if !self.microtask_queue.is_empty() {
        self
          .perform_microtask_checkpoint_limited_handling_errors_inner(host, run_state, on_error)?;
        hook(host, self).map_err(RunStepError::Error)?;
        continue;
      }

      if self.run_next_task_limited_handling_errors_inner(host, run_state, on_error)? {
        hook(host, self).map_err(RunStepError::Error)?;
        continue;
      }

      return Ok(RunUntilIdleOutcome::Idle);
    }
  }

  fn run_next_task_limited_inner(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> RunStepResult<bool> {
    self.queue_due_timers().map_err(RunStepError::Error)?;

    // IMPORTANT: enforce run limits *before* popping a task off the queue.
    //
    // `run_until_idle` is used for bounded execution. If we pop first and then hit `MaxTasks`,
    // we'd effectively drop the task from the queue, which is incorrect (and can break
    // determinism/correctness for embeddings that resume the event loop later).
    if self.pending_tasks == 0
      && !self
        .queue_next_idle_callback_if_idle()
        .map_err(RunStepError::Error)?
    {
      return Ok(false);
    }

    run_state.check_deadline()?;
    run_state.before_task()?;

    let Some(task) = self.pop_next_task() else {
      // A task queue existed but no task was available. This should be unreachable, but avoid
      // panicking if invariants are violated.
      return Ok(false);
    };

    let mut trace_span = self.trace.span("js.task.run", "js");
    trace_span.arg_str("source", task_source_name(task.source));
    trace_span.arg_u64("seq", task.seq);

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    let previous_idle_callback_turn = self.in_idle_callback_turn;
    self.in_idle_callback_turn =
      self.in_idle_callback_turn || task.source == TaskSource::IdleCallback;

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    self.currently_running_task = None;
    // HTML performs a microtask checkpoint at the end of every task. Even if the task threw an
    // exception, queued microtasks must still be drained.
    let microtask_result = self.perform_microtask_checkpoint_limited_inner(host, run_state);
    self.in_idle_callback_turn = previous_idle_callback_turn;
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    // Prefer surfacing the task error if the task failed, even if the microtask checkpoint also hit
    // a stop condition/error: the caller is already in an exceptional state, and losing the task
    // error would be surprising.
    match task_result {
      Ok(()) => microtask_result?,
      Err(err) => {
        let _ = microtask_result;
        return Err(RunStepError::Error(err));
      }
    }
    Ok(true)
  }

  fn run_next_task_limited_handling_errors_inner<F>(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    on_error: &mut F,
  ) -> RunStepResult<bool>
  where
    F: FnMut(Error),
  {
    self.queue_due_timers().map_err(RunStepError::Error)?;

    // Same reasoning as `run_next_task_limited`: don't drop tasks when we hit `MaxTasks`.
    if self.pending_tasks == 0
      && !self
        .queue_next_idle_callback_if_idle()
        .map_err(RunStepError::Error)?
    {
      return Ok(false);
    }

    run_state.check_deadline()?;
    run_state.before_task()?;

    let Some(task) = self.pop_next_task() else {
      return Ok(false);
    };

    let mut trace_span = self.trace.span("js.task.run", "js");
    trace_span.arg_str("source", task_source_name(task.source));
    trace_span.arg_u64("seq", task.seq);

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    let previous_idle_callback_turn = self.in_idle_callback_turn;
    self.in_idle_callback_turn =
      self.in_idle_callback_turn || task.source == TaskSource::IdleCallback;

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    self.currently_running_task = None;
    if let Err(err) = task_result {
      on_error(err);
    }

    let microtask_result =
      self.perform_microtask_checkpoint_limited_handling_errors_inner(host, run_state, on_error);
    self.in_idle_callback_turn = previous_idle_callback_turn;
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    microtask_result?;
    Ok(true)
  }

  fn perform_microtask_checkpoint_limited_inner(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> RunStepResult<()> {
    if self.performing_microtask_checkpoint {
      return Ok(());
    }

    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let mut trace_span = self.trace.span("js.microtask_checkpoint", "js");
    trace_span.arg_u64("queued_at_start", self.microtask_queue.len() as u64);
    let mut drained: u64 = 0;
    let result = (|| -> RunStepResult<()> {
      let mut first_err: Option<Error> = None;

      loop {
        while !self.microtask_queue.is_empty() {
          // Continue draining even if a microtask fails, but still respect run limits/deadlines.
          // If an error has already occurred, prefer surfacing that error over a stop reason, since
          // the caller is already in an exceptional state.
          if let Err(err) = run_state.check_deadline() {
            return match first_err {
              Some(err) => Err(RunStepError::Error(err)),
              None => Err(err),
            };
          }
          if let Err(err) = run_state.before_microtask() {
            return match first_err {
              Some(err) => Err(RunStepError::Error(err)),
              None => Err(err),
            };
          }

          let Some(task) = self.microtask_queue.pop_front() else {
            break;
          };
          self.currently_running_task = Some(RunningTask {
            source: task.source,
            is_microtask: true,
          });
          if let Err(err) = task.run(host, self) {
            if first_err.is_none() {
              first_err = Some(err);
            }
          }
          drained = drained.saturating_add(1);
        }

        let hooks = self.microtask_checkpoint_hooks.clone();
        for hook in hooks.iter().copied() {
          // Respect deadlines even when the event loop microtask queue is empty: hooks may perform
          // additional microtask work (e.g. draining JS engine Promise job queues).
          if let Err(err) = run_state.check_deadline() {
            return match first_err {
              Some(err) => Err(RunStepError::Error(err)),
              None => Err(err),
            };
          }

          if let Err(err) = hook(host, self) {
            if first_err.is_none() {
              first_err = Some(err);
            }
          }
        }

        if self.microtask_queue.is_empty() {
          break;
        }
      }

      if let Some(err) = first_err {
        return Err(RunStepError::Error(err));
      }
      Ok(())
    })();

    trace_span.arg_u64("drained", drained);
    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  fn perform_microtask_checkpoint_limited_handling_errors_inner<F>(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    on_error: &mut F,
  ) -> RunStepResult<()>
  where
    F: FnMut(Error),
  {
    if self.performing_microtask_checkpoint {
      return Ok(());
    }

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let result = (|| -> RunStepResult<()> {
      loop {
        while !self.microtask_queue.is_empty() {
          run_state.check_deadline()?;
          run_state.before_microtask()?;

          let Some(task) = self.microtask_queue.pop_front() else {
            break;
          };
          self.currently_running_task = Some(RunningTask {
            source: task.source,
            is_microtask: true,
          });
          if let Err(err) = task.run(host, self) {
            on_error(err);
          }
        }

        let hooks = self.microtask_checkpoint_hooks.clone();
        for hook in hooks.iter().copied() {
          run_state.check_deadline()?;
          if let Err(err) = hook(host, self) {
            on_error(err);
          }
        }

        if self.microtask_queue.is_empty() {
          break;
        }
      }
      Ok(())
    })();

    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  fn run_animation_frame_inner(&mut self, host: &mut Host) -> Result<RunAnimationFrameOutcome> {
    self.run_animation_frame_inner_with_error_handler(host, None)
  }

  fn run_animation_frame_inner_with_error_handler(
    &mut self,
    host: &mut Host,
    mut on_error: Option<&mut dyn FnMut(Error)>,
  ) -> Result<RunAnimationFrameOutcome> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    // Animation frame turns represent a rendering opportunity, which ends any active idle period.
    // Ensure a subsequent idle period computes a fresh deadline rather than reusing the previous
    // one (which may have been clamped by an earlier pending rAF).
    self.idle_period_deadline = None;

    // Integrate renderer-level cancellation/deadlines.
    let stage = render_control::active_stage().unwrap_or(self.default_deadline_stage);
    render_control::check_active(stage)?;

    // If all callbacks have been cancelled, clear out any stale IDs from the queue.
    if self.animation_frame_callbacks.is_empty() {
      self.animation_frame_queue.clear();
    }

    // Snapshot semantics: callbacks queued during this frame are deferred to the next one.
    let queued_at_start = self.animation_frame_queue.len();
    let mut trace_span = self.trace.span("js.animation_frame.run", "js");
    trace_span.arg_u64("queued_at_start", queued_at_start as u64);
    let mut queue = std::mem::take(&mut self.animation_frame_queue);
    let now = self.now();
    let timestamp = duration_to_ms_f64(now.saturating_sub(self.raf_time_origin));

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      // Treat rAF as script execution for the purposes of "is the JS stack empty?" checks.
      source: TaskSource::Script,
      is_microtask: false,
    });

    let callbacks_result = (|| -> Result<usize> {
      let mut executed = 0usize;
      while let Some(id) = queue.front().copied() {
        // Integrate renderer-level cancellation/deadlines.
        //
        // IMPORTANT: check before popping so a timeout/cancel does not drop the next queued
        // callback ID. Dropping an ID without running/canceling the corresponding callback makes
        // it unreachable (it remains in `animation_frame_callbacks` but is no longer in the
        // scheduling queue).
        render_control::check_active(stage)?;
        let _ = queue.pop_front();
        let Some(mut callback) = self.animation_frame_callbacks.remove(&id) else {
          continue;
        };
        if let Err(err) = (callback)(host, self, timestamp) {
          if let Some(handler) = on_error.as_mut() {
            (*handler)(err);
          } else {
            return Err(err);
          }
        }
        executed += 1;
      }
      Ok(executed)
    })();

    // Always clear running-task state so errors don't leave the event loop in a "running" state.
    self.currently_running_task = None;

    // HTML event loop semantics: an animation frame "turn" ends with a microtask checkpoint.
    // This checkpoint must run even when there were no rAF callbacks (e.g. microtasks queued by
    // the embedder before driving `run_animation_frame`).
    let microtask_err: Option<Error> = match self.perform_microtask_checkpoint(host) {
      Ok(()) => None,
      Err(err) => {
        if let Some(handler) = on_error.as_mut() {
          (*handler)(err);
          None
        } else {
          Some(err)
        }
      }
    };

    self.currently_running_task = previous_running_task;
    if self.animation_frame_callbacks.is_empty() {
      // Avoid accumulating canceled IDs in the scheduling queue when all callbacks are gone.
      self.animation_frame_queue.clear();
    }

    match callbacks_result {
      Ok(executed) => {
        trace_span.arg_u64("executed", executed as u64);
        if let Some(err) = microtask_err {
          return Err(err);
        }
        if executed == 0 {
          Ok(RunAnimationFrameOutcome::Idle)
        } else {
          Ok(RunAnimationFrameOutcome::Ran {
            callbacks: executed,
          })
        }
      }
      Err(err) => {
        // Preserve any callbacks we did not get to execute.
        //
        // `run_animation_frame` snapshot-drains `animation_frame_queue` at the start of a frame so
        // callbacks queued during the frame are deferred to the next one. If the frame aborts
        // early (e.g. callback error without an error handler, or a renderer deadline), `queue`
        // still contains the remaining snapshot IDs. We must reattach them; otherwise callbacks
        // become unreachable (IDs removed from the queue but still present in
        // `animation_frame_callbacks`).
        queue.append(&mut self.animation_frame_queue);
        self.animation_frame_queue = queue;

        // Prefer surfacing the callback error if both the frame and checkpoint failed: this mirrors
        // `run_next_task` error precedence.
        let _ = microtask_err;
        Err(err)
      }
    }
  }

  pub(crate) fn pending_task_count(&self) -> usize {
    self.pending_tasks
  }

  fn maybe_compact_animation_frame_queue(&mut self) {
    let live = self.animation_frame_callbacks.len();
    if live == 0 {
      // If all callbacks are canceled we can drop any queued IDs eagerly.
      self.animation_frame_queue.clear();
      return;
    }

    // `animation_frame_queue` can contain stale IDs for canceled callbacks. Since `VecDeque` does
    // not support removal-by-key, those stale entries would otherwise accumulate unboundedly if
    // attacker-controlled JS repeatedly schedules/cancels animation frame callbacks while keeping
    // at least one callback pending (so the queue is not cleared).
    //
    // Compact opportunistically when the queue grows noticeably larger than the set of live
    // callbacks.
    let queue_len = self.animation_frame_queue.len();
    let should_compact = queue_len > self.queue_limits.max_pending_animation_frame_callbacks
      || queue_len > live.saturating_mul(2).max(64);
    if !should_compact {
      return;
    }

    let callbacks = &self.animation_frame_callbacks;
    self
      .animation_frame_queue
      .retain(|id| callbacks.contains_key(id));
  }
  fn maybe_compact_timer_queue(&mut self) {
    // `timer_queue` can contain stale entries for cleared timers (and for interval timers that have
    // since been rescheduled). Since `BinaryHeap` does not support removal-by-key, those stale
    // entries would otherwise accumulate unboundedly if attacker-controlled JS repeatedly
    // schedules/cancels timers (especially with long delays).
    //
    // Compact opportunistically when the heap grows noticeably larger than the set of live timers.
    let live = self.timers.len();
    let heap_len = self.timer_queue.len();
    let should_compact =
      heap_len > self.queue_limits.max_pending_timers || heap_len > live.saturating_mul(2).max(64);
    if !should_compact {
      return;
    }

    let mut entries = std::mem::take(&mut self.timer_queue).into_vec();
    entries.retain(|Reverse((_due, schedule_seq, id))| {
      self
        .timers
        .get(id)
        .is_some_and(|timer| timer.schedule_seq == *schedule_seq)
    });
    self.timer_queue = BinaryHeap::from(entries);
  }

  fn pop_next_task(&mut self) -> Option<Task<Host>> {
    if self.pending_tasks == 0 {
      return None;
    }

    let mut chosen_idx: Option<usize> = None;
    let mut chosen_seq: u64 = u64::MAX;
    for (idx, queue) in self.task_queues.iter().enumerate() {
      if let Some(task) = queue.front() {
        if task.seq < chosen_seq {
          chosen_seq = task.seq;
          chosen_idx = Some(idx);
        }
      }
    }
    let idx = chosen_idx?;
    let task = self.task_queues[idx].pop_front();
    if task.is_some() {
      debug_assert!(self.pending_tasks > 0);
      self.pending_tasks -= 1;
    }
    task
  }

  fn add_timer(
    &mut self,
    kind: TimerKind,
    requested_delay: Duration,
    interval: Option<Duration>,
    callback: TimerCallback<Host>,
  ) -> Result<TimerId> {
    self.maybe_compact_timer_queue();
    if self.timers.len() >= self.queue_limits.max_pending_timers {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending timers (limit={})",
        self.queue_limits.max_pending_timers
      )));
    }

    let id: TimerId = loop {
      if self.next_timer_id == 0 {
        self.next_timer_id = 1;
      }
      let id = self.next_timer_id;
      self.next_timer_id = self.next_timer_id.wrapping_add(1);
      if self.next_timer_id == 0 {
        self.next_timer_id = 1;
      }
      if !self.timers.contains_key(&id) {
        break id;
      }
    };

    let delay = self.clamp_timer_delay(requested_delay);
    let now = self.clock.now();
    let due = now.checked_add(delay).unwrap_or(Duration::MAX);

    let schedule_seq = self.next_timer_seq;
    self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

    self
      .timers
      .try_reserve(1)
      .map_err(|err| Error::Other(format!("EventLoop timers allocation failed: {err}")))?;
    self
      .timer_queue
      .try_reserve(1)
      .map_err(|err| Error::Other(format!("EventLoop timer queue allocation failed: {err}")))?;
    self.timers.insert(
      id,
      TimerState {
        kind,
        callback: Some(callback),
        interval,
        due,
        schedule_seq,
        nesting_level: self.timer_nesting_level.saturating_add(1),
      },
    );
    self.timer_queue.push(Reverse((due, schedule_seq, id)));
    Ok(id)
  }

  fn clamp_timer_delay(&self, requested: Duration) -> Duration {
    const MIN_NESTED_DELAY: Duration = Duration::from_millis(4);
    // HTML timer nesting clamping (HTML Standard: timer initialization steps,
    // https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html#timer-initialisation-steps):
    // "If nesting level is greater than 5, and timeout is less than 4, then set timeout to 4."
    //
    // In this implementation, `timer_nesting_level` tracks the *currently executing* timer task's
    // "timer nesting level" (and is reset to 0 for non-timer tasks). When a timer schedules
    // another timer (including `setInterval` rescheduling itself), that scheduling observes the
    // current task's nesting level and may clamp the requested delay.
    if self.timer_nesting_level > 5 {
      requested.max(MIN_NESTED_DELAY)
    } else {
      requested
    }
  }

  fn queue_due_timers(&mut self) -> Result<()> {
    // Drain any tasks queued from other threads (e.g. WebSocket network callbacks) into the normal
    // task queues before determining what work is runnable.
    self.drain_external_tasks()?;
    self.queue_due_idle_callbacks()?;
    let now = self.clock.now();
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      if due > now {
        break;
      }

      // Discard stale entries (cleared or rescheduled timers) before attempting to enqueue.
      match self.timers.get(&id) {
        Some(timer) if timer.schedule_seq == schedule_seq => {}
        _ => {
          // Stale queue entry: timer was cleared or rescheduled since it was pushed.
          let _ = self.timer_queue.pop();
          continue;
        }
      }

      if let Err(err) = self.queue_task(TaskSource::Timer, move |host, event_loop| {
        // HTML timers validate the global ID→uniqueHandle map at *task execution time* so that
        // `clearTimeout`/`clearInterval` (and potential ID reuse) can cancel already-queued tasks.
        event_loop.fire_timer(host, id, schedule_seq)
      }) {
        // If the task queue is full (or allocation fails), keep the heap entry so the timer can be
        // retried later.
        return Err(err);
      }

      // Only drop the heap entry once the corresponding timer task is successfully queued.
      let _ = self.timer_queue.pop();
    }
    Ok(())
  }

  fn queue_due_idle_callbacks(&mut self) -> Result<()> {
    let now = self.clock.now();
    if self.idle_callback_queue.is_empty() {
      return Ok(());
    }

    // Promote any timed-out idle callbacks into the normal task queue so they can run even when
    // regular tasks keep the event loop busy.
    //
    // Important: do not permanently remove an ID from `idle_callback_queue` until we have
    // successfully queued the corresponding task. Otherwise, a failure to enqueue (due to queue
    // limits or allocation failure) would leak the callback by making it unreachable.
    let mut idx: usize = 0;
    while idx < self.idle_callback_queue.len() {
      let id = match self.idle_callback_queue.get(idx).copied() {
        Some(id) => id,
        None => break,
      };

      let Some(state) = self.idle_callbacks.get(&id) else {
        // Stale ID: callback was cancelled/cleared.
        let _ = self.idle_callback_queue.remove(idx);
        continue;
      };

      if state.timeout_at.is_some_and(|t| t <= now) {
        let schedule_seq = state.schedule_seq;
        if let Err(err) = self.queue_task(TaskSource::IdleCallback, move |host, event_loop| {
          event_loop.fire_idle_callback(host, id, schedule_seq)
        }) {
          // Leave the ID in `idle_callback_queue` so it can be promoted later.
          return Err(err);
        }

        // Only remove the ID once the task has been successfully queued.
        let _ = self.idle_callback_queue.remove(idx);
        continue;
      }

      idx += 1;
    }

    Ok(())
  }

  fn queue_next_idle_callback_if_idle(&mut self) -> Result<bool> {
    if self.idle_callback_queue.is_empty() {
      return Ok(false);
    }
    // Only run non-timed-out idle callbacks when the event loop is otherwise idle: no pending
    // tasks or microtasks.
    if self.pending_tasks != 0 || !self.microtask_queue.is_empty() {
      self.idle_period_deadline = None;
      return Ok(false);
    }

    // If the idle period deadline has expired, stop scheduling non-timeout idle callbacks and
    // defer remaining callbacks to a future idle period.
    let now = self.clock.now();
    if self
      .idle_period_deadline
      .is_some_and(|deadline| now >= deadline)
    {
      self.idle_period_deadline = None;
      return Ok(false);
    }

    loop {
      let Some(id) = self.idle_callback_queue.front().copied() else {
        return Ok(false);
      };
      let Some(state) = self.idle_callbacks.get(&id) else {
        // Stale ID: callback was cancelled/cleared.
        let _ = self.idle_callback_queue.pop_front();
        continue;
      };

      let schedule_seq = state.schedule_seq;
      // Only remove the ID once the task has been successfully queued so that enqueue failures do
      // not leak the callback.
      self.queue_task(TaskSource::IdleCallback, move |host, event_loop| {
        event_loop.fire_idle_callback(host, id, schedule_seq)
      })?;
      let _ = self.idle_callback_queue.pop_front();
      return Ok(true);
    }
  }

  fn fire_idle_callback(
    &mut self,
    host: &mut Host,
    id: IdleCallbackId,
    schedule_seq: u64,
  ) -> Result<()> {
    let Some(mut state) = self.idle_callbacks.remove(&id) else {
      return Ok(());
    };
    if state.schedule_seq != schedule_seq {
      // Stale task for a cleared/reused handle.
      return Ok(());
    }

    let now = self.clock.now();
    let did_timeout = state.timeout_at.is_some_and(|t| t <= now);

    let remaining_ms: f64 = if did_timeout {
      // Timed-out idle callbacks run even when the event loop is busy and do not observe an idle
      // period budget.
      self.idle_period_deadline = None;
      0.0
    } else {
      let deadline = match self.idle_period_deadline {
        Some(deadline) => deadline,
        None => {
          let mut deadline = now.checked_add(DEFAULT_IDLE_BUDGET).unwrap_or(Duration::MAX);
          if self.has_pending_animation_frame_callbacks() {
            // When rendering work is pending (e.g. requestAnimationFrame callbacks), HTML clamps the
            // idle period deadline so idle callbacks don't assume a full 50ms budget when a render
            // opportunity is imminent.
            let frame_deadline = now.checked_add(DEFAULT_FRAME_BUDGET).unwrap_or(Duration::MAX);
            deadline = deadline.min(frame_deadline);
          }
          if let Some(next_due) = self.next_timer_due_time() {
            deadline = deadline.min(next_due);
          }
          self.idle_period_deadline = Some(deadline);
          deadline
        }
      };

      // The caller should avoid dispatching idle callbacks after the deadline has expired, but be
      // defensive in case time advances between scheduling and execution.
      if now >= deadline {
        self.idle_period_deadline = None;
        0.0
       } else {
         let remaining = deadline.saturating_sub(now);
         duration_to_ms_f64(remaining).max(0.0)
       }
     };

    let Some(mut callback) = state.callback.take() else {
      return Err(Error::Other(
        "Idle callback missing while callback is active".to_string(),
      ));
    };

    if self.idle_callbacks.is_empty() {
      // Avoid accumulating stale handles when all callbacks are gone.
      self.idle_callback_queue.clear();
    }

    (callback)(host, self, did_timeout, remaining_ms)
  }

  fn fire_timer(&mut self, host: &mut Host, id: TimerId, generation: u64) -> Result<()> {
    // Execution-time validation: the timer might have been cleared after it became due (or the ID
    // could have been reused). In either case, abort without invoking the callback.
    let Some(timer) = self.timers.get_mut(&id) else {
      return Ok(());
    };
    if timer.schedule_seq != generation {
      return Ok(());
    }
    let kind = timer.kind;
    let interval = timer.interval;
    let nesting_level = timer.nesting_level;
    let Some(mut callback) = timer.callback.take() else {
      return Err(Error::Other(
        "Timer callback missing while timer is active".to_string(),
      ));
    };

    // Update nesting level for the duration of this task (including the microtask checkpoint that
    // `run_next_task` performs after this task returns).
    self.timer_nesting_level = nesting_level;

    let callback_err = match (callback)(host, self) {
      Ok(()) => None,
      Err(err) => Some(err),
    };

    match kind {
      TimerKind::Timeout => {
        // Post-handler validation (mirrors HTML): the timer could have been cleared (or reused)
        // during the callback.
        if self
          .timers
          .get(&id)
          .is_some_and(|timer| timer.schedule_seq == generation)
        {
          self.timers.remove(&id);
        }
      }
      TimerKind::Interval => {
        let Some(interval) = interval else {
          return Err(Error::Other(
            "Interval timer missing interval duration".to_string(),
          ));
        };
        let now = self.clock.now();
        let delay = self.clamp_timer_delay(interval);
        let due = now.checked_add(delay).unwrap_or(Duration::MAX);

        // HTML timer initialization steps (see link above) create a new timer task each time an
        // interval fires ("if repeat is true... perform the timer initialization steps again"),
        // using the currently running timer task's nesting level as the basis and then
        // incrementing it for the new task.
        let next_nesting_level = nesting_level.saturating_add(1);
        let schedule_seq = self.next_timer_seq;
        self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

        // Post-handler validation: the callback may have cleared (or reused) this timer.
        let Some(timer) = self.timers.get_mut(&id) else {
          return callback_err.map_or(Ok(()), Err);
        };
        if timer.schedule_seq != generation {
          return callback_err.map_or(Ok(()), Err);
        }
        timer.callback = Some(callback);
        timer.due = due;
        timer.nesting_level = next_nesting_level;
        timer.schedule_seq = schedule_seq;
        self
          .timer_queue
          .try_reserve(1)
          .map_err(|err| Error::Other(format!("EventLoop timer queue allocation failed: {err}")))?;
        self.timer_queue.push(Reverse((due, schedule_seq, id)));
      }
    }

    callback_err.map_or(Ok(()), Err)
  }

  pub fn spin_until(
    &mut self,
    host: &mut Host,
    limits: RunLimits,
    mut condition: impl FnMut(&Host) -> bool,
  ) -> Result<SpinOutcome> {
    let previous_stage = render_control::active_stage();
    let _stage_guard = StageGuard::install(previous_stage.or(Some(RenderStage::Script)));
    if previous_stage.is_none() {
      record_stage(StageHeartbeat::Script);
    }

    let mut run_state = RunState::new(limits, Arc::clone(&self.clock), self.default_deadline_stage);
    match self.spin_until_inner(host, &mut run_state, &mut condition) {
      Ok(outcome) => Ok(outcome),
      Err(RunStepError::Stop(reason)) => Ok(SpinOutcome::Stopped(reason)),
      Err(RunStepError::Error(err)) => Err(err),
    }
  }

  fn spin_until_inner(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    condition: &mut impl FnMut(&Host) -> bool,
  ) -> RunStepResult<SpinOutcome> {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;

      if !condition(host) {
        return Ok(SpinOutcome::ConditionMet);
      }

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_limited_inner(host, run_state)?;
        continue;
      }

      if self.run_next_task_limited_inner(host, run_state)? {
        continue;
      }

      return Ok(SpinOutcome::Idle);
    }
  }
}

impl<Host: 'static> Drop for EventLoop<Host> {
  fn drop(&mut self) {
    // Ensure tasks queued from other threads cannot accumulate unboundedly after the event loop is
    // dropped (for example during navigation resets).
    self.external_task_queue.close();
  }
}

/// Stateful run bookkeeping for bounded/step-wise event loop execution.
///
/// Embeddings that need deterministic "stepping" can create one `RunState` per run and reuse it
/// across multiple calls to [`EventLoop::run_next_task_limited`] and
/// [`EventLoop::perform_microtask_checkpoint_limited`].
pub struct RunState {
  limits: RunLimits,
  clock: Arc<dyn Clock>,
  started_at: Duration,
  default_deadline_stage: RenderStage,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl RunState {
  pub fn new(
    limits: RunLimits,
    clock: Arc<dyn Clock>,
    default_deadline_stage: RenderStage,
  ) -> Self {
    Self {
      limits,
      started_at: clock.now(),
      clock,
      default_deadline_stage,
      tasks_executed: 0,
      microtasks_executed: 0,
    }
  }

  pub fn limits(&self) -> RunLimits {
    self.limits
  }

  pub fn tasks_executed(&self) -> usize {
    self.tasks_executed
  }

  pub fn microtasks_executed(&self) -> usize {
    self.microtasks_executed
  }

  pub fn tasks_remaining(&self) -> usize {
    self.limits.max_tasks.saturating_sub(self.tasks_executed)
  }

  pub fn microtasks_remaining(&self) -> usize {
    self
      .limits
      .max_microtasks
      .saturating_sub(self.microtasks_executed)
  }

  pub fn elapsed_wall_time(&self) -> Duration {
    self.clock.now().saturating_sub(self.started_at)
  }

  pub fn wall_time_remaining(&self) -> Option<Duration> {
    self
      .limits
      .max_wall_time
      .map(|limit| limit.saturating_sub(self.elapsed_wall_time()))
  }

  fn check_deadline(&self) -> RunStepResult<()> {
    // Integrate renderer-level cancellation/deadlines.
    let stage = render_control::active_stage().unwrap_or(self.default_deadline_stage);
    render_control::check_active(stage).map_err(|err| RunStepError::Error(err.into()))?;

    let Some(max_wall_time) = self.limits.max_wall_time else {
      return Ok(());
    };
    let elapsed = self.clock.now().saturating_sub(self.started_at);
    if elapsed > max_wall_time {
      return Err(RunStepError::Stop(RunUntilIdleStopReason::WallTime {
        elapsed,
        limit: max_wall_time,
      }));
    }
    Ok(())
  }

  fn before_task(&mut self) -> RunStepResult<()> {
    if self.tasks_executed >= self.limits.max_tasks {
      return Err(RunStepError::Stop(RunUntilIdleStopReason::MaxTasks {
        executed: self.tasks_executed,
        limit: self.limits.max_tasks,
      }));
    }
    self.tasks_executed += 1;
    Ok(())
  }

  fn before_microtask(&mut self) -> RunStepResult<()> {
    if self.microtasks_executed >= self.limits.max_microtasks {
      return Err(RunStepError::Stop(RunUntilIdleStopReason::MaxMicrotasks {
        executed: self.microtasks_executed,
        limit: self.limits.max_microtasks,
      }));
    }
    self.microtasks_executed += 1;
    Ok(())
  }
}

type RunStepResult<T> = std::result::Result<T, RunStepError>;

enum RunStepError {
  Stop(RunUntilIdleStopReason),
  Error(Error),
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::VirtualClock;
  use crate::{error::RenderError, render_control::RenderDeadline};
  use std::cell::Cell;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::sync::Mutex;

  #[derive(Default)]
  struct TestHost {
    log: Vec<&'static str>,
    count: usize,
  }

  #[test]
  fn microtask_checkpoint_hooks_are_multiplexed_in_insertion_order() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    fn hook_a(host: &mut Host, _event_loop: &mut EventLoop<Host>) -> Result<()> {
      host.log.push("hook_a");
      Ok(())
    }

    fn hook_b(host: &mut Host, _event_loop: &mut EventLoop<Host>) -> Result<()> {
      host.log.push("hook_b");
      Ok(())
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.add_microtask_checkpoint_hook(hook_a)?;
    event_loop.add_microtask_checkpoint_hook(hook_b)?;

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;

    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(host.log, vec!["microtask", "hook_a", "hook_b"]);
    Ok(())
  }

  #[test]
  fn reset_for_navigation_closes_old_external_task_queue_handles() {
    let mut event_loop = EventLoop::<TestHost>::new();
    let old_handle = event_loop.external_task_queue_handle();

    event_loop.reset_for_navigation(TraceHandle::default(), QueueLimits::default());

    let err = old_handle
      .queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))
      .expect_err("expected old external task queue handle to be closed after reset");
    assert!(
      err.to_string().contains("closed"),
      "expected error message to mention \"closed\", got: {err}"
    );

    event_loop
      .external_task_queue_handle()
      .queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))
      .expect("expected new external task queue handle to be live after reset");
  }

  #[test]
  fn reset_for_navigation_preserves_external_task_queue_wake_callback() -> Result<()> {
    let mut event_loop = EventLoop::<TestHost>::new();
    let handle = event_loop.external_task_queue_handle();

    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_for_cb = Arc::clone(&wakes);
    handle.set_wake_callback(Some(Arc::new(move || {
      wakes_for_cb.fetch_add(1, Ordering::SeqCst);
    })));

    event_loop.reset_for_navigation(TraceHandle::default(), QueueLimits::default());

    // The old handle should be closed (existing test covers this), but the wake callback should be
    // reinstalled on the new queue.
    let new_handle = event_loop.external_task_queue_handle();
    new_handle.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    Ok(())
  }

  #[test]
  fn add_microtask_checkpoint_hook_dedupes_duplicate_registrations() -> Result<()> {
    #[derive(Default)]
    struct Host {
      calls: usize,
    }

    fn hook(host: &mut Host, _event_loop: &mut EventLoop<Host>) -> Result<()> {
      host.calls += 1;
      Ok(())
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.add_microtask_checkpoint_hook(hook)?;
    event_loop.add_microtask_checkpoint_hook(hook)?;

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.calls, 1);
    Ok(())
  }

  #[test]
  fn external_task_queue_wake_callback_invoked_once_per_queued_task() -> Result<()> {
    let handle = ExternalTaskQueueHandle::<()>::new(10);
    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_for_cb = Arc::clone(&wakes);
    handle.set_wake_callback(Some(Arc::new(move || {
      wakes_for_cb.fetch_add(1, Ordering::SeqCst);
    })));

    handle.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;
    assert_eq!(wakes.load(Ordering::SeqCst), 1);

    handle.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;
    assert_eq!(wakes.load(Ordering::SeqCst), 2);

    Ok(())
  }

  #[test]
  fn external_task_queue_wake_callback_not_invoked_when_queue_is_full() -> Result<()> {
    let handle = ExternalTaskQueueHandle::<()>::new(1);
    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_for_cb = Arc::clone(&wakes);
    handle.set_wake_callback(Some(Arc::new(move || {
      wakes_for_cb.fetch_add(1, Ordering::SeqCst);
    })));

    handle.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;
    assert_eq!(wakes.load(Ordering::SeqCst), 1);

    assert!(handle
      .queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))
      .is_err());
    assert_eq!(wakes.load(Ordering::SeqCst), 1);

    Ok(())
  }

  #[test]
  fn external_task_queue_wake_callback_not_invoked_when_queue_is_closed() {
    let handle = ExternalTaskQueueHandle::<()>::new(10);
    handle.close();

    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_for_cb = Arc::clone(&wakes);
    handle.set_wake_callback(Some(Arc::new(move || {
      wakes_for_cb.fetch_add(1, Ordering::SeqCst);
    })));

    assert!(handle
      .queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))
      .is_err());
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
  }

  #[test]
  fn external_task_queue_wake_callback_is_invoked_outside_mutex() -> Result<()> {
    let handle = ExternalTaskQueueHandle::<()>::new(10);
    let handle_for_cb = handle.clone();
    let wake_ran = Arc::new(AtomicBool::new(false));
    let wake_ran_for_cb = Arc::clone(&wake_ran);
    handle.set_wake_callback(Some(Arc::new(move || {
      // If the queue mutex were held while calling the wake callback, this would deadlock.
      let empty = handle_for_cb.is_empty();
      wake_ran_for_cb.store(!empty, Ordering::SeqCst);
    })));

    handle.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;
    assert!(
      wake_ran.load(Ordering::SeqCst),
      "wake callback should have been invoked and observed the queued task"
    );
    Ok(())
  }

  #[test]
  fn external_task_queue_wake_callback_panics_are_swallowed() -> Result<()> {
    let handle = ExternalTaskQueueHandle::<()>::new(10);
    handle.set_wake_callback(Some(Arc::new(|| {
      panic!("wake callback panic should be caught");
    })));

    // Should not panic even though the wake callback panics.
    assert!(handle
      .queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))
      .is_ok());
    assert!(!handle.is_empty(), "task should still be enqueued");
    Ok(())
  }

  #[test]
  fn cooperative_timeout_during_event_loop_is_attributed_to_script_stage() {
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let deadline = render_control::RenderDeadline::new(None, Some(Arc::new(|| true)));
    let _deadline_guard = render_control::DeadlineGuard::install(Some(&deadline));

    let mut host = Host;
    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected timeout");
    match err {
      Error::Render(crate::error::RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Script);
      }
      err => panic!("expected RenderError::Timeout, got {err:?}"),
    }
  }

  #[test]
  fn run_until_idle_records_script_stage_heartbeat_when_no_stage_active() -> Result<()> {
    struct Host;

    let mut host = Host;
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    {
      let _listener_guard = render_control::push_stage_listener(Some(Arc::new(move |stage| {
        stages_for_listener
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push(stage);
      })));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    }

    assert_eq!(
      stages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_slice(),
      &[StageHeartbeat::Script]
    );
    Ok(())
  }

  #[test]
  fn run_next_task_records_script_stage_heartbeat_when_no_stage_active() -> Result<()> {
    struct Host;

    let mut host = Host;
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    {
      let _listener_guard = render_control::push_stage_listener(Some(Arc::new(move |stage| {
        stages_for_listener
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .push(stage);
      })));
      assert!(event_loop.run_next_task(&mut host)?);
    }

    assert_eq!(
      stages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_slice(),
      &[StageHeartbeat::Script]
    );
    Ok(())
  }

  #[test]
  fn run_until_idle_handling_errors_installs_script_stage_guard_for_tasks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<Option<RenderStage>>,
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.observed.push(render_control::active_stage());
      Ok(())
    })?;

    assert_eq!(render_control::active_stage(), None);
    assert_eq!(
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_| {})?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.observed, vec![Some(RenderStage::Script)]);
    assert_eq!(render_control::active_stage(), None);
    Ok(())
  }

  #[test]
  fn run_until_idle_with_hook_runs_after_checkpoint_and_task() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("nested_microtask");
        Ok(())
      })?;
      Ok(())
    })?;

    let mut hooks = 0usize;
    assert_eq!(
      event_loop.run_until_idle_with_hook(
        &mut host,
        RunLimits::unbounded(),
        |host, _event_loop| {
          hooks += 1;
          host.log.push("hook");
          Ok(())
        }
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(hooks, 2);
    assert_eq!(
      host.log,
      vec!["microtask", "hook", "task", "nested_microtask", "hook"]
    );
    Ok(())
  }

  #[test]
  fn run_until_idle_handling_errors_with_hook_reports_task_error_and_continues() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let mut host = Host::default();
    let mut errors: Vec<String> = Vec::new();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task1");
      Err(Error::Other("boom".to_string()))
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle_handling_errors_with_hook(
        &mut host,
        RunLimits::unbounded(),
        |err| match err {
          Error::Other(msg) => errors.push(msg),
          other => errors.push(other.to_string()),
        },
        |_host, _event_loop| Ok(()),
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["task1", "task2"]);
    assert_eq!(errors, vec!["boom".to_string()]);
    Ok(())
  }

  #[test]
  fn run_until_idle_handling_errors_with_hook_runs_after_checkpoint_and_task() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("nested_microtask");
        Ok(())
      })?;
      Ok(())
    })?;

    let mut hooks = 0usize;
    assert_eq!(
      event_loop.run_until_idle_handling_errors_with_hook(
        &mut host,
        RunLimits::unbounded(),
        |_| {},
        |host, _event_loop| {
          hooks += 1;
          host.log.push("hook");
          Ok(())
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(hooks, 2);
    assert_eq!(
      host.log,
      vec!["microtask", "hook", "task", "nested_microtask", "hook"]
    );
    Ok(())
  }

  #[test]
  fn run_until_idle_handling_errors_with_hook_aborts_on_hook_error() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let mut host = Host::default();
    let mut errors: Vec<String> = Vec::new();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task1");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    let err = event_loop
      .run_until_idle_handling_errors_with_hook(
        &mut host,
        RunLimits::unbounded(),
        |err| errors.push(err.to_string()),
        |_host, _event_loop| Err(Error::Other("hook failed".to_string())),
      )
      .expect_err("expected hook failure to abort the run");
    assert!(matches!(err, Error::Other(msg) if msg == "hook failed"));
    assert_eq!(host.log, vec!["task1"]);
    assert!(errors.is_empty(), "hook errors should not go to on_error");
    Ok(())
  }

  #[test]
  fn run_until_idle_handling_errors_respects_existing_stage_guard() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<Option<RenderStage>>,
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.observed.push(render_control::active_stage());
      Ok(())
    })?;

    {
      let _outer_guard = StageGuard::install(Some(RenderStage::Layout));
      assert_eq!(
        event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |_| {})?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.observed, vec![Some(RenderStage::Layout)]);
      assert_eq!(render_control::active_stage(), Some(RenderStage::Layout));
    }
    assert_eq!(render_control::active_stage(), None);
    Ok(())
  }

  #[test]
  fn microtasks_run_after_a_task_and_before_the_next_task() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task1");
      event_loop.queue_microtask(|host, _| {
        host.log.push("microtask1");
        Ok(())
      })?;
      Ok(())
    })?;

    event_loop.queue_task(TaskSource::Script, |host, _| {
      host.log.push("task2");
      Ok(())
    })?;

    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.log, vec!["task1", "microtask1", "task2"]);
    Ok(())
  }

  #[test]
  fn microtasks_queued_by_microtasks_run_in_the_same_checkpoint() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop.queue_microtask(|host, event_loop| {
      host.log.push("microtask1");
      event_loop.queue_microtask(|host, _| {
        host.log.push("microtask2");
        Ok(())
      })?;
      Ok(())
    })?;

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.log, vec!["microtask1", "microtask2"]);
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_reentrancy_guard_prevents_recursion() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop.queue_microtask(|host, event_loop| {
      host.count += 1;
      event_loop.perform_microtask_checkpoint(host)?;
      Ok(())
    })?;

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.count, 1);
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_runs_remaining_microtasks_after_error() {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop
      .queue_microtask(|host, _| {
        host.log.push("microtask1");
        Err(Error::Other("boom".to_string()))
      })
      .unwrap();
    event_loop
      .queue_microtask(|host, _| {
        host.log.push("microtask2");
        Ok(())
      })
      .unwrap();

    let err = event_loop
      .perform_microtask_checkpoint(&mut host)
      .expect_err("expected microtask error to be surfaced");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.log, vec!["microtask1", "microtask2"]);
    assert_eq!(event_loop.pending_microtask_count(), 0);
  }

  #[test]
  fn run_until_idle_drains_remaining_microtasks_after_error() {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);
    event_loop
      .queue_microtask(|host, _| {
        host.log.push("microtask1");
        Err(Error::Other("boom".to_string()))
      })
      .unwrap();
    event_loop
      .queue_microtask(|host, _| {
        host.log.push("microtask2");
        Ok(())
      })
      .unwrap();

    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected microtask error to be surfaced");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.log, vec!["microtask1", "microtask2"]);
    assert_eq!(event_loop.pending_microtask_count(), 0);
  }

  fn self_requeue_microtask(
    host: &mut TestHost,
    event_loop: &mut EventLoop<TestHost>,
  ) -> Result<()> {
    host.count += 1;
    event_loop.queue_microtask(self_requeue_microtask)?;
    Ok(())
  }

  #[test]
  fn run_limits_stop_infinite_microtask_chains() {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop.queue_microtask(self_requeue_microtask).unwrap();

    let result = event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 100,
        max_microtasks: 5,
        max_wall_time: None,
      },
    );
    assert!(matches!(
      result,
      Ok(RunUntilIdleOutcome::Stopped(
        RunUntilIdleStopReason::MaxMicrotasks { .. }
      ))
    ));
    assert_eq!(host.count, 5);
  }

  #[test]
  fn limited_microtask_checkpoint_stops_infinite_microtask_chains() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    event_loop.queue_microtask(self_requeue_microtask)?;

    let mut run_state = RunState::new(
      RunLimits {
        max_tasks: usize::MAX,
        max_microtasks: 5,
        max_wall_time: None,
      },
      clock,
      event_loop.default_deadline_stage(),
    );

    assert_eq!(
      event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state)?,
      MicrotaskCheckpointLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxMicrotasks {
        executed: 5,
        limit: 5
      })
    );
    assert_eq!(host.count, 5);
    assert_eq!(run_state.microtasks_executed(), 5);
    // The next microtask should still be queued: the limit is enforced before popping.
    assert_eq!(event_loop.pending_microtask_count(), 1);
    Ok(())
  }

  fn self_requeue_microtask_advancing_clock(
    clock: Arc<VirtualClock>,
    advance_by: Duration,
  ) -> impl FnOnce(&mut TestHost, &mut EventLoop<TestHost>) -> Result<()> {
    move |host, event_loop| {
      host.count += 1;
      clock.advance(advance_by);
      event_loop.queue_microtask(self_requeue_microtask_advancing_clock(
        Arc::clone(&clock),
        advance_by,
      ))?;
      Ok(())
    }
  }

  #[test]
  fn limited_microtask_checkpoint_stops_on_wall_time_before_popping_next_microtask() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(Arc::clone(&clock_for_loop));

    event_loop.queue_microtask(self_requeue_microtask_advancing_clock(
      Arc::clone(&clock),
      Duration::from_millis(10),
    ))?;

    let mut run_state = RunState::new(
      RunLimits {
        max_tasks: usize::MAX,
        max_microtasks: usize::MAX,
        max_wall_time: Some(Duration::from_millis(5)),
      },
      clock_for_loop,
      event_loop.default_deadline_stage(),
    );

    assert_eq!(
      event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state)?,
      MicrotaskCheckpointLimitedOutcome::Stopped(RunUntilIdleStopReason::WallTime {
        elapsed: Duration::from_millis(10),
        limit: Duration::from_millis(5),
      })
    );
    assert_eq!(host.count, 1);
    assert_eq!(run_state.microtasks_executed(), 1);
    // The next microtask should still be queued: the limit is enforced before popping.
    assert_eq!(event_loop.pending_microtask_count(), 1);
    Ok(())
  }

  #[test]
  fn run_next_task_limited_stops_on_wall_time_before_popping_next_task() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(Arc::clone(&clock_for_loop));

    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task1");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    let limits = RunLimits {
      max_tasks: usize::MAX,
      max_microtasks: usize::MAX,
      max_wall_time: Some(Duration::from_millis(5)),
    };
    let mut run_state = RunState::new(
      limits,
      Arc::clone(&clock_for_loop),
      event_loop.default_deadline_stage(),
    );

    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(host.log, vec!["task1"]);
    assert_eq!(run_state.tasks_executed(), 1);

    // Advance virtual time beyond the wall-time budget.
    clock.advance(Duration::from_millis(10));

    // The next task must not be popped once the wall-time budget is exhausted.
    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Stopped(RunUntilIdleStopReason::WallTime {
        elapsed: Duration::from_millis(10),
        limit: Duration::from_millis(5),
      })
    );
    assert_eq!(host.log, vec!["task1"]);
    assert_eq!(run_state.tasks_executed(), 1);
    assert_eq!(event_loop.pending_task_count(), 1);

    // A fresh run state should allow the queued task to run, proving it wasn't dropped.
    let mut run_state2 = event_loop.new_run_state(limits);
    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state2)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(host.log, vec!["task1", "task2"]);
    Ok(())
  }

  #[test]
  fn unbounded_microtask_checkpoint_aborts_infinite_microtask_chains_via_queue_limit() {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);
    let mut limits = QueueLimits::unbounded();
    limits.max_pending_microtasks = 5;
    event_loop.set_queue_limits(limits);

    event_loop.queue_microtask(self_requeue_microtask).unwrap();

    let err = event_loop
      .perform_microtask_checkpoint(&mut host)
      .expect_err("expected microtask checkpoint to abort an infinite chain");
    assert!(
      err
        .to_string()
        .contains("microtask checkpoint exceeded drain limit"),
      "unexpected error: {err}"
    );
    assert_eq!(host.count, 5);
    assert_eq!(
      event_loop.pending_microtask_count(),
      1,
      "expected the next microtask to remain queued"
    );
  }

  #[test]
  fn run_until_idle_max_tasks_does_not_drop_next_task() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::default();

    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task1");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 1,
          max_microtasks: usize::MAX,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks {
        executed: 1,
        limit: 1
      })
    );
    assert_eq!(host.log, vec!["task1"]);

    // Remaining tasks should still be queued for the next run.
    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 1,
          max_microtasks: usize::MAX,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["task1", "task2"]);
    Ok(())
  }

  #[test]
  fn run_next_task_limited_stops_before_dropping_next_task_at_max_tasks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::default();

    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task1");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    let mut run_state = RunState::new(
      RunLimits {
        max_tasks: 1,
        max_microtasks: usize::MAX,
        max_wall_time: None,
      },
      clock.clone(),
      event_loop.default_deadline_stage(),
    );

    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(run_state.tasks_executed(), 1);
    assert_eq!(host.log, vec!["task1"]);

    // The second task should not be popped when the max-task limit is hit.
    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxTasks {
        executed: 1,
        limit: 1
      })
    );
    assert_eq!(run_state.tasks_executed(), 1);
    assert_eq!(host.log, vec!["task1"]);

    // Reset budgets with a fresh run state and verify the second task still runs.
    let mut run_state2 = RunState::new(
      RunLimits {
        max_tasks: 1,
        max_microtasks: usize::MAX,
        max_wall_time: None,
      },
      clock,
      event_loop.default_deadline_stage(),
    );
    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state2)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(host.log, vec!["task1", "task2"]);
    Ok(())
  }

  #[test]
  fn limited_stepping_apis_advance_counters_and_are_reusable() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::default();

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask1");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task1");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("microtask2");
        Ok(())
      })?;
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task2");
      Ok(())
    })?;

    let mut run_state = RunState::new(
      RunLimits {
        max_tasks: 10,
        max_microtasks: 10,
        max_wall_time: None,
      },
      clock,
      event_loop.default_deadline_stage(),
    );

    assert_eq!(run_state.tasks_executed(), 0);
    assert_eq!(run_state.microtasks_executed(), 0);

    assert_eq!(
      event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state)?,
      MicrotaskCheckpointLimitedOutcome::Completed
    );
    assert_eq!(host.log, vec!["microtask1"]);
    assert_eq!(run_state.tasks_executed(), 0);
    assert_eq!(run_state.microtasks_executed(), 1);

    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(host.log, vec!["microtask1", "task1", "microtask2"]);
    assert_eq!(run_state.tasks_executed(), 1);
    assert_eq!(run_state.microtasks_executed(), 2);

    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::Ran
    );
    assert_eq!(host.log, vec!["microtask1", "task1", "microtask2", "task2"]);
    assert_eq!(run_state.tasks_executed(), 2);
    assert_eq!(run_state.microtasks_executed(), 2);

    assert_eq!(
      event_loop.run_next_task_limited(&mut host, &mut run_state)?,
      RunNextTaskLimitedOutcome::NoTask
    );
    assert_eq!(run_state.tasks_executed(), 2);
    Ok(())
  }

  #[test]
  fn run_until_idle_with_zero_task_limit_drains_microtasks_without_dropping_tasks() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      host.log.push("task");
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 0,
          max_microtasks: usize::MAX,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks {
        executed: 0,
        limit: 0
      })
    );
    assert_eq!(host.log, vec!["microtask"]);

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["microtask", "task"]);
    Ok(())
  }

  #[test]
  fn exposes_currently_running_task_inside_tasks_and_microtasks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<RunningTask>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.observed.push(
        event_loop
          .currently_running_task()
          .expect("task should be marked as running"),
      );
      event_loop.queue_microtask(|host, event_loop| {
        host.observed.push(
          event_loop
            .currently_running_task()
            .expect("microtask should be marked as running"),
        );
        Ok(())
      })?;
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      host.observed,
      vec![
        RunningTask {
          source: TaskSource::Script,
          is_microtask: false,
        },
        RunningTask {
          source: TaskSource::Microtask,
          is_microtask: true,
        },
      ]
    );
    assert_eq!(event_loop.currently_running_task(), None);
    Ok(())
  }

  #[test]
  fn clears_currently_running_task_on_task_error() {
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| {
        Err(Error::Other("boom".to_string()))
      })
      .unwrap();

    let mut host = Host;
    let err = event_loop
      .run_next_task(&mut host)
      .expect_err("task should fail");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(event_loop.currently_running_task(), None);
  }

  #[test]
  fn microtasks_run_after_task_error() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("microtask");
        Ok(())
      })?;
      Err(Error::Other("boom".to_string()))
    })?;

    let mut host = Host::default();
    let err = event_loop
      .run_next_task(&mut host)
      .expect_err("task should fail");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));

    // Even though the task failed, the post-task microtask checkpoint should still drain the
    // microtask queue (HTML event loop semantics).
    assert_eq!(host.log, vec!["task", "microtask"]);
    assert_eq!(event_loop.pending_microtask_count(), 0);
    Ok(())
  }

  #[test]
  fn is_quiescent_is_false_when_timer_scheduled_even_if_idle() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);
    assert!(event_loop.is_idle(), "fresh event loop should be idle");
    assert!(
      event_loop.is_quiescent(),
      "fresh event loop should be quiescent"
    );

    let _id = event_loop.set_timeout(Duration::from_millis(10), |_host, _event_loop| Ok(()))?;
    assert!(
      event_loop.is_idle(),
      "scheduling a timer should not make the event loop non-idle"
    );
    assert!(
      !event_loop.is_quiescent(),
      "scheduled timers should keep the event loop non-quiescent"
    );
    Ok(())
  }

  #[test]
  fn is_quiescent_is_true_after_timer_cleared() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    let id = event_loop.set_timeout(Duration::from_millis(10), |_host, _event_loop| Ok(()))?;
    event_loop.clear_timeout(id);

    assert!(
      event_loop.is_quiescent(),
      "clearing the last timer should allow the event loop to become quiescent"
    );
    Ok(())
  }

  #[test]
  fn clear_timeout_after_due_but_before_run_cancels_callback() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    let id = event_loop.set_timeout(Duration::from_millis(0), |host, _event_loop| {
      host.count += 1;
      Ok(())
    })?;

    // Enqueue due timers as runnable tasks without executing them.
    event_loop.queue_due_timers()?;
    event_loop.clear_timeout(id);

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 0);
    Ok(())
  }

  #[test]
  fn queue_due_timers_failure_does_not_leak_timer_queue_entry() -> Result<()> {
    #[derive(Default)]
    struct Host {
      fired: usize,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 1,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });

    // Fill the task queue so queuing due timers fails.
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

    let id = event_loop.set_timeout(Duration::from_millis(0), |host, _event_loop| {
      host.fired += 1;
      Ok(())
    })?;

    let err = event_loop
      .queue_due_timers()
      .expect_err("expected due timer enqueue to fail");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending tasks")),
      "unexpected error: {err:?}"
    );

    // The timer should still be present and still be schedulable.
    assert!(
      event_loop.timers.contains_key(&id),
      "timer should not be dropped from timers map"
    );
    assert_eq!(
      event_loop.next_timer_due_time(),
      Some(Duration::from_millis(0)),
      "timer heap entry should not be lost"
    );

    // Once capacity becomes available again, the timer should still fire.
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 2,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });
    event_loop.queue_due_timers()?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.fired, 1);
    assert!(!event_loop.timers.contains_key(&id));
    Ok(())
  }

  #[test]
  fn queue_due_idle_callbacks_failure_does_not_leak_callback() -> Result<()> {
    #[derive(Default)]
    struct Host {
      fired: usize,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 1,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });

    // Fill the task queue so promoting timed-out idle callbacks fails.
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

    let id = event_loop.request_idle_callback(Some(Duration::from_millis(0)), |host, _, did_timeout, _| {
      assert!(did_timeout);
      host.fired += 1;
      Ok(())
    })?;

    let err = event_loop
      .queue_due_idle_callbacks()
      .expect_err("expected idle callback promotion to fail");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending tasks")),
      "unexpected error: {err:?}"
    );

    assert!(
      event_loop.idle_callbacks.contains_key(&id),
      "idle callback should remain stored"
    );
    assert!(
      event_loop.idle_callback_queue.contains(&id),
      "idle callback ID should remain queued for later promotion"
    );

    // Once capacity becomes available again, promotion should succeed and the callback should run.
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 2,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });
    event_loop.queue_due_idle_callbacks()?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.fired, 1);
    assert!(!event_loop.idle_callbacks.contains_key(&id));
    Ok(())
  }

  #[test]
  fn queue_next_idle_callback_if_idle_failure_does_not_leak_callback() -> Result<()> {
    #[derive(Default)]
    struct Host {
      fired: usize,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 0,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });

    let id = event_loop.request_idle_callback(None, |host, _event_loop, did_timeout, _| {
      assert!(!did_timeout);
      host.fired += 1;
      Ok(())
    })?;

    let err = event_loop
      .queue_next_idle_callback_if_idle()
      .expect_err("expected idle callback enqueue to fail");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending tasks")),
      "unexpected error: {err:?}"
    );

    assert!(
      event_loop.idle_callbacks.contains_key(&id),
      "idle callback should remain stored"
    );
    assert!(
      event_loop.idle_callback_queue.contains(&id),
      "idle callback ID should remain queued for later execution"
    );

    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 1,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });

    assert!(
      event_loop.queue_next_idle_callback_if_idle()?,
      "idle callback should be queued once capacity is available"
    );

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.fired, 1);
    assert!(!event_loop.idle_callbacks.contains_key(&id));
    Ok(())
  }

  #[test]
  fn interval_cleared_inside_callback_does_not_reschedule() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    let id_cell: Rc<Cell<Option<TimerId>>> = Rc::new(Cell::new(None));
    let id_cell_for_cb = Rc::clone(&id_cell);

    let id = event_loop.set_interval(Duration::from_millis(0), move |host, event_loop| {
      host.count += 1;
      let id = id_cell_for_cb.get().expect("interval id should be set");
      event_loop.clear_interval(id);
      Ok(())
    })?;
    id_cell.set(Some(id));

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 1);

    // Even if time advances again, the cleared interval should not fire a second time.
    clock.advance(Duration::from_millis(0));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 1);
    Ok(())
  }

  #[test]
  fn nested_timeouts_are_clamped_to_minimum_delay_when_nesting_level_exceeds_five() -> Result<()> {
    fn schedule(event_loop: &mut EventLoop<TestHost>, target: usize) -> Result<()> {
      event_loop.set_timeout(Duration::from_millis(0), move |host, event_loop| {
        host.count += 1;
        if host.count < target {
          schedule(event_loop, target)?;
        }
        Ok(())
      })?;
      Ok(())
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);
    let mut host = TestHost::default();

    // Run a chain of nested 0ms timeouts. Once the timer nesting level is greater than 5, further
    // timers should be clamped to at least 4ms.
    schedule(&mut event_loop, 7)?;

    // The first six timers should run immediately; the seventh should be clamped to 4ms.
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 6);
    assert_eq!(event_loop.timers.len(), 1);
    let due = event_loop
      .timers
      .values()
      .next()
      .expect("expected a pending clamped timer")
      .due;
    assert_eq!(due, Duration::from_millis(4));

    // Without advancing the clock, the clamped timer should not run.
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 6);

    clock.advance(Duration::from_millis(4));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 7);
    assert_eq!(event_loop.timers.len(), 0);
    Ok(())
  }

  #[test]
  fn interval_0ms_is_clamped_once_nesting_level_exceeds_five() -> Result<()> {
    #[derive(Default)]
    struct Host {
      ticks: usize,
      times: Vec<Duration>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.set_interval(Duration::from_millis(0), |host, event_loop| {
      host.ticks += 1;
      host.times.push(event_loop.now());
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 1024,
          max_microtasks: 1024,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );

    // The first six ticks run at the same virtual time; the next is clamped to 4ms.
    assert_eq!(host.times, vec![Duration::from_millis(0); 6]);
    assert_eq!(host.ticks, 6);

    assert_eq!(event_loop.timers.len(), 1);
    let interval = event_loop
      .timers
      .values()
      .next()
      .expect("expected interval to remain scheduled");
    assert_eq!(interval.kind, TimerKind::Interval);
    assert_eq!(interval.due, Duration::from_millis(4));
    assert_eq!(interval.nesting_level, 7);

    // Without advancing virtual time, the clamped interval should not tick again.
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.ticks, 6);

    Ok(())
  }

  #[test]
  fn interval_scheduled_from_nested_timeouts_is_clamped_immediately() -> Result<()> {
    #[derive(Default)]
    struct Host {
      timeouts: usize,
      interval_ticks: usize,
    }

    fn schedule_timeout_chain_then_interval(
      event_loop: &mut EventLoop<Host>,
      target_timeouts: usize,
    ) -> Result<()> {
      event_loop.set_timeout(Duration::from_millis(0), move |host, event_loop| {
        host.timeouts += 1;
        if host.timeouts < target_timeouts {
          schedule_timeout_chain_then_interval(event_loop, target_timeouts)?;
        } else {
          event_loop.set_interval(Duration::from_millis(0), |host, _event_loop| {
            host.interval_ticks += 1;
            Ok(())
          })?;
        }
        Ok(())
      })?;
      Ok(())
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    // The interval is scheduled from within a nested timeout chain at nesting level 6 (>5), so its
    // initial 0ms delay should be clamped to 4ms.
    schedule_timeout_chain_then_interval(&mut event_loop, 6)?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 1024,
          max_microtasks: 1024,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.timeouts, 6);
    assert_eq!(host.interval_ticks, 0);

    assert_eq!(event_loop.timers.len(), 1);
    let interval = event_loop.timers.values().next().unwrap();
    assert_eq!(interval.kind, TimerKind::Interval);
    assert_eq!(interval.due, Duration::from_millis(4));
    assert_eq!(interval.nesting_level, 7);

    // Once the clock reaches the due time, exactly one interval tick should run and reschedule.
    clock.advance(Duration::from_millis(4));
    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 1024,
          max_microtasks: 1024,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.interval_ticks, 1);
    Ok(())
  }

  #[test]
  fn clearing_timers_does_not_leak_timer_queue_entries() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    let mut ids = Vec::new();
    for _ in 0..100 {
      ids.push(event_loop.set_timeout(Duration::from_secs(60), |_host, _event_loop| Ok(()))?);
    }
    for id in ids {
      event_loop.clear_timeout(id);
    }

    assert_eq!(event_loop.timers.len(), 0);
    assert_eq!(event_loop.timer_queue.len(), 100);

    let _ = event_loop.set_timeout(Duration::from_secs(60), |_host, _event_loop| Ok(()))?;
    assert_eq!(event_loop.timers.len(), 1);
    assert_eq!(event_loop.timer_queue.len(), 1);
    Ok(())
  }

  #[test]
  fn timer_due_saturates_at_duration_max_on_overflow() -> Result<()> {
    struct HugeClock;

    impl Clock for HugeClock {
      fn now(&self) -> Duration {
        Duration::MAX.saturating_sub(Duration::from_secs(1))
      }
    }

    let clock: Arc<dyn Clock> = Arc::new(HugeClock);
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    let id = event_loop.set_timeout(Duration::from_secs(10), |_host, _event_loop| Ok(()))?;
    let due = event_loop
      .timers
      .get(&id)
      .expect("timer should be stored")
      .due;
    assert_eq!(due, Duration::MAX);
    Ok(())
  }

  #[test]
  fn interval_reschedule_saturates_at_duration_max_on_overflow() -> Result<()> {
    struct MutableClock {
      now: Mutex<Duration>,
    }

    impl MutableClock {
      fn new(now: Duration) -> Self {
        Self {
          now: Mutex::new(now),
        }
      }

      fn set(&self, now: Duration) {
        *self
          .now
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner()) = now;
      }
    }

    impl Clock for MutableClock {
      fn now(&self) -> Duration {
        *self
          .now
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
      }
    }

    let clock = Arc::new(MutableClock::new(
      Duration::MAX.saturating_sub(Duration::from_secs(1)),
    ));
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);
    let mut host = TestHost::default();

    let id = event_loop.set_interval(Duration::from_secs(1), |host, _event_loop| {
      host.count += 1;
      Ok(())
    })?;
    assert_eq!(event_loop.timers.get(&id).unwrap().due, Duration::MAX);

    clock.set(Duration::MAX);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.count, 1);

    let due = event_loop.timers.get(&id).unwrap().due;
    assert_eq!(due, Duration::MAX);
    Ok(())
  }

  #[test]
  fn set_timeout_callback_invoked_twice_returns_error_not_panic() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);

    let id = event_loop.set_timeout(Duration::from_millis(0), |host, _event_loop| {
      host.count += 1;
      Ok(())
    })?;

    // Simulate an internal bug by invoking the timer callback twice directly.
    let mut callback = event_loop
      .timers
      .get_mut(&id)
      .and_then(|timer| timer.callback.take())
      .expect("timer callback should exist");

    callback(&mut host, &mut event_loop)?;
    assert_eq!(host.count, 1);

    let err = callback(&mut host, &mut event_loop).expect_err("second invocation should fail");
    assert!(
      matches!(err, Error::Other(msg) if msg.contains("setTimeout callback invoked more than once"))
    );
    Ok(())
  }

  #[test]
  fn interval_cleared_after_first_firing_but_before_queued_second_firing_cancels_second(
  ) -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    let id = event_loop.set_interval(Duration::from_millis(5), |host, _event_loop| {
      host.count += 1;
      Ok(())
    })?;

    // Run the first firing.
    clock.advance(Duration::from_millis(5));
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.count, 1);

    // Enqueue the second firing, then clear before executing it.
    clock.advance(Duration::from_millis(5));
    event_loop.queue_due_timers()?;
    event_loop.clear_interval(id);

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 1);
    Ok(())
  }

  #[test]
  fn reused_timer_id_does_not_run_stale_enqueued_task() -> Result<()> {
    let mut host = TestHost::default();
    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock_for_loop);

    let id = event_loop.set_timeout(Duration::from_millis(0), |_host, _event_loop| Ok(()))?;
    event_loop.queue_due_timers()?;
    event_loop.clear_timeout(id);

    // Force ID reuse by rewinding the internal counter (mirrors the HTML model where IDs can be
    // reused once cleared).
    event_loop.next_timer_id = id;
    let _new_id = event_loop.set_timeout(Duration::from_millis(5), |host, _event_loop| {
      host.count += 1;
      Ok(())
    })?;

    // The stale task enqueued for the old timer must not run the new timer early.
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 0);

    // Once time advances enough for the new timer, it should fire normally.
    clock.advance(Duration::from_millis(5));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.count, 1);
    Ok(())
  }

  #[test]
  fn idle_callbacks_share_a_single_idle_period_deadline() -> Result<()> {
    #[derive(Default)]
    struct Host {
      remaining: Vec<f64>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let clock_for_first = clock.clone();
    event_loop.request_idle_callback(None, move |host, _event_loop, did_timeout, remaining_ms| {
      assert!(!did_timeout);
      host.remaining.push(remaining_ms);
      // Simulate time passing during the idle period so subsequent callbacks see less time.
      clock_for_first.advance(Duration::from_millis(10));
      Ok(())
    })?;

    event_loop.request_idle_callback(None, |host, _event_loop, did_timeout, remaining_ms| {
      assert!(!did_timeout);
      host.remaining.push(remaining_ms);
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(host.remaining.len(), 2);
    assert!(
      host.remaining[1] <= host.remaining[0],
      "expected idle callback budget to be non-increasing within an idle period; first={}ms second={}ms",
      host.remaining[0],
      host.remaining[1]
    );
    Ok(())
  }

  #[test]
  fn idle_period_deadline_persists_across_microtasks_queued_by_idle_callbacks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      remaining: Vec<f64>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let clock_for_microtasks = clock.clone();
    event_loop.request_idle_callback(None, move |host, event_loop, did_timeout, remaining_ms| {
      assert!(!did_timeout);
      host.remaining.push(remaining_ms);

      // Queue a microtask which itself queues another microtask. This exercises that microtasks
      // running as part of an idle callback's microtask checkpoint do not reset the shared idle
      // period deadline.
      let clock_for_microtasks = clock_for_microtasks.clone();
      event_loop.queue_microtask(move |_host, event_loop| {
        clock_for_microtasks.advance(Duration::from_millis(10));
        event_loop.queue_microtask(|_host, _event_loop| Ok(()))?;
        Ok(())
      })?;

      Ok(())
    })?;

    event_loop.request_idle_callback(None, |host, _event_loop, did_timeout, remaining_ms| {
      assert!(!did_timeout);
      host.remaining.push(remaining_ms);
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(host.remaining.len(), 2);
    assert!(
      host.remaining[1] < host.remaining[0],
      "expected timeRemaining() to shrink across an idle period even when microtasks run between callbacks; first={}ms second={}ms",
      host.remaining[0],
      host.remaining[1]
    );
    Ok(())
  }

  #[test]
  fn animation_frame_turn_clears_idle_period_deadline() -> Result<()> {
    #[derive(Default)]
    struct Host {
      raf_ran: bool,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    // Keep a rAF callback pending so the idle period deadline is clamped by a rendering budget.
    event_loop.request_animation_frame(|host, _event_loop, _timestamp| {
      host.raf_ran = true;
      Ok(())
    })?;

    event_loop.request_idle_callback(None, |_host, _event_loop, did_timeout, _remaining_ms| {
      assert!(!did_timeout);
      Ok(())
    })?;

    let mut host = Host::default();
    assert!(event_loop.run_next_task(&mut host)?, "expected idle callback task to run");
    assert!(
      event_loop.idle_period_deadline.is_some(),
      "expected idle callback to initialize an idle period deadline"
    );

    event_loop.run_animation_frame(&mut host)?;
    assert!(host.raf_ran, "expected rAF callback to run");
    assert!(
      event_loop.idle_period_deadline.is_none(),
      "animation frame turns should end an idle period and clear its deadline"
    );
    Ok(())
  }

  #[test]
  fn idle_callbacks_do_not_run_past_the_idle_period_deadline() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let clock_for_first = clock.clone();
    event_loop.request_idle_callback(None, move |host, _event_loop, did_timeout, _remaining_ms| {
      assert!(!did_timeout);
      host.log.push("a");
      // Advance beyond the default idle budget (50ms) so the idle period expires.
      clock_for_first.advance(Duration::from_millis(60));
      Ok(())
    })?;

    event_loop.request_idle_callback(None, |host, _event_loop, did_timeout, _remaining_ms| {
      assert!(!did_timeout);
      host.log.push("b");
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      host.log,
      vec!["a"],
      "second idle callback should not run in the same idle period once the deadline is exceeded"
    );
    assert_eq!(event_loop.idle_callbacks.len(), 1);
    assert_eq!(event_loop.idle_callback_queue.len(), 1);

    // A subsequent run can start a new idle period and deliver the remaining callback.
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["a", "b"]);
    Ok(())
  }

  #[test]
  fn deadline_defaults_to_script_stage_when_no_active_stage() {
    let cancel = Arc::new(|| true);
    let deadline = RenderDeadline::new(None, Some(cancel));
    let _stage_guard = render_control::StageGuard::install(None);

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);
    let mut host = TestHost::default();

    let err = render_control::with_deadline(Some(&deadline), || {
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())
    })
    .expect_err("expected deadline to abort execution");

    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Script),
      other => panic!("expected render timeout error, got {other:?}"),
    }
  }

  #[test]
  fn deadline_attribution_respects_existing_stage_guard() {
    let cancel = Arc::new(|| true);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<TestHost>::with_clock(clock);
    let mut host = TestHost::default();

    let err = render_control::with_deadline(Some(&deadline), || {
      let _stage_guard = render_control::StageGuard::install(Some(RenderStage::Layout));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())
    })
    .expect_err("expected deadline to abort execution");

    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Layout),
      other => panic!("expected render timeout error, got {other:?}"),
    }
  }

  #[test]
  fn queue_limits_reject_tasks_microtasks_and_timers_when_exceeded() {
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 1,
      max_pending_microtasks: 1,
      max_pending_timers: 1,
      max_pending_animation_frame_callbacks: 1,
      max_pending_idle_callbacks: 1,
    });

    event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))
      .unwrap();
    let err = event_loop
      .queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))
      .expect_err("expected task queue limit error");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending tasks")),
      "unexpected error: {err:?}"
    );

    event_loop
      .queue_microtask(|_host, _event_loop| Ok(()))
      .unwrap();
    let err = event_loop
      .queue_microtask(|_host, _event_loop| Ok(()))
      .expect_err("expected microtask queue limit error");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending microtasks")),
      "unexpected error: {err:?}"
    );

    let _timer = event_loop
      .set_timeout(Duration::from_millis(0), |_host, _event_loop| Ok(()))
      .unwrap();
    let err = event_loop
      .set_timeout(Duration::from_millis(0), |_host, _event_loop| Ok(()))
      .expect_err("expected timer queue limit error");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending timers")),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn drain_external_tasks_failure_does_not_drop_remaining_tasks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    // Queue multiple tasks into the external thread-safe buffer before shrinking the task queue
    // limit (the external buffer shares the same limit).
    let handle = event_loop.external_task_queue_handle();
    handle.queue_task(TaskSource::Networking, |host, _event_loop| {
      host.log.push("a");
      Ok(())
    })?;
    handle.queue_task(TaskSource::Networking, |host, _event_loop| {
      host.log.push("b");
      Ok(())
    })?;
    handle.queue_task(TaskSource::Networking, |host, _event_loop| {
      host.log.push("c");
      Ok(())
    })?;

    // Limit the main task queue to 1 so draining will fail after the first task is promoted.
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 1,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });

    let err = event_loop
      .drain_external_tasks()
      .expect_err("expected external task drain to fail due to queue limit");
    assert!(
      matches!(err, Error::Other(ref msg) if msg.contains("max pending tasks")),
      "unexpected error: {err:?}"
    );

    // One task should have been queued; the others must remain in the external buffer.
    assert_eq!(event_loop.pending_task_count(), 1);
    {
      let lock = handle.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      assert_eq!(lock.queue.len(), 2, "remaining external tasks should not be lost");
    }

    // Increase capacity and drain again; all tasks should now run.
    event_loop.set_queue_limits(QueueLimits {
      max_pending_tasks: 10,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
      max_pending_animation_frame_callbacks: usize::MAX,
      max_pending_idle_callbacks: usize::MAX,
    });
    event_loop.drain_external_tasks()?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["a", "b", "c"]);
    Ok(())
  }

  #[test]
  fn idle_callback_budget_is_clamped_when_animation_frame_callbacks_pending() -> Result<()> {
    #[derive(Default)]
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    // Non-empty animation frame callback map is considered pending rendering work by HTML's idle
    // period deadline computation. FastRender approximates this by clamping the idle budget to a
    // nominal frame interval.
    event_loop.request_animation_frame(|_host, _event_loop, _ts| Ok(()))?;
    assert!(event_loop.has_pending_animation_frame_callbacks());

    let remaining_ms: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
    let remaining_ms_for_cb = Rc::clone(&remaining_ms);
    event_loop.request_idle_callback(None, move |_host, _event_loop, did_timeout, remaining_ms| {
      assert!(!did_timeout);
      remaining_ms_for_cb.set(Some(remaining_ms));
      Ok(())
    })?;

    let mut host = Host::default();
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let observed = remaining_ms.get().expect("idle callback should have run");
    let frame_budget_ms = duration_to_ms_f64(DEFAULT_FRAME_BUDGET);
    assert!(
      observed <= frame_budget_ms,
      "expected idle callback remaining budget to be <= {frame_budget_ms}ms when requestAnimationFrame callbacks are pending, got {observed}ms"
    );
    Ok(())
  }

  #[test]
  fn user_interaction_external_tasks_are_drained_and_run() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let handle = event_loop.external_task_queue_handle();

    handle.queue_task(TaskSource::UserInteraction, |host, _event_loop| {
      host.log.push("ui");
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.log, vec!["ui"]);
    Ok(())
  }

  #[test]
  fn animation_frame_callbacks_are_ordered_and_snapshotted() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.request_animation_frame(|host, event_loop, _ts| {
      host.log.push("a");
      event_loop.request_animation_frame(|host, _event_loop, _ts| {
        host.log.push("a2");
        Ok(())
      })?;
      Ok(())
    })?;

    event_loop.request_animation_frame(|host, event_loop, _ts| {
      host.log.push("b");
      event_loop.request_animation_frame(|host, _event_loop, _ts| {
        host.log.push("b2");
        Ok(())
      })?;
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );
    assert_eq!(host.log, vec!["a", "b"]);

    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );
    assert_eq!(host.log, vec!["a", "b", "a2", "b2"]);

    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Idle
    );
    Ok(())
  }

  #[test]
  fn canceled_animation_frame_callbacks_do_not_accumulate_in_queue() -> Result<()> {
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    event_loop.set_queue_limits(QueueLimits {
      max_pending_animation_frame_callbacks: 4,
      ..QueueLimits::unbounded()
    });

    // Keep one callback live so `cancel_animation_frame` does not clear the entire queue.
    let _keep_alive = event_loop.request_animation_frame(|_host, _event_loop, _ts| Ok(()))?;

    for _ in 0..1024 {
      let id = event_loop.request_animation_frame(|_host, _event_loop, _ts| Ok(()))?;
      event_loop.cancel_animation_frame(id);
    }

    // The scheduling queue should not grow linearly with cancellations.
    assert_eq!(event_loop.animation_frame_callbacks.len(), 1);
    assert!(
      event_loop.animation_frame_queue.len() <= 8,
      "animation_frame_queue grew too large (len={})",
      event_loop.animation_frame_queue.len()
    );
    Ok(())
  }

  #[test]
  fn animation_frame_callback_errors_abort_without_error_handler() {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop
      .request_animation_frame(|host, _event_loop, _ts| {
        host.log.push("a");
        Err(Error::Other("boom".to_string()))
      })
      .unwrap();
    event_loop
      .request_animation_frame(|host, _event_loop, _ts| {
        host.log.push("b");
        Ok(())
      })
      .unwrap();

    let mut host = Host::default();
    let err = event_loop
      .run_animation_frame(&mut host)
      .expect_err("expected animation frame error");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(host.log, vec!["a"]);
    assert_eq!(event_loop.currently_running_task(), None);

    // The remaining callback should still be queued for the next frame (no leak).
    assert_eq!(
      event_loop.run_animation_frame(&mut host).unwrap(),
      RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );
    assert_eq!(host.log, vec!["a", "b"]);
  }

  #[test]
  fn animation_frame_deadline_abort_preserves_remaining_callbacks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("a");
      Ok(())
    })?;
    event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("b");
      Ok(())
    })?;

    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_cancel = Arc::clone(&checks);
    let deadline = RenderDeadline::new(None, Some(Arc::new(move || {
      // `run_animation_frame` checks once before starting and again before each callback.
      // Trip on the second check to abort before any callbacks are popped/executed.
      checks_for_cancel.fetch_add(1, Ordering::SeqCst) >= 1
    })));

    let mut host = Host::default();
    {
      let _guard = render_control::DeadlineGuard::install(Some(&deadline));
      let err = event_loop
        .run_animation_frame(&mut host)
        .expect_err("expected deadline timeout");
      match err {
        Error::Render(RenderError::Timeout { stage, .. }) => {
          assert_eq!(stage, RenderStage::Script);
        }
        err => panic!("expected RenderError::Timeout, got {err:?}"),
      }
    }

    assert_eq!(host.log, Vec::<&'static str>::new());
    assert_eq!(event_loop.currently_running_task(), None);
    assert_eq!(event_loop.animation_frame_callbacks.len(), 2);
    assert_eq!(event_loop.animation_frame_queue.len(), 2);

    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );
    assert_eq!(host.log, vec!["a", "b"]);
    Ok(())
  }

  #[test]
  fn animation_frame_callback_errors_are_reported_and_do_not_abort_frame() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("a");
      Err(Error::Other("boom".to_string()))
    })?;
    event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("b");
      Ok(())
    })?;

    let mut host = Host::default();
    let mut errors: Vec<String> = Vec::new();
    assert_eq!(
      event_loop.run_animation_frame_handling_errors(&mut host, |err| {
        errors.push(err.to_string());
      })?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );
    assert_eq!(host.log, vec!["a", "b"]);
    assert_eq!(errors, vec!["[other] boom".to_string()]);
    assert_eq!(event_loop.currently_running_task(), None);

    // Both callbacks were drained, so a second frame should be idle.
    assert_eq!(
      event_loop.run_animation_frame_handling_errors(&mut host, |_| {})?,
      RunAnimationFrameOutcome::Idle
    );
    Ok(())
  }

  #[test]
  fn cancel_animation_frame_before_run_cancels_callback() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let a = event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("a");
      Ok(())
    })?;
    let _b = event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("b");
      Ok(())
    })?;
    event_loop.cancel_animation_frame(a);

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );
    assert_eq!(host.log, vec!["b"]);
    Ok(())
  }

  #[test]
  fn cancel_animation_frame_inside_other_callback_prevents_invocation() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let id_to_cancel: Rc<Cell<Option<AnimationFrameId>>> = Rc::new(Cell::new(None));
    let id_to_cancel_for_cb = Rc::clone(&id_to_cancel);

    event_loop.request_animation_frame(move |host, event_loop, _ts| {
      host.log.push("a");
      let id = id_to_cancel_for_cb
        .get()
        .expect("expected animation frame id to be set");
      event_loop.cancel_animation_frame(id);
      Ok(())
    })?;

    let b = event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("b");
      Ok(())
    })?;
    id_to_cancel.set(Some(b));

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );
    assert_eq!(host.log, vec!["a"]);
    Ok(())
  }

  #[test]
  fn request_animation_frame_compacts_stale_queue_entries_from_canceled_handles() -> Result<()> {
    #[derive(Default)]
    struct Host;

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock_and_queue_limits(
      clock_for_loop,
      QueueLimits {
        max_pending_animation_frame_callbacks: 2,
        ..QueueLimits::unbounded()
      },
    );

    // Keep one callback live so the animation frame queue isn't cleared wholesale.
    let _persistent = event_loop.request_animation_frame(|_host, _event_loop, _ts| Ok(()))?;

    // Simulate an attacker pattern: repeatedly schedule+cancel callbacks, leaving stale IDs behind.
    for _ in 0..50 {
      let id = event_loop.request_animation_frame(|_host, _event_loop, _ts| Ok(()))?;
      event_loop.cancel_animation_frame(id);
      assert!(
        event_loop.animation_frame_queue.len() <= 3,
        "expected rAF queue to stay bounded, got len={}",
        event_loop.animation_frame_queue.len()
      );
    }

    Ok(())
  }

  #[test]
  fn animation_frame_timestamp_is_stable_within_frame() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<f64>,
    }

    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(10));
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    let clock_for_cb = Arc::clone(&clock);
    event_loop.request_animation_frame(move |host, _event_loop, ts| {
      host.observed.push(ts);
      clock_for_cb.advance(Duration::from_millis(5));
      Ok(())
    })?;

    event_loop.request_animation_frame(|host, _event_loop, ts| {
      host.observed.push(ts);
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );

    assert_eq!(host.observed, vec![10.0, 10.0]);
    Ok(())
  }

  #[test]
  fn animation_frame_performs_microtask_checkpoint_after_frame() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    event_loop.request_animation_frame(|host, event_loop, _ts| {
      host.log.push("raf1");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("microtask");
        Ok(())
      })?;
      host.log.push("raf1_end");
      Ok(())
    })?;

    event_loop.request_animation_frame(|host, _event_loop, _ts| {
      host.log.push("raf2");
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 2 }
    );
    // The microtask queued by the first callback must not run until after the second callback.
    assert_eq!(host.log, vec!["raf1", "raf1_end", "raf2", "microtask"]);
    Ok(())
  }

  #[test]
  fn animation_frame_microtask_checkpoint_runs_even_when_no_callbacks() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    let mut event_loop = EventLoop::<Host>::new();
    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Idle
    );
    assert_eq!(host.log, vec!["microtask"]);
    Ok(())
  }

  #[test]
  fn animation_frame_timestamp_resets_on_navigation() -> Result<()> {
    #[derive(Default)]
    struct Host {
      observed: Vec<f64>,
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);

    clock.advance(Duration::from_millis(100));
    event_loop.request_animation_frame(|host, _event_loop, ts| {
      host.observed.push(ts);
      Ok(())
    })?;

    let mut host = Host::default();
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );
    assert_eq!(host.observed, vec![100.0]);

    clock.advance(Duration::from_millis(100));
    event_loop.reset_for_navigation(TraceHandle::default(), event_loop.queue_limits());

    event_loop.request_animation_frame(|host, _event_loop, ts| {
      host.observed.push(ts);
      Ok(())
    })?;
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );

    assert_eq!(host.observed, vec![100.0, 0.0]);
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_hooks_run_in_registration_order_per_checkpoint() -> Result<()> {
    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
    }

    fn hook1(host: &mut Host, _event_loop: &mut EventLoop<Host>) -> Result<()> {
      host.log.push("hook1");
      Ok(())
    }

    fn hook2(host: &mut Host, _event_loop: &mut EventLoop<Host>) -> Result<()> {
      host.log.push("hook2");
      Ok(())
    }

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    event_loop.register_microtask_checkpoint_hook(hook1)?;
    event_loop.register_microtask_checkpoint_hook(hook2)?;

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask");
      Ok(())
    })?;

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task");
      event_loop.queue_microtask(|host, _event_loop| {
        host.log.push("nested_microtask");
        Ok(())
      })?;
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      host.log,
      vec![
        "microtask",
        "hook1",
        "hook2",
        "task",
        "nested_microtask",
        "hook1",
        "hook2"
      ]
    );
    Ok(())
  }

  mod js_event_loop_timers {
    use super::*;

    #[derive(Default)]
    struct Host {
      log: Vec<&'static str>,
      ticks: usize,
      interval_id: Option<TimerId>,
      times: Vec<Duration>,
    }

    #[test]
    fn pending_timer_state_is_exposed_for_host_scheduling() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

      assert!(!event_loop.has_pending_timers());
      assert_eq!(event_loop.pending_timer_count(), 0);
      assert_eq!(event_loop.duration_until_next_timer(), None);

      event_loop.set_timeout(Duration::from_millis(10), |host, _| {
        host.ticks += 1;
        Ok(())
      })?;

      // The event loop is still "idle" (no runnable work queued), but it is not quiescent: a future
      // timer is scheduled.
      assert!(event_loop.is_idle());
      assert!(event_loop.has_pending_timers());
      assert_eq!(event_loop.pending_timer_count(), 1);
      assert_eq!(
        event_loop.duration_until_next_timer(),
        Some(Duration::from_millis(10))
      );

      clock.advance(Duration::from_millis(10));
      let mut host = Host::default();
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.ticks, 1);
      assert!(!event_loop.has_pending_timers());
      assert_eq!(event_loop.pending_timer_count(), 0);
      Ok(())
    }

    #[test]
    fn set_timeout_orders_by_due_time_then_registration_order() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

      event_loop.set_timeout(Duration::from_millis(10), |host, _| {
        host.log.push("t10");
        Ok(())
      })?;
      event_loop.set_timeout(Duration::from_millis(5), |host, _| {
        host.log.push("t5a");
        Ok(())
      })?;
      event_loop.set_timeout(Duration::from_millis(5), |host, _| {
        host.log.push("t5b");
        Ok(())
      })?;

      let mut host = Host::default();
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert!(host.log.is_empty());

      clock.advance(Duration::from_millis(5));
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.log, vec!["t5a", "t5b"]);

      clock.advance(Duration::from_millis(5));
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.log, vec!["t5a", "t5b", "t10"]);
      Ok(())
    }

    #[test]
    fn set_interval_repeats_until_cleared() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

      let id = event_loop.set_interval(Duration::from_millis(10), |host, event_loop| {
        host.ticks += 1;
        host.log.push("tick");
        if host.ticks == 3 {
          event_loop.clear_interval(host.interval_id.expect("interval id should be set"));
        }
        Ok(())
      })?;

      let mut host = Host::default();
      host.interval_id = Some(id);

      // Nothing due yet.
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.ticks, 0);

      clock.advance(Duration::from_millis(10));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
      clock.advance(Duration::from_millis(10));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
      clock.advance(Duration::from_millis(10));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

      // Cleared on the third tick.
      clock.advance(Duration::from_millis(10));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

      assert_eq!(host.ticks, 3);
      assert_eq!(host.log, vec!["tick", "tick", "tick"]);
      Ok(())
    }

    #[test]
    fn microtasks_queued_by_timer_run_before_next_task() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock);

      event_loop.set_timeout(Duration::from_millis(0), |host, event_loop| {
        host.log.push("timer");
        event_loop.queue_microtask(|host, _| {
          host.log.push("microtask");
          Ok(())
        })?;
        event_loop.queue_task(TaskSource::Script, |host, _| {
          host.log.push("task");
          Ok(())
        })?;
        Ok(())
      })?;

      let mut host = Host::default();
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.log, vec!["timer", "microtask", "task"]);
      Ok(())
    }

    #[test]
    fn runaway_timers_stop_at_max_tasks_limit_deterministically() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock);

      // 0ms interval: immediately re-queues itself at the same virtual time.
      event_loop.set_interval(Duration::from_millis(0), |host, _| {
        host.ticks += 1;
        Ok(())
      })?;

      let mut host = Host::default();
      let outcome = event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 3,
          max_microtasks: 100,
          max_wall_time: None,
        },
      )?;

      assert_eq!(
        outcome,
        RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks {
          executed: 3,
          limit: 3
        })
      );
      assert_eq!(host.ticks, 3);
      Ok(())
    }

    fn schedule_nested_timeout(event_loop: &mut EventLoop<Host>, target: usize) -> Result<()> {
      event_loop.set_timeout(Duration::from_millis(0), move |host, event_loop| {
        host.ticks += 1;
        host.times.push(event_loop.now());
        if host.ticks < target {
          schedule_nested_timeout(event_loop, target)?;
        }
        Ok(())
      })?;
      Ok(())
    }

    #[test]
    fn nested_timeout_delay_clamps_when_nesting_level_exceeds_five() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

      // HTML timer clamping: once the timer nesting level is greater than 5, subsequent 0ms timers
      // should be clamped to 4ms (virtual time doesn't advance unless the host moves it forward).
      schedule_nested_timeout(&mut event_loop, 9)?;

      let mut host = Host::default();
      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.times, vec![Duration::from_millis(0); 6]);

      clock.advance(Duration::from_millis(4));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
      assert_eq!(
        host.times,
        vec![
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(4),
        ]
      );

      clock.advance(Duration::from_millis(4));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
      assert_eq!(
        host.times,
        vec![
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(4),
          Duration::from_millis(8),
        ]
      );

      clock.advance(Duration::from_millis(4));
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
      assert_eq!(
        host.times,
        vec![
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(0),
          Duration::from_millis(4),
          Duration::from_millis(8),
          Duration::from_millis(12),
        ]
      );

      Ok(())
    }
  }
}
