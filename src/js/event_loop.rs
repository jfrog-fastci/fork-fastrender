use crate::error::{Error, Result};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// HTML task sources (WHATWG terminology).
///
/// This enum is intentionally small for now, but designed to be extended as more
/// web APIs are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskSource {
  Script,
  Microtask,
  Networking,
  DOMManipulation,
}

type Runnable<Host> =
  Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static>;

/// A single runnable unit of work (task or microtask).
pub struct Task<Host> {
  pub source: TaskSource,
  runnable: Runnable<Host>,
}

impl<Host> Task<Host> {
  pub fn new<F>(source: TaskSource, runnable: F) -> Self
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    Self {
      source,
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
pub struct RunningTask {
  pub source: TaskSource,
  pub is_microtask: bool,
}

pub struct EventLoop<Host> {
  task_queue: VecDeque<Task<Host>>,
  microtask_queue: VecDeque<Task<Host>>,
  performing_microtask_checkpoint: bool,
  currently_running_task: Option<RunningTask>,
}

impl<Host> Default for EventLoop<Host> {
  fn default() -> Self {
    Self {
      task_queue: VecDeque::new(),
      microtask_queue: VecDeque::new(),
      performing_microtask_checkpoint: false,
      currently_running_task: None,
    }
  }
}

impl<Host> EventLoop<Host> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn currently_running_task(&self) -> Option<RunningTask> {
    self.currently_running_task
  }

  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F)
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    self.task_queue.push_back(Task::new(source, runnable));
  }

  pub fn queue_microtask<F>(&mut self, runnable: F)
  where
    F: FnOnce(&mut Host, &mut EventLoop<Host>) -> Result<()> + 'static,
  {
    self
      .microtask_queue
      .push_back(Task::new(TaskSource::Microtask, runnable));
  }

  /// Perform a microtask checkpoint (HTML Standard terminology).
  ///
  /// - If a checkpoint is already in progress, this is a no-op (reentrancy guard).
  /// - Otherwise, drains the microtask queue until it becomes empty.
  pub fn perform_microtask_checkpoint(&mut self, host: &mut Host) -> Result<()> {
    self.perform_microtask_checkpoint_inner(host, None)
  }

  fn perform_microtask_checkpoint_inner(
    &mut self,
    host: &mut Host,
    mut run_state: Option<&mut RunState>,
  ) -> Result<()> {
    if self.performing_microtask_checkpoint {
      return Ok(());
    }

    self.performing_microtask_checkpoint = true;
    let previous_running_task = self.currently_running_task.take();

    let result = (|| {
      while !self.microtask_queue.is_empty() {
        if let Some(run_state) = run_state.as_deref_mut() {
          run_state.check_deadline()?;
          run_state.before_microtask()?;
        }

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
    self.run_next_task_inner(host, None)
  }

  fn run_next_task_inner(
    &mut self,
    host: &mut Host,
    mut run_state: Option<&mut RunState>,
  ) -> Result<bool> {
    if self.task_queue.is_empty() {
      return Ok(false);
    }

    if let Some(run_state) = run_state.as_deref_mut() {
      run_state.check_deadline()?;
      run_state.before_task()?;
    }

    let task = self
      .task_queue
      .pop_front()
      .expect("task queue must be non-empty");
    self.currently_running_task = Some(RunningTask {
      source: task.source,
      is_microtask: false,
    });
    task.run(host, self)?;
    self.currently_running_task = None;

    self.perform_microtask_checkpoint_inner(host, run_state.as_deref_mut())?;
    Ok(true)
  }

  pub fn run_until_idle(&mut self, host: &mut Host, limits: RunLimits) -> Result<()> {
    let mut run_state = RunState::new(limits);
    loop {
      run_state.check_deadline()?;

      if !self.microtask_queue.is_empty() {
        self.perform_microtask_checkpoint_inner(host, Some(&mut run_state))?;
        continue;
      }

      if !self.run_next_task_inner(host, Some(&mut run_state))? {
        break;
      }
    }
    Ok(())
  }
}

struct RunState {
  limits: RunLimits,
  started_at: Instant,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl RunState {
  fn new(limits: RunLimits) -> Self {
    Self {
      limits,
      started_at: Instant::now(),
      tasks_executed: 0,
      microtasks_executed: 0,
    }
  }

  fn check_deadline(&self) -> Result<()> {
    let Some(max_wall_time) = self.limits.max_wall_time else {
      return Ok(());
    };
    let elapsed = self.started_at.elapsed();
    if elapsed > max_wall_time {
      return Err(Error::Other(format!(
        "Event loop exceeded wall-time limit (elapsed={elapsed:?} limit={max_wall_time:?})"
      )));
    }
    Ok(())
  }

  fn before_task(&mut self) -> Result<()> {
    if self.tasks_executed >= self.limits.max_tasks {
      return Err(Error::Other(format!(
        "Event loop exceeded max task executions (limit={})",
        self.limits.max_tasks
      )));
    }
    self.tasks_executed += 1;
    Ok(())
  }

  fn before_microtask(&mut self) -> Result<()> {
    if self.microtasks_executed >= self.limits.max_microtasks {
      return Err(Error::Other(format!(
        "Event loop exceeded max microtask executions (limit={})",
        self.limits.max_microtasks
      )));
    }
    self.microtasks_executed += 1;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct TestHost {
    log: Vec<&'static str>,
    count: usize,
  }

  #[test]
  fn microtasks_run_after_a_task_and_before_the_next_task() -> Result<()> {
    let mut host = TestHost::default();
    let mut event_loop = EventLoop::<TestHost>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      host.log.push("task1");
      event_loop.queue_microtask(|host, _| {
        host.log.push("microtask1");
        Ok(())
      });
      Ok(())
    });

    event_loop.queue_task(TaskSource::Script, |host, _| {
      host.log.push("task2");
      Ok(())
    });

    assert!(event_loop.run_next_task(&mut host)?);
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.log, vec!["task1", "microtask1", "task2"]);
    Ok(())
  }

  #[test]
  fn microtasks_queued_by_microtasks_run_in_the_same_checkpoint() -> Result<()> {
    let mut host = TestHost::default();
    let mut event_loop = EventLoop::<TestHost>::new();

    event_loop.queue_microtask(|host, event_loop| {
      host.log.push("microtask1");
      event_loop.queue_microtask(|host, _| {
        host.log.push("microtask2");
        Ok(())
      });
      Ok(())
    });

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.log, vec!["microtask1", "microtask2"]);
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_reentrancy_guard_prevents_recursion() -> Result<()> {
    let mut host = TestHost::default();
    let mut event_loop = EventLoop::<TestHost>::new();

    event_loop.queue_microtask(|host, event_loop| {
      host.count += 1;
      event_loop.perform_microtask_checkpoint(host)?;
      Ok(())
    });

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.count, 1);
    Ok(())
  }

  fn self_requeue_microtask(host: &mut TestHost, event_loop: &mut EventLoop<TestHost>) -> Result<()> {
    host.count += 1;
    event_loop.queue_microtask(self_requeue_microtask);
    Ok(())
  }

  #[test]
  fn run_limits_stop_infinite_microtask_chains() {
    let mut host = TestHost::default();
    let mut event_loop = EventLoop::<TestHost>::new();

    event_loop.queue_microtask(self_requeue_microtask);

    let result = event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 100,
        max_microtasks: 5,
        max_wall_time: None,
      },
    );
    assert!(result.is_err(), "expected run_until_idle to hit microtask limit");
    assert_eq!(host.count, 5);
  }
}
