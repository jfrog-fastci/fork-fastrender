use crate::error::{Error, RenderStage, Result};
use crate::render_control;
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

type Runnable<Host> =
  Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(u64);

type TimerCallback<Host> =
  Box<dyn FnMut(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

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
  queue_limits: QueueLimits,
  task_queues: BTreeMap<TaskSource, VecDeque<Task<Host>>>,
  microtask_queue: VecDeque<Task<Host>>,
  next_task_seq: u64,
  timers: HashMap<TimerId, TimerState<Host>>,
  timer_queue: BinaryHeap<Reverse<(Duration, u64, TimerId)>>,
  next_timer_id: u64,
  next_timer_seq: u64,
  timer_nesting_level: u32,
  performing_microtask_checkpoint: bool,
  currently_running_task: Option<RunningTask>,
}

impl<Host: 'static> Default for EventLoop<Host> {
  fn default() -> Self {
    Self {
      clock: Arc::new(RealClock::default()),
      queue_limits: QueueLimits::default(),
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
    self
      .task_queues
      .entry(source)
      .or_default()
      .push_back(Task::new_with_seq(source, seq, runnable));
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
    self
      .microtask_queue
      .push_back(Task::new_with_seq(TaskSource::Microtask, seq, runnable));
    Ok(())
  }

  pub fn set_timeout<F>(&mut self, delay: Duration, callback: F) -> Result<TimerId>
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    let mut maybe = Some(Box::new(callback) as Runnable<Host>);
    let callback: TimerCallback<Host> = Box::new(move |host, event_loop| {
      let runnable = maybe
        .take()
        .expect("setTimeout callback invoked more than once");
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

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let result = (|| {
      while !self.microtask_queue.is_empty() {
        let task = self
          .microtask_queue
          .pop_front()
          .expect("microtask queue must be non-empty");
        self.currently_running_task = Some(RunningTask {
          source: task.source,
          is_microtask: true,
        });
        task.run(host, self)?;
      }
      Ok(())
    })();

    self.currently_running_task = previous_running_task;
    self.performing_microtask_checkpoint = false;
    result
  }

  /// Run a single task, if one is queued.
  ///
  /// Returns `Ok(true)` when a task was executed, `Ok(false)` when the task queue was empty.
  /// After executing a task, a microtask checkpoint is performed.
  pub fn run_next_task(&mut self, host: &mut Host) -> Result<bool> {
    self.queue_due_timers()?;

    let Some(task) = self.pop_next_task() else {
      return Ok(false);
    };

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    // Always clear running-task state so errors don't leave the event loop in a "running" state.
    self.currently_running_task = None;
    task_result?;

    self.perform_microtask_checkpoint(host)?;
    self.timer_nesting_level = previous_timer_nesting_level;
    Ok(true)
  }

  pub fn run_until_idle(&mut self, host: &mut Host, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    let mut run_state = RunState::new(limits, Arc::clone(&self.clock));

    match self.run_until_idle_inner(host, &mut run_state) {
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
        self
          .perform_microtask_checkpoint_limited(host, run_state)
          ?;
        continue;
      }

      if self.run_next_task_limited(host, run_state)? {
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

    let previous_timer_nesting_level = self.timer_nesting_level;
    if task.source != TaskSource::Timer {
      self.timer_nesting_level = 0;
    }

    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    let task_result = task.run(host, self);
    self.currently_running_task = None;
    task_result.map_err(RunStepError::Error)?;

    self.perform_microtask_checkpoint_limited(host, run_state)?;
    self.timer_nesting_level = previous_timer_nesting_level;
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

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let result = (|| -> RunStepResult<()> {
      while !self.microtask_queue.is_empty() {
        run_state.check_deadline()?;
        run_state.before_microtask()?;

        let task = self
          .microtask_queue
          .pop_front()
          .expect("microtask queue must be non-empty");
        self.currently_running_task = Some(RunningTask {
          source: task.source,
          is_microtask: true,
        });
        task.run(host, self).map_err(RunStepError::Error)?;
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
    let (task, empty) = {
      let queue = self
        .task_queues
        .get_mut(&source)
        .expect("task queue should exist for selected source");
      let task = queue.pop_front();
      let empty = queue.is_empty();
      (task, empty)
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
    if self.timers.len() >= self.queue_limits.max_pending_timers {
      return Err(Error::Other(format!(
        "EventLoop exceeded max pending timers (limit={})",
        self.queue_limits.max_pending_timers
      )));
    }

    let id = TimerId(self.next_timer_id);
    self.next_timer_id = self.next_timer_id.wrapping_add(1);

    let delay = self.clamp_timer_delay(requested_delay);
    let due = self.clock.now() + delay;

    let schedule_seq = self.next_timer_seq;
    self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

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
    if self.timer_nesting_level >= 5 {
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
        event_loop.fire_timer(host, id)
      })?;
    }
    Ok(())
  }

  fn fire_timer(&mut self, host: &mut Host, id: TimerId) -> Result<()> {
    let (kind, interval, nesting_level, mut callback) = {
      let Some(timer) = self.timers.get_mut(&id) else {
        return Ok(());
      };
      let callback = timer
        .callback
        .take()
        .expect("Timer callback should always be present while active");
      (timer.kind, timer.interval, timer.nesting_level, callback)
    };

    // Update nesting level for the duration of this task (including the microtask checkpoint that
    // `run_next_task` performs after this task returns).
    self.timer_nesting_level = (nesting_level + 1).min(5);

    if let Err(err) = (callback)(host, self) {
      if let Some(timer) = self.timers.get_mut(&id) {
        timer.callback = Some(callback);
      }
      return Err(err);
    }

    match kind {
      TimerKind::Timeout => {
        // `clearTimeout` may have already removed the timer.
        self.timers.remove(&id);
      }
      TimerKind::Interval => {
        let Some(interval) = interval else {
          return Err(Error::Other("Interval timer missing interval duration".to_string()));
        };
        let now = self.clock.now();
        let delay = self.clamp_timer_delay(interval);
        let due = now + delay;

        let nesting_level = self.timer_nesting_level;
        let schedule_seq = self.next_timer_seq;
        self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

        // The callback may have cleared this timer.
        let Some(timer) = self.timers.get_mut(&id) else {
          return Ok(());
        };
        timer.callback = Some(callback);
        timer.due = due;
        timer.nesting_level = nesting_level;
        timer.schedule_seq = schedule_seq;
        self.timer_queue.push(Reverse((due, schedule_seq, id)));
      }
    }
    Ok(())
  }
}

struct RunState {
  limits: RunLimits,
  clock: Arc<dyn Clock>,
  started_at: Duration,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl RunState {
  fn new(limits: RunLimits, clock: Arc<dyn Clock>) -> Self {
    Self {
      limits,
      started_at: clock.now(),
      clock,
      tasks_executed: 0,
      microtasks_executed: 0,
    }
  }

  fn check_deadline(&self) -> RunStepResult<()> {
    // Integrate renderer-level cancellation/deadlines.
    let stage = render_control::active_stage().unwrap_or(RenderStage::DomParse);
    render_control::check_active(stage)
      .map_err(|err| RunStepError::Error(err.into()))?;

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

  #[derive(Default)]
  struct TestHost {
    log: Vec<&'static str>,
    count: usize,
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

  fn self_requeue_microtask(host: &mut TestHost, event_loop: &mut EventLoop<TestHost>) -> Result<()> {
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
      Ok(RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxMicrotasks { .. }))
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
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| {
      Err(Error::Other("boom".to_string()))
    }).unwrap();

    let mut host = Host;
    let err = event_loop
      .run_next_task(&mut host)
      .expect_err("task should fail");
    assert!(matches!(err, Error::Other(msg) if msg == "boom"));
    assert_eq!(event_loop.currently_running_task(), None);
  }
}
