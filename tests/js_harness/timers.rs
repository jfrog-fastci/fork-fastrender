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
