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

