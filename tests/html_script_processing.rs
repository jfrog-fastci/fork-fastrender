use fastrender::js::{
  parse_and_run_classic_scripts, ClassicScriptExecutor, ClassicScriptFetcher, CurrentScriptHost,
  DomHost, EventLoop, ScriptId, ScriptType,
};
use fastrender::{dom2, Error, Result};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

#[derive(Default)]
struct FakeFetcher {
  sources: HashMap<String, String>,
  completion_plan: VecDeque<String>,
  started_by_url: HashMap<String, ScriptId>,
  started_urls: Rc<RefCell<Vec<String>>>,
  ready: VecDeque<(ScriptId, String)>,
}

impl FakeFetcher {
  fn new() -> Self {
    Self::default()
  }

  fn with_source(mut self, url: &str, source: &str) -> Self {
    self.sources.insert(url.to_string(), source.to_string());
    self
  }

  fn with_completion_order(mut self, urls: &[&str]) -> Self {
    self.completion_plan = urls.iter().map(|u| u.to_string()).collect();
    self
  }
}

impl ClassicScriptFetcher for FakeFetcher {
  fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()> {
    self.started_urls.borrow_mut().push(url.to_string());
    self.started_by_url.insert(url.to_string(), script_id);
    Ok(())
  }

  fn poll_complete(&mut self) -> Result<Option<(ScriptId, String)>> {
    // Promote ready fetches in the configured completion order, but only once the URL has been
    // started (i.e. discovered by the parser).
    while let Some(next_url) = self.completion_plan.front().cloned() {
      let Some(&script_id) = self.started_by_url.get(&next_url) else {
        break;
      };
      self.completion_plan.pop_front();
      let source = self
        .sources
        .get(&next_url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no script source for url={next_url}")))?;
      self.ready.push_back((script_id, source));
    }
    Ok(self.ready.pop_front())
  }
}

#[derive(Default, Clone)]
struct RecordingExecutor {
  events: Rc<RefCell<Vec<String>>>,
  current_during_exec: Rc<RefCell<Vec<(dom2::NodeId, Option<dom2::NodeId>)>>>,
  current_during_microtask: Rc<RefCell<Vec<Option<dom2::NodeId>>>>,
}

impl RecordingExecutor {
  fn new() -> Self {
    Self::default()
  }
}

impl<Host> ClassicScriptExecutor<Host> for RecordingExecutor
where
  Host: CurrentScriptHost + DomHost + 'static,
{
  fn execute(
    &mut self,
    host: &mut Host,
    source_text: &str,
    script_node_id: dom2::NodeId,
    script_type: ScriptType,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    assert_eq!(
      script_type,
      ScriptType::Classic,
      "parse_and_run_classic_scripts should execute only classic scripts in v1"
    );
    self.events
      .borrow_mut()
      .push(format!("script:{source_text}"));
    self
      .current_during_exec
      .borrow_mut()
      .push((script_node_id, host.current_script()));

    let events = Rc::clone(&self.events);
    let current = Rc::clone(&self.current_during_microtask);
    let text = source_text.to_string();
    event_loop.queue_microtask(move |host, _event_loop| {
      events.borrow_mut().push(format!("microtask:{text}"));
      current.borrow_mut().push(host.current_script());
      Ok(())
    })?;

    Ok(())
  }
}

#[test]
fn blocking_external_script_delays_later_inline_script_until_fetched_and_executed() -> Result<()> {
  let html = "<!doctype html><script src=\"https://ex/a.js\"></script><script>INLINE</script>";

  let exec = RecordingExecutor::new();
  let events = Rc::clone(&exec.events);

  let fetcher = FakeFetcher::new()
    .with_source("https://ex/a.js", "A")
    .with_completion_order(&["https://ex/a.js"]);

  let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

  assert_eq!(
    &*events.borrow(),
    &[
      "script:A".to_string(),
      "microtask:A".to_string(),
      "script:INLINE".to_string(),
      "microtask:INLINE".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn async_external_scripts_execute_in_completion_order() -> Result<()> {
  let html = concat!(
    "<!doctype html>",
    "<script async src=\"https://ex/a1.js\"></script>",
    "<script async src=\"https://ex/a2.js\"></script>",
  );

  let exec = RecordingExecutor::new();
  let events = Rc::clone(&exec.events);

  let fetcher = FakeFetcher::new()
    .with_source("https://ex/a1.js", "a1")
    .with_source("https://ex/a2.js", "a2")
    // a2 completes before a1, regardless of discovery order.
    .with_completion_order(&["https://ex/a2.js", "https://ex/a1.js"]);

  let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

  assert_eq!(
    &*events.borrow(),
    &[
      "script:a2".to_string(),
      "microtask:a2".to_string(),
      "script:a1".to_string(),
      "microtask:a1".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn defer_scripts_execute_after_parsing_completes_in_document_order() -> Result<()> {
  let html = concat!(
    "<!doctype html>",
    "<script defer src=\"https://ex/d1.js\"></script>",
    "<script defer src=\"https://ex/d2.js\"></script>",
    "<div id=end></div>",
  );

  let exec = RecordingExecutor::new();
  let events = Rc::clone(&exec.events);

  let fetcher = FakeFetcher::new()
    .with_source("https://ex/d1.js", "d1")
    .with_source("https://ex/d2.js", "d2")
    // d2 completes before d1.
    .with_completion_order(&["https://ex/d2.js", "https://ex/d1.js"]);

  let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

  assert_eq!(
    &*events.borrow(),
    &[
      "script:d1".to_string(),
      "microtask:d1".to_string(),
      "script:d2".to_string(),
      "microtask:d2".to_string(),
    ]
  );
  Ok(())
}

#[test]
fn microtask_checkpoint_runs_after_every_script_and_clears_current_script() -> Result<()> {
  let html = concat!(
    "<!doctype html>",
    "<script>ONE</script>",
    "<script async src=\"https://ex/a.js\"></script>",
    "<script>THREE</script>",
  );

  let exec = RecordingExecutor::new();
  let events = Rc::clone(&exec.events);
  let current_exec = Rc::clone(&exec.current_during_exec);
  let current_micro = Rc::clone(&exec.current_during_microtask);

  let fetcher = FakeFetcher::new()
    .with_source("https://ex/a.js", "TWO")
    .with_completion_order(&["https://ex/a.js"]);

  let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

  // Async scripts can run at any point after they are discovered and downloaded, so only assert:
  // - scripts/microtasks alternate (microtask checkpoint runs after each script),
  // - inline scripts preserve document order relative to each other.
  let ev = events.borrow();
  assert_eq!(ev.len(), 6, "expected 3 scripts + 3 microtask checkpoints");
  for pair in ev.chunks(2) {
    let [script, micro] = pair else { unreachable!() };
    assert!(
      script.starts_with("script:"),
      "expected script entry, got {script:?}"
    );
    assert_eq!(
      micro,
      &script.replacen("script:", "microtask:", 1),
      "expected microtask checkpoint entry following {script:?}"
    );
  }
  let idx_one = ev.iter().position(|e| e == "script:ONE").unwrap();
  let idx_three = ev.iter().position(|e| e == "script:THREE").unwrap();
  assert!(
    idx_one < idx_three,
    "expected inline scripts to execute in document order"
  );

  // `Document.currentScript` should be set during each classic script execution.
  assert_eq!(current_exec.borrow().len(), 3);
  for (script_node_id, observed_current) in current_exec.borrow().iter().copied() {
    assert_eq!(
      observed_current,
      Some(script_node_id),
      "expected currentScript to be set to the executing script element"
    );
  }
  // It should be cleared/restored by the time microtasks run after the script.
  assert_eq!(current_micro.borrow().len(), 3);
  assert!(current_micro.borrow().iter().all(|v| v.is_none()));
  Ok(())
}

#[test]
fn base_url_is_snapshotted_at_script_discovery_time() -> Result<()> {
  let html = r#"<!doctype html>
    <html><head>
      <script src="a.js"></script>
      <base href="https://ex/base/">
    </head></html>"#;

  let started_urls = Rc::new(RefCell::new(Vec::new()));
  let mut fetcher = FakeFetcher::new()
    .with_source("https://ex/a.js", "A")
    .with_completion_order(&["https://ex/a.js"]);
  fetcher.started_urls = Rc::clone(&started_urls);

  let exec = RecordingExecutor::new();
  let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

  assert_eq!(
    &*started_urls.borrow(),
    &["https://ex/a.js".to_string()],
    "expected <script src=\"a.js\"> before <base> to resolve against the document URL"
  );
  Ok(())
}
