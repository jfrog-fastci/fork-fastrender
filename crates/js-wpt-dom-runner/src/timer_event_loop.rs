use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::time::Duration;
use thiserror::Error;

pub type TimerId = i32;

#[derive(Debug, Clone, Copy)]
pub struct QueueLimits {
  pub max_pending_tasks: usize,
  pub max_pending_timers: usize,
}

impl Default for QueueLimits {
  fn default() -> Self {
    Self {
      max_pending_tasks: 100_000,
      max_pending_timers: 100_000,
    }
  }
}

#[derive(Debug, Error)]
pub enum TimerLoopError {
  #[error("timer loop exceeded max pending timers (limit={limit})")]
  MaxPendingTimers { limit: usize },
  #[error("timer loop exceeded max pending tasks (limit={limit})")]
  MaxPendingTasks { limit: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
  Timeout,
  Interval,
}

#[derive(Debug, Clone, Copy)]
struct TimerState {
  kind: TimerKind,
  interval: Option<Duration>,
  due: Duration,
  schedule_seq: u64,
  nesting_level: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct TimerTask {
  pub id: TimerId,
  pub schedule_seq: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TimerExecution {
  pub id: TimerId,
  kind: TimerKind,
  interval: Option<Duration>,
  generation: u64,
  pub prev_timer_nesting_level: u32,
}

#[derive(Debug)]
pub struct TimerEventLoop {
  now: Duration,
  limits: QueueLimits,
  tasks: VecDeque<TimerTask>,
  timers: HashMap<TimerId, TimerState>,
  timer_queue: BinaryHeap<Reverse<(Duration, u64, TimerId)>>,
  next_timer_id: TimerId,
  next_timer_seq: u64,
  timer_nesting_level: u32,
}

impl Default for TimerEventLoop {
  fn default() -> Self {
    Self {
      now: Duration::from_millis(0),
      limits: QueueLimits::default(),
      tasks: VecDeque::new(),
      timers: HashMap::new(),
      timer_queue: BinaryHeap::new(),
      next_timer_id: 1,
      next_timer_seq: 0,
      timer_nesting_level: 0,
    }
  }
}

impl TimerEventLoop {
  pub fn new(limits: QueueLimits) -> Self {
    Self {
      limits,
      ..Self::default()
    }
  }

  pub fn now(&self) -> Duration {
    self.now
  }

  pub fn set_timer_nesting_level(&mut self, level: u32) {
    self.timer_nesting_level = level;
  }

  pub fn advance_to(&mut self, now: Duration) {
    self.now = now;
  }

  pub fn set_timeout(&mut self, requested_delay: Duration) -> Result<TimerId, TimerLoopError> {
    self.add_timer(TimerKind::Timeout, requested_delay, None)
  }

  pub fn set_interval(&mut self, interval: Duration) -> Result<TimerId, TimerLoopError> {
    self.add_timer(TimerKind::Interval, interval, Some(interval))
  }

  pub fn clear_timer(&mut self, id: TimerId) {
    self.timers.remove(&id);
  }

  pub fn queue_due_timers(&mut self) -> Result<(), TimerLoopError> {
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      if due > self.now {
        break;
      }
      let _ = self.timer_queue.pop();

      let Some(timer) = self.timers.get(&id) else {
        continue;
      };
      if timer.schedule_seq != schedule_seq {
        continue;
      }

      if self.tasks.len() >= self.limits.max_pending_tasks {
        return Err(TimerLoopError::MaxPendingTasks {
          limit: self.limits.max_pending_tasks,
        });
      }
      self.tasks.push_back(TimerTask { id, schedule_seq });
    }
    Ok(())
  }

  pub fn pop_task(&mut self) -> Option<TimerTask> {
    self.tasks.pop_front()
  }

  pub fn next_timer_due(&mut self) -> Option<Duration> {
    loop {
      let Reverse((due, schedule_seq, id)) = self.timer_queue.peek().copied()?;
      let Some(timer) = self.timers.get(&id) else {
        let _ = self.timer_queue.pop();
        continue;
      };
      if timer.schedule_seq != schedule_seq {
        let _ = self.timer_queue.pop();
        continue;
      }
      return Some(due);
    }
  }

  pub fn begin_timer_task(&mut self, task: TimerTask) -> Option<TimerExecution> {
    let (kind, interval, nesting_level) = {
      let timer = self.timers.get(&task.id)?;
      if timer.schedule_seq != task.schedule_seq {
        return None;
      }
      (timer.kind, timer.interval, timer.nesting_level)
    };

    let prev_timer_nesting_level = self.timer_nesting_level;
    self.timer_nesting_level = (nesting_level + 1).min(5);

    if kind == TimerKind::Timeout {
      self.timers.remove(&task.id);
    }

    Some(TimerExecution {
      id: task.id,
      kind,
      interval,
      generation: task.schedule_seq,
      prev_timer_nesting_level,
    })
  }

  pub fn finish_timer_task(&mut self, exec: TimerExecution) {
    if exec.kind != TimerKind::Interval {
      return;
    }
    let Some(interval) = exec.interval else {
      return;
    };

    let Some(timer) = self.timers.get(&exec.id) else {
      return;
    };
    if timer.schedule_seq != exec.generation {
      return;
    }

    let delay = self.clamp_timer_delay(interval);
    let due = self.now + delay;

    let schedule_seq = self.next_timer_seq;
    self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

    let Some(timer) = self.timers.get_mut(&exec.id) else {
      return;
    };
    if timer.schedule_seq != exec.generation {
      return;
    }

    timer.due = due;
    timer.nesting_level = self.timer_nesting_level;
    timer.schedule_seq = schedule_seq;

    self.timer_queue.push(Reverse((due, schedule_seq, exec.id)));
  }

  fn add_timer(
    &mut self,
    kind: TimerKind,
    requested_delay: Duration,
    interval: Option<Duration>,
  ) -> Result<TimerId, TimerLoopError> {
    if self.timers.len() >= self.limits.max_pending_timers {
      return Err(TimerLoopError::MaxPendingTimers {
        limit: self.limits.max_pending_timers,
      });
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
    let due = self.now + delay;

    let schedule_seq = self.next_timer_seq;
    self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

    self.timers.insert(
      id,
      TimerState {
        kind,
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
}
