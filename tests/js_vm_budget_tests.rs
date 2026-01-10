use fastrender::dom2;
use fastrender::js::{JsExecutionOptions, WindowHost};
use selectors::context::QuirksMode;
use std::time::Duration;

#[test]
fn exec_script_infinite_loop_is_terminated_by_instruction_budget() -> fastrender::Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut opts = JsExecutionOptions::default();
  opts.max_instruction_count = Some(50);
  // Keep wall-time generous so we reliably hit fuel termination first.
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));

  let mut host = WindowHost::new_with_options(dom, "https://example.invalid/", opts)?;
  let err = host.exec_script("for(;;){}").expect_err("expected script to terminate");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("out of fuel"),
    "expected OutOfFuel termination, got: {msg}"
  );
  Ok(())
}

#[test]
fn exec_script_deadline_budget_can_terminate_immediately() -> fastrender::Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut opts = JsExecutionOptions::default();
  // Force an already-expired wall-time deadline so the first `tick()` fails.
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_millis(0));

  let mut host = WindowHost::new_with_options(dom, "https://example.invalid/", opts)?;
  let err = host.exec_script("for(;;){}").expect_err("expected deadline termination");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("deadline exceeded"),
    "expected DeadlineExceeded termination, got: {msg}"
  );
  Ok(())
}

