use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{
  Clock, EventLoop, RunLimits, RunUntilIdleOutcome, VirtualClock, WindowHost, WindowHostState,
};
use fastrender::Result;
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::time::Duration;
use vm_js::Value;

#[test]
fn window_host_time_apis_are_deterministic_and_match_event_loop_clock() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<WindowHostState>::with_clock(clock_for_loop);

  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_event_loop(dom, "https://example.invalid/", event_loop)?;

  clock.set_now(Duration::from_millis(0));
  assert_eq!(
    host.exec_script("performance.timeOrigin")?,
    Value::Number(0.0)
  );
  assert_eq!(host.exec_script("performance.now()")?, Value::Number(0.0));
  assert_eq!(host.exec_script("Date.now()")?, Value::Number(0.0));
  assert_eq!(
    host.exec_script("new Date().getTime()")?,
    Value::Number(0.0)
  );

  clock.advance(Duration::from_millis(5));
  assert_eq!(host.exec_script("performance.now()")?, Value::Number(5.0));
  assert_eq!(host.exec_script("Date.now()")?, Value::Number(5.0));
  assert_eq!(
    host.exec_script("new Date().getTime()")?,
    Value::Number(5.0)
  );

  // Ensure timer scheduling observes the same clock as `performance.now()`.
  clock.set_now(Duration::from_millis(0));
  host.exec_script(
    "globalThis.__fired = false;\n\
     globalThis.__ts = undefined;\n\
     setTimeout(() => { globalThis.__fired = true; globalThis.__ts = performance.now(); }, 5);\n",
  )?;
  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(host.exec_script("__fired")?, Value::Bool(false));

  clock.advance(Duration::from_millis(5));
  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(host.exec_script("__fired")?, Value::Bool(true));
  assert_eq!(host.exec_script("__ts")?, Value::Number(5.0));

  Ok(())
}
