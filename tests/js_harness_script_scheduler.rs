mod js_harness;

use fastrender::js::{ClassicScriptScheduler, RunLimits, RunUntilIdleOutcome, ScriptElementSpec, ScriptType};
use fastrender::Result;
use std::collections::HashMap;

fn external_spec(url: &str, async_attr: bool, defer_attr: bool) -> ScriptElementSpec {
  ScriptElementSpec {
    base_url: Some("https://example.com/".to_string()),
    src: Some(url.to_string()),
    inline_text: String::new(),
    async_attr,
    defer_attr,
    parser_inserted: true,
    script_type: ScriptType::Classic,
  }
}

#[test]
fn async_external_scripts_execute_in_completion_order() -> Result<()> {
  let mut harness = js_harness::Harness::new("https://example.com/", "<!doctype html><html></html>")?;
  harness.set_external_script_sources(HashMap::from([
    ("https://example.com/a.js".to_string(), "console.log('a');".to_string()),
    ("https://example.com/b.js".to_string(), "console.log('b');".to_string()),
  ]));

  let mut scheduler = ClassicScriptScheduler::<js_harness::HostState>::new();
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.handle_script(host, event_loop, external_spec("https://example.com/a.js", true, false))?;
  }
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.handle_script(host, event_loop, external_spec("https://example.com/b.js", true, false))?;
  }

  // Complete `b` first, ensuring it runs first.
  harness.complete_external_script("https://example.com/b.js")?;
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.poll(host, event_loop)?;
  }
  assert_eq!(
    harness.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.take_log(), vec!["b".to_string()]);

  harness.complete_external_script("https://example.com/a.js")?;
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.poll(host, event_loop)?;
  }
  assert_eq!(
    harness.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.take_log(), vec!["a".to_string()]);

  Ok(())
}

#[test]
fn defer_external_scripts_execute_in_document_order_after_parsing_finished() -> Result<()> {
  let mut harness = js_harness::Harness::new("https://example.com/", "<!doctype html><html></html>")?;
  harness.set_external_script_sources(HashMap::from([
    ("https://example.com/1.js".to_string(), "console.log('d1');".to_string()),
    ("https://example.com/2.js".to_string(), "console.log('d2');".to_string()),
  ]));

  let mut scheduler = ClassicScriptScheduler::<js_harness::HostState>::new();
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.handle_script(host, event_loop, external_spec("https://example.com/1.js", false, true))?;
  }
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.handle_script(host, event_loop, external_spec("https://example.com/2.js", false, true))?;
  }

  // Defer scripts should execute in insertion order (1 then 2), regardless of fetch completion
  // ordering. Complete 2 first to validate the scheduler logic.
  harness.complete_external_script("https://example.com/2.js")?;
  harness.complete_external_script("https://example.com/1.js")?;
  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.poll(host, event_loop)?;
  }

  // Parsing isn't finished yet; defer scripts should not have run.
  assert_eq!(
    harness.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.take_log(), Vec::<String>::new());

  {
    let (host, event_loop) = harness.host_and_event_loop_mut();
    scheduler.finish_parsing(host, event_loop)?;
  }
  assert_eq!(
    harness.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    harness.take_log(),
    vec!["d1".to_string(), "d2".to_string()]
  );

  Ok(())
}
