use fastrender::js::{
  EventLoop, RunLimits, RunUntilIdleOutcome, RunUntilIdleStopReason, TaskSource, TimerId,
  VirtualClock,
};
use fastrender::Result;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Host {
  log: Vec<&'static str>,
  ticks: usize,
  interval_id: Option<TimerId>,
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
  let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

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
  let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());

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
