use fastrender::api::SharedRenderDiagnostics;
use fastrender::error::RenderStage;
use fastrender::js::{BrowserTab, RunLimits, TaskSource};
use fastrender::{Error, Result};

#[test]
fn js_exceptions_are_captured_and_do_not_abort_subsequent_execution() -> Result<()> {
  let diagnostics = SharedRenderDiagnostics::new();
  let mut tab = BrowserTab::new(Some(diagnostics.clone()));

  tab.execute_script(|host, _event_loop| {
    host.debug_log.push("script1".to_string());
    Err(Error::Other("boom".to_string()))
  });
  tab.execute_script(|host, _event_loop| {
    host.debug_log.push("script2".to_string());
    Ok(())
  });

  tab.queue_task(TaskSource::Script, |host, _event_loop| {
    host.debug_log.push("task1".to_string());
    Err(Error::Other("task boom".to_string()))
  })?;
  tab.queue_task(TaskSource::Script, |host, _event_loop| {
    host.debug_log.push("task2".to_string());
    Ok(())
  })?;

  // The first task throws, but the event loop keeps running.
  tab.run_until_idle(RunLimits::unbounded())?;

  assert_eq!(
    tab.host().debug_log,
    vec![
      "script1".to_string(),
      "script2".to_string(),
      "task1".to_string(),
      "task2".to_string()
    ]
  );

  let captured = diagnostics.into_inner();
  assert_eq!(captured.failure_stage, Some(RenderStage::Script));
  assert_eq!(captured.js_exceptions.len(), 2);
  assert!(
    captured.js_exceptions[0].message.contains("boom"),
    "expected first exception to contain boom; got {:?}",
    captured.js_exceptions[0]
  );
  assert!(
    captured.js_exceptions[1].message.contains("task boom"),
    "expected second exception to contain task boom; got {:?}",
    captured.js_exceptions[1]
  );

  Ok(())
}

