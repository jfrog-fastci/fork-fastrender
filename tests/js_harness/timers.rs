use super::Harness;
use fastrender::js::RunLimits;
use fastrender::Result;

#[test]
fn harness_set_timeout_orders_by_due_time_then_registration_order() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      setTimeout(() => console.log("t10"), 10);
      setTimeout(() => console.log("t5a"), 5);
      setTimeout(() => console.log("t5b"), 5);
    "#,
  )?;

  // Nothing due yet.
  h.run_until_idle(RunLimits::unbounded())?;
  assert!(h.take_log().is_empty());

  h.advance_time(5);
  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["t5a".to_string(), "t5b".to_string()]);

  h.advance_time(5);
  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["t10".to_string()]);
  Ok(())
}

#[test]
fn harness_set_timeout_passes_args_and_binds_this_to_window() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      setTimeout(function (a, b) {
        console.log(a);
        console.log(b);
        console.log(this === window);
      }, 0, 1, "x");
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec!["1".to_string(), "x".to_string(), "true".to_string()]
  );
  Ok(())
}

#[test]
fn harness_queue_microtask_calls_callback_with_undefined_this_in_strict_mode() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      queueMicrotask(function () {
        "use strict";
        console.log(this === undefined);
      });
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(h.take_log(), vec!["true".to_string()]);
  Ok(())
}

#[test]
fn harness_microtasks_run_after_each_timer_task() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      setTimeout(() => {
        console.log("t1");
        queueMicrotask(() => console.log("m1"));
      }, 0);
      setTimeout(() => console.log("t2"), 0);
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec![
      "t1".to_string(),
      "m1".to_string(),
      "t2".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn harness_set_interval_passes_args_and_can_cancel_itself() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      var count = 0;
      var id = setInterval(function (x) {
        console.log(x);
        console.log(this === window);
        count++;
        if (count === 2) clearInterval(id);
      }, 0, "x");
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec![
      "x".to_string(),
      "true".to_string(),
      "x".to_string(),
      "true".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn harness_set_timeout_rejects_string_handler() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      try {
        setTimeout("console.log('nope')", 0);
      } catch (e) {
        console.log(e instanceof TypeError);
        console.log(e.message);
      }
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec![
      "true".to_string(),
      "setTimeout does not currently support string handlers".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn harness_promises_run_after_script_completes() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      console.log("sync-start");
      Promise.resolve().then(() => console.log("promise"));
      console.log("sync-end");
    "#,
  )?;

  // Promise jobs should run at the microtask checkpoint after script evaluation.
  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec![
      "sync-start".to_string(),
      "sync-end".to_string(),
      "promise".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn harness_promise_and_queue_microtask_preserve_ordering() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      Promise.resolve().then(() => console.log("p1"));
      queueMicrotask(() => console.log("qm"));
      Promise.resolve().then(() => console.log("p2"));
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec!["p1".to_string(), "qm".to_string(), "p2".to_string()]
  );
  Ok(())
}

#[test]
fn harness_promise_jobs_run_between_timer_tasks() -> Result<()> {
  let html = "<!doctype html><html><body></body></html>";
  let mut h = Harness::new("https://example.com/", html)?;

  h.exec_script(
    r#"
      setTimeout(() => {
        console.log("t1");
        Promise.resolve().then(() => console.log("p"));
      }, 0);
      setTimeout(() => console.log("t2"), 0);
    "#,
  )?;

  h.run_until_idle(RunLimits::unbounded())?;
  assert_eq!(
    h.take_log(),
    vec!["t1".to_string(), "p".to_string(), "t2".to_string()]
  );
  Ok(())
}
