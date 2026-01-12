#![cfg(feature = "vmjs")]

use fastrender::js::{
  Clock, EventLoop, MicrotaskCheckpointLimitedOutcome, RunLimits, RunNextTaskLimitedOutcome,
  RunState, RunUntilIdleStopReason, TaskSource, VirtualClock,
};
use fastrender::Result;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Host {
  count: usize,
  log: Vec<&'static str>,
}

fn self_requeue_microtask(host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
  host.count += 1;
  event_loop.queue_microtask(self_requeue_microtask)?;
  Ok(())
}

#[test]
fn limited_microtask_checkpoint_is_budgeted_and_stateful_across_calls() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop.clone());
  let mut host = Host::default();

  event_loop.queue_microtask(self_requeue_microtask)?;

  let limits = RunLimits {
    max_tasks: usize::MAX,
    max_microtasks: 5,
    max_wall_time: None,
  };
  let mut run_state = RunState::new(
    limits,
    clock_for_loop.clone(),
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

  // Reusing the same run state preserves counters and should stop immediately (without hanging).
  assert_eq!(
    event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state)?,
    MicrotaskCheckpointLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxMicrotasks {
      executed: 5,
      limit: 5
    })
  );
  assert_eq!(host.count, 5);

  // A fresh run state should allow further progress, proving the next microtask was not dropped
  // when the limit was hit.
  let mut run_state2 = RunState::new(limits, clock_for_loop, event_loop.default_deadline_stage());
  assert_eq!(
    event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state2)?,
    MicrotaskCheckpointLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxMicrotasks {
      executed: 5,
      limit: 5
    })
  );
  assert_eq!(host.count, 10);
  Ok(())
}

#[test]
fn run_next_task_limited_stops_before_popping_next_task_at_max_tasks() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
  let mut host = Host::default();

  event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
    host.log.push("task1");
    Ok(())
  })?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.log.push("task2");
    // Ensure the post-task microtask checkpoint still runs (this should be drained before the next task).
    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask-after-task2");
      Ok(())
    })?;
    Ok(())
  })?;

  let limits = RunLimits {
    max_tasks: 1,
    max_microtasks: usize::MAX,
    max_wall_time: Some(Duration::from_secs(60)),
  };
  let mut run_state = event_loop.new_run_state(limits);

  assert_eq!(
    event_loop.run_next_task_limited(&mut host, &mut run_state)?,
    RunNextTaskLimitedOutcome::Ran
  );
  assert_eq!(host.log, vec!["task1"]);

  // The second task must not be popped when the max-task limit is hit.
  assert_eq!(
    event_loop.run_next_task_limited(&mut host, &mut run_state)?,
    RunNextTaskLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxTasks {
      executed: 1,
      limit: 1
    })
  );
  assert_eq!(host.log, vec!["task1"]);

  // Reset budgets and verify the queued task still runs.
  let mut run_state2 = event_loop.new_run_state(limits);
  assert_eq!(
    event_loop.run_next_task_limited(&mut host, &mut run_state2)?,
    RunNextTaskLimitedOutcome::Ran
  );
  assert_eq!(host.log, vec!["task1", "task2", "microtask-after-task2"]);

  assert_eq!(
    event_loop.run_next_task_limited(&mut host, &mut run_state2)?,
    RunNextTaskLimitedOutcome::NoTask
  );
  Ok(())
}
