use crate::error::{Error, RenderStage, Result};
use crate::debug::trace::TraceHandle;
use crate::render_control::{self, record_stage, StageGuard, StageHeartbeat};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use super::clock::{Clock, RealClock};

/// HTML task sources (WHATWG terminology).
///
/// This enum is intentionally small for now, but designed to be extended as more
/// web APIs are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskSource {
  Script,
  Microtask,
  Networking,
  DOMManipulation,
  Timer,
}

fn task_source_name(source: TaskSource) -> &'static str {
  match source {
    TaskSource::Script => "Script",
    TaskSource::Microtask => "Microtask",
    TaskSource::Networking => "Networking",
    TaskSource::DOMManipulation => "DOMManipulation",
    TaskSource::Timer => "Timer",
  }
}

type Runnable<Host> = Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

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
}

impl QueueLimits {
  pub fn unbounded() -> Self {
    Self {
      max_pending_tasks: usize::MAX,
      max_pending_microtasks: usize::MAX,
      max_pending_timers: usize::MAX,
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
pub enum SpinOutcome {
  ConditionMet,
  Idle,
  Stopped(RunUntilIdleStopReason),
}

/// JS-visible timer ID returned by `setTimeout`/`setInterval`.
///
/// The HTML Standard uses integer handles for timers; we use `i32` so this can be exposed to JS
/// without lossy conversions.
pub type TimerId = i32;

type TimerCallback<Host> = Box<dyn FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

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

pub struct EventLoop<Host: 'static> {
  clock: Arc<dyn Clock>,
  default_deadline_stage: RenderStage,
  queue_limits: QueueLimits,
  trace: TraceHandle,
  task_queues: BTreeMap<TaskSource, VecDeque<Task<Host>>>,
  microtask_queue: VecDeque<Task<Host>>,
  next_task_seq: u64,
  timers: HashMap<TimerId, TimerState<Host>>,
  timer_queue: BinaryHeap<Reverse<(Duration, u64, TimerId)>>,
  next_timer_id: TimerId,
  next_timer_seq: u64,
  timer_nesting_level: u32,
  performing_microtask_checkpoint: bool,
  currently_running_task: Option<RunningTask>,
}

impl<Host: 'static> Default for EventLoop<Host> {
  fn default() -> Self {
    Self {
      clock: Arc::new(RealClock::default()),
      default_deadline_stage: RenderStage::Script,
      queue_limits: QueueLimits::default(),
      trace: TraceHandle::default(),
      task_queues: BTreeMap::new(),
      microtask_queue: VecDeque::new(),
      next_task_seq: 0,
      timers: HashMap::new(),
      timer_queue: BinaryHeap::new(),
      next_timer_id: 1,
      next_timer_seq: 0,
      timer_nesting_level: 0,
      performing_microtask_checkpoint: false,
      currently_running_task: None,
    }
  }
}

impl<Host: 'static> EventLoop<Host> {
  pub fn new() -> Self {
    Self::default()
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
    Self {
      clock,
      ..Self::default()
    }
  }

  pub fn with_clock_and_queue_limits(clock: Arc<dyn Clock>, queue_limits: QueueLimits) -> Self {
    Self {
      clock,
      queue_limits,
      ..Self::default()
    }
  }

  pub fn now(&self) -> Duration {
    self.clock.now()
  }

  pub fn queue_limits(&self) -> QueueLimits {
    self.queue_limits
  }

  pub fn set_queue_limits(&mut self, limits: QueueLimits) {
    self.queue_limits = limits;
  }

  pub fn currently_running_task(&self) -> Option<RunningTask> {
    self.currently_running_task
  }

  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    if self.pending_task_count() >= self.queue_limits.max_pending_tasks {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending tasks (limit={})",
        self.queue_limits.max_pending_tasks
      )));
    }
    let seq = self.next_task_seq;
    self.next_task_seq = self.next_task_seq.wrapping_add(1);
    let queue = self.task_queues.entry(source).or_default();
    queue
      .try_reserve(1)
      .map_err(|err| Error::Other(format!("EventLoop task queue allocation failed: {err}")))?;
    queue.push_back(Task::new_with_seq(source, seq, runnable));
    Ok(())
  }

  pub fn queue_microtask<F>(&mut self, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
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

  /// Perform a microtask checkpoint (HTML Standard terminology).
  ///
  /// - If a checkpoint is already in progress, this is a no-op (reentrancy guard).
  /// - Otherwise, drains the microtask queue until it becomes empty.
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
    let result = (|| {
      while !self.microtask_queue.is_empty() {
        let Some(task) = self.microtask_queue.pop_front() else {
          // The emptiness check above and the `VecDeque` API guarantee this can't happen, but avoid
          // panicking if an invariant is ever violated.
          break;
        };
        self.currently_running_task = Some(RunningTask {
          source: task.source,
          is_microtask: true,
        });
        task.run(host, self)?;
        drained = drained.saturating_add(1);
      }
      Ok(())
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

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    // Always clear running-task state so errors don't leave the event loop in a "running" state.
    self.currently_running_task = None;
    let task_result = task_result.and_then(|()| self.perform_microtask_checkpoint(host));
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    task_result?;
    Ok(true)
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

  fn run_until_idle_inner(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> RunStepResult<RunUntilIdleOutcome> {
    loop {
      run_state.check_deadline()?;
      self.queue_due_timers().map_err(RunStepError::Error)?;

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_limited(host, run_state)?;
        continue;
      }

      if self.run_next_task_limited(host, run_state)? {
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

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_limited_handling_errors(host, run_state, on_error)?;
        continue;
      }

      if self.run_next_task_limited_handling_errors(host, run_state, on_error)? {
        continue;
      }

      return Ok(RunUntilIdleOutcome::Idle);
    }
  }

  fn run_next_task_limited(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
  ) -> RunStepResult<bool> {
    self.queue_due_timers().map_err(RunStepError::Error)?;

    let Some(task) = self.pop_next_task() else {
      return Ok(false);
    };

    run_state.check_deadline()?;
    run_state.before_task()?;

    let mut trace_span = self.trace.span("js.task.run", "js");
    trace_span.arg_str("source", task_source_name(task.source));
    trace_span.arg_u64("seq", task.seq);

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    let previous_running_task = self.currently_running_task;
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    self.currently_running_task = None;
    let task_result = task_result
      .map_err(RunStepError::Error)
      .and_then(|()| self.perform_microtask_checkpoint_limited(host, run_state));
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    task_result?;
    Ok(true)
  }

  fn run_next_task_limited_handling_errors<F>(
    &mut self,
    host: &mut Host,
    run_state: &mut RunState,
    on_error: &mut F,
  ) -> RunStepResult<bool>
  where
    F: FnMut(Error),
  {
    self.queue_due_timers().map_err(RunStepError::Error)?;

    let Some(task) = self.pop_next_task() else {
      return Ok(false);
    };

    run_state.check_deadline()?;
    run_state.before_task()?;

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

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
      self.perform_microtask_checkpoint_limited_handling_errors(host, run_state, on_error);
    self.timer_nesting_level = previous_timer_nesting_level;
    self.currently_running_task = previous_running_task;
    microtask_result?;
    Ok(true)
  }

  fn perform_microtask_checkpoint_limited(
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
        task.run(host, self).map_err(RunStepError::Error)?;
        drained = drained.saturating_add(1);
      }
      Ok(())
    })();

    trace_span.arg_u64("drained", drained);
    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  fn perform_microtask_checkpoint_limited_handling_errors<F>(
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
      Ok(())
    })();

    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  fn pending_task_count(&self) -> usize {
    self.task_queues.values().map(VecDeque::len).sum()
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
    let should_compact = heap_len > self.queue_limits.max_pending_timers
      || heap_len > live.saturating_mul(2).max(64);
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
    let mut chosen_source: Option<TaskSource> = None;
    let mut chosen_seq: u64 = u64::MAX;
    for (source, queue) in &self.task_queues {
      if let Some(task) = queue.front() {
        if task.seq < chosen_seq {
          chosen_seq = task.seq;
          chosen_source = Some(*source);
        }
      }
    }
    let source = chosen_source?;
    let (task, empty) = match self.task_queues.get_mut(&source) {
      Some(queue) => {
        let task = queue.pop_front();
        let empty = queue.is_empty();
        (task, empty)
      }
      None => {
        debug_assert!(false, "task queue missing for selected source");
        return None;
      }
    };
    if empty {
      self.task_queues.remove(&source);
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
    let due = self.clock.now() + delay;

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
        nesting_level: self.timer_nesting_level,
      },
    );
    self.timer_queue.push(Reverse((due, schedule_seq, id)));
    Ok(id)
  }

  fn clamp_timer_delay(&self, requested: Duration) -> Duration {
    const MIN_NESTED_DELAY: Duration = Duration::from_millis(4);
    if self.timer_nesting_level > 5 {
      requested.max(MIN_NESTED_DELAY)
    } else {
      requested
    }
  }

  fn queue_due_timers(&mut self) -> Result<()> {
    let now = self.clock.now();
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      if due > now {
        break;
      }
      let _ = self.timer_queue.pop();

      let Some(timer) = self.timers.get(&id) else {
        continue;
      };
      if timer.schedule_seq != schedule_seq {
        continue;
      }

      self.queue_task(TaskSource::Timer, move |host, event_loop| {
        // HTML timers validate the global ID→uniqueHandle map at *task execution time* so that
        // `clearTimeout`/`clearInterval` (and potential ID reuse) can cancel already-queued tasks.
        event_loop.fire_timer(host, id, schedule_seq)
      })?;
    }
    Ok(())
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
    self.timer_nesting_level = (nesting_level + 1).min(6);

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
        let due = now + delay;

        let nesting_level = self.timer_nesting_level;
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
        timer.nesting_level = nesting_level;
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
        self.perform_microtask_checkpoint_limited(host, run_state)?;
        continue;
      }

      if self.run_next_task_limited(host, run_state)? {
        continue;
      }

      return Ok(SpinOutcome::Idle);
    }
  }
}

struct RunState {
  limits: RunLimits,
  clock: Arc<dyn Clock>,
  started_at: Duration,
  default_deadline_stage: RenderStage,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl RunState {
  fn new(limits: RunLimits, clock: Arc<dyn Clock>, default_deadline_stage: RenderStage) -> Self {
    Self {
      limits,
      started_at: clock.now(),
      clock,
      default_deadline_stage,
      tasks_executed: 0,
      microtasks_executed: 0,
    }
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
  use std::sync::Mutex;

  #[derive(Default)]
  struct TestHost {
    log: Vec<&'static str>,
    count: usize,
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
      stages.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
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
      stages.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
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
}
