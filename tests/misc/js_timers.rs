use fastrender::js::{
  EventLoop, JsVmHost, RunLimits, RunUntilIdleOutcome, TaskSource, VirtualClock, WebTime,
};
use fastrender::{Error, Result};
use std::sync::Arc;
use std::time::Duration;

fn read_log(host: &JsVmHost) -> Result<Vec<serde_json::Value>> {
  let json: String = host.eval(r#"JSON.stringify(__log)"#)?;
  serde_json::from_str(&json).map_err(|e| Error::Other(e.to_string()))
}

fn reset_log(host: &JsVmHost) -> Result<()> {
  host.eval::<()>("var __log = [];")
}

#[test]
fn ordering_queue_microtask_runs_before_timeout() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      setTimeout(() => __log.push("t"), 0);
      queueMicrotask(() => __log.push("m"));
      __log.push("sync");
    "#,
    )
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::String("sync".to_string()),
      serde_json::Value::String("m".to_string()),
      serde_json::Value::String("t".to_string()),
    ]
  );

  Ok(())
}

#[test]
fn interval_repeats_and_clear_interval_stops_rescheduling() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock.clone());
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      let count = 0;
      let id = setInterval(() => {
        __log.push("tick");
        count++;
        if (count === 3) clearInterval(id);
      }, 5);
    "#,
    )
  })?;

  // Interval is due in the future; nothing should run yet.
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(read_log(&host)?, Vec::<serde_json::Value>::new());

  for _ in 0..3 {
    clock.advance(Duration::from_millis(5));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
  }

  // After 3 ticks, the interval clears itself and does not reschedule.
  clock.advance(Duration::from_millis(5));
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::String("tick".to_string()),
      serde_json::Value::String("tick".to_string()),
      serde_json::Value::String("tick".to_string()),
    ]
  );
  Ok(())
}

#[test]
fn timeout_delivers_additional_arguments() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      setTimeout(function (a, b) {
        __log.push(a);
        __log.push(b);
      }, 0, 1, "x");
    "#,
    )
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::Number(1.into()),
      serde_json::Value::String("x".to_string()),
    ]
  );
  Ok(())
}

#[test]
fn date_now_advances_with_virtual_clock_and_time_origin() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock.clone());
  let mut host = JsVmHost::new(WebTime {
    time_origin_unix_ms: 1_000,
  })?;

  reset_log(&host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(Date.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  clock.advance(Duration::from_micros(1500));
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(Date.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  clock.advance(Duration::from_millis(10));
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(Date.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::Number(1000.into()),
      serde_json::Value::Number(1001.into()),
      serde_json::Value::Number(1011.into()),
    ]
  );
  Ok(())
}

#[test]
fn performance_now_is_deterministic_f64_milliseconds() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock.clone());
  let mut host = JsVmHost::new(WebTime {
    time_origin_unix_ms: 0,
  })?;

  reset_log(&host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(performance.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  clock.advance(Duration::from_micros(1500));
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(performance.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  clock.advance(Duration::from_millis(10));
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(performance.now());")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = read_log(&host)?;
  assert_eq!(log.len(), 3);
  assert_eq!(log[0].as_f64(), Some(0.0));
  assert_eq!(log[1].as_f64(), Some(1.5));
  assert_eq!(log[2].as_f64(), Some(11.5));
  Ok(())
}

#[test]
fn performance_time_origin_reflects_web_time_origin() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime {
    time_origin_unix_ms: 1234,
  })?;

  reset_log(&host)?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "__log.push(performance.timeOrigin);")
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(
    read_log(&host)?,
    vec![serde_json::Value::Number(1234.into())]
  );
  Ok(())
}

#[test]
fn queue_microtask_invokes_callback_with_undefined_this() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      queueMicrotask(function () {
        "use strict";
        __log.push(this === undefined);
      });
    "#,
    )
  })?;
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  assert_eq!(read_log(&host)?, vec![serde_json::Value::Bool(true)]);
  Ok(())
}

#[test]
fn clear_timeout_cancels_due_timer_before_it_executes() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;
  // Enqueue two script tasks. The first schedules a 0ms timer; the second clears it.
  // When the timer becomes due it will be queued as a Timer task *after* these Script tasks, so the
  // clear runs after the timer is due but before the timer task executes.
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      globalThis.__id = setTimeout(() => __log.push("timer"), 0);
    "#,
    )
  })?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(event_loop, "clearTimeout(globalThis.__id);")
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(read_log(&host)?, Vec::<serde_json::Value>::new());
  Ok(())
}

#[test]
fn nested_timeouts_are_clamped_after_five_nesting_levels() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock.clone());
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      let count = 0;
      function tick() {
        __log.push(count);
        count++;
        if (count < 6) setTimeout(tick, 0);
      }
      setTimeout(tick, 0);
    "#,
    )
  })?;

  // The first 5 nested 0ms timeouts should run immediately. The 6th should be clamped to 4ms.
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::Number(0.into()),
      serde_json::Value::Number(1.into()),
      serde_json::Value::Number(2.into()),
      serde_json::Value::Number(3.into()),
      serde_json::Value::Number(4.into()),
    ]
  );

  // Not yet due.
  clock.advance(Duration::from_millis(3));
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(read_log(&host)?.len(), 5);

  // Now due.
  clock.advance(Duration::from_millis(1));
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(read_log(&host)?.len(), 6);
  Ok(())
}

#[test]
fn microtasks_queued_from_timer_run_before_next_task() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<JsVmHost>::with_clock(clock);
  let mut host = JsVmHost::new(WebTime::default())?;

  reset_log(&host)?;
  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script(
      event_loop,
      r#"
      setTimeout(() => {
        __log.push("t1");
        queueMicrotask(() => __log.push("m1"));
      }, 0);
      setTimeout(() => __log.push("t2"), 0);
    "#,
    )
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    read_log(&host)?,
    vec![
      serde_json::Value::String("t1".to_string()),
      serde_json::Value::String("m1".to_string()),
      serde_json::Value::String("t2".to_string()),
    ]
  );
  Ok(())
}
