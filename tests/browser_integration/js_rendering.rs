use fastrender::dom2::{Document, NodeId};
use fastrender::error::{Error, Result};
use fastrender::html::base_url_tracker::BaseUrlTracker;
use fastrender::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use fastrender::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use fastrender::js::{
  Clock, EventLoop, RunLimits, RunUntilIdleOutcome, ScriptBlockExecutor, ScriptElementSpec,
  ScriptId, ScriptOrchestrator, ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
  VirtualClock, WindowHostState,
};
use fastrender::resource::ResourceFetcher;
use fastrender::{FastRender, RenderOptions, ResourcePolicy};
use selectors::context::QuirksMode;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

use super::support::FileResourceFetcher;

fn offline_renderer() -> Result<FastRender> {
  super::support::deterministic_renderer_builder()
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .build()
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/html/js")
}

fn fixture_path(name: &str) -> PathBuf {
  fixtures_dir().join(name)
}

fn read_fixture(name: &str) -> Result<String> {
  std::fs::read_to_string(fixture_path(name))
    .map_err(|err| Error::Other(format!("failed to read fixture {name}: {err}")))
}

fn file_url_for_path(path: &Path) -> Result<String> {
  Url::from_file_path(path)
    .map(|url| url.to_string())
    .map_err(|()| Error::Other(format!("failed to convert path to file:// URL: {path:?}")))
}

fn render_static_fixture(name: &str, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
  let html = read_fixture(name)?;
  let mut renderer = offline_renderer()?;
  renderer.render_html_with_options(&html, options)
}

fn fetch_script_source(fetcher: &dyn ResourceFetcher, url: &str) -> Result<String> {
  let res = fetcher.fetch(url)?;
  String::from_utf8(res.bytes).map_err(|err| {
    Error::Other(format!(
      "script source was not valid UTF-8: url={url:?} err={err}"
    ))
  })
}

#[derive(Debug, Clone)]
struct ScriptEntry {
  node_id: NodeId,
  spec: ScriptElementSpec,
}

struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl JsExecutionGuard {
  fn enter(depth: &Rc<Cell<usize>>) -> Self {
    depth.set(depth.get().saturating_add(1));
    Self {
      depth: Rc::clone(depth),
    }
  }
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let current = self.depth.get();
    self.depth.set(current.saturating_sub(1));
  }
}

struct SchedulerState {
  fetcher: Arc<dyn ResourceFetcher>,
  scheduler: ScriptScheduler<NodeId>,
  orchestrator: ScriptOrchestrator,
  scripts: HashMap<ScriptId, ScriptEntry>,
  executed: HashSet<ScriptId>,
  pending_fetches: HashMap<String, ScriptId>,
  parser_blocked_on: Option<ScriptId>,
  js_execution_depth: Rc<Cell<usize>>,
}

impl SchedulerState {
  fn new(fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self {
      fetcher,
      scheduler: ScriptScheduler::new(),
      orchestrator: ScriptOrchestrator::new(),
      scripts: HashMap::new(),
      executed: HashSet::new(),
      pending_fetches: HashMap::new(),
      parser_blocked_on: None,
      js_execution_depth: Rc::new(Cell::new(0)),
    }
  }

  fn js_execution_depth(&self) -> &Rc<Cell<usize>> {
    &self.js_execution_depth
  }

  fn register_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
  ) -> Result<(ScriptId, Vec<ScriptSchedulerAction<NodeId>>)> {
    let base_url_at_discovery = spec.base_url.clone();
    let discovered =
      self
        .scheduler
        .discovered_parser_script(spec.clone(), node_id, base_url_at_discovery)?;
    self
      .scripts
      .insert(discovered.id, ScriptEntry { node_id, spec });
    Ok((discovered.id, discovered.actions))
  }

  fn finish_script_execution(&mut self, script_id: ScriptId) -> Result<()> {
    self.executed.insert(script_id);
    if self.parser_blocked_on == Some(script_id) {
      self.parser_blocked_on = None;
    }
    Ok(())
  }

  fn execute_script(
    &mut self,
    host: &mut WindowHostState,
    event_loop: &mut EventLoop<WindowHostState>,
    script_id: ScriptId,
    source_text: &str,
  ) -> Result<()> {
    if self.executed.contains(&script_id) {
      return Ok(());
    }

    let Some(entry) = self.scripts.get(&script_id).cloned() else {
      return Err(Error::Other(format!(
        "ScriptScheduler requested execution for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    struct Adapter<'a> {
      source_name: Arc<str>,
      source_text: &'a str,
      event_loop: &'a mut EventLoop<WindowHostState>,
    }

    impl ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
      fn execute_script(
        &mut self,
        host: &mut WindowHostState,
        _orchestrator: &mut ScriptOrchestrator,
        _script: NodeId,
        script_type: ScriptType,
      ) -> Result<()> {
        if script_type != ScriptType::Classic {
          return Ok(());
        }
        host
          .exec_script_with_name_in_event_loop(
            self.event_loop,
            self.source_name.clone(),
            self.source_text,
          )
          .map(|_value| ())
      }
    }

    let source_name: Arc<str> = entry
      .spec
      .src
      .clone()
      .unwrap_or_else(|| "inline".to_string())
      .into();
    let mut adapter = Adapter {
      source_name,
      source_text,
      event_loop,
    };

    // Avoid double-borrowing `self` by temporarily moving the orchestrator out.
    let mut orchestrator = std::mem::take(&mut self.orchestrator);
    let result = orchestrator.execute_script_element(
      host,
      entry.node_id,
      entry.spec.script_type,
      &mut adapter,
    );
    self.orchestrator = orchestrator;
    result
  }
}

type SharedState = Rc<std::cell::RefCell<SchedulerState>>;

fn apply_scheduler_actions(
  state: &SharedState,
  host: &mut WindowHostState,
  event_loop: &mut EventLoop<WindowHostState>,
  actions: Vec<ScriptSchedulerAction<NodeId>>,
) -> Result<()> {
  for action in actions {
    match action {
      ScriptSchedulerAction::StartFetch { script_id, url, .. } => {
        let (fetcher, is_blocking) = {
          let st = state.borrow();
          let entry = st.scripts.get(&script_id).ok_or_else(|| {
            Error::Other(format!(
              "ScriptScheduler requested fetch for unknown script_id={}",
              script_id.as_u64()
            ))
          })?;
          let is_blocking =
            entry.spec.src_attr_present && !entry.spec.async_attr && !entry.spec.defer_attr;
          (Arc::clone(&st.fetcher), is_blocking)
        };

        if is_blocking {
          let source = fetch_script_source(fetcher.as_ref(), &url)?;
          let actions = {
            state
              .borrow_mut()
              .scheduler
              .fetch_completed(script_id, source)?
          };
          apply_scheduler_actions(state, host, event_loop, actions)?;
          continue;
        }

        let url_for_err = url.clone();
        let mut st = state.borrow_mut();
        if st.pending_fetches.insert(url, script_id).is_some() {
          return Err(Error::Other(format!(
            "duplicate StartFetch action for script url={url_for_err:?}"
          )));
        }
      }
      ScriptSchedulerAction::StartModuleGraphFetch { script_id, .. } => {
        return Err(Error::Other(format!(
          "browser integration harness does not support module script graphs (script_id={})",
          script_id.as_u64()
        )));
      }
      ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
        let mut st = state.borrow_mut();
        if st.executed.contains(&script_id) {
          continue;
        }
        if st
          .parser_blocked_on
          .is_some_and(|existing| existing != script_id)
        {
          return Err(Error::Other(
            "ScriptScheduler requested multiple simultaneous parser blocks".to_string(),
          ));
        }
        st.parser_blocked_on = Some(script_id);
      }
      ScriptSchedulerAction::ExecuteNow {
        script_id,
        source_text,
        ..
      } => {
        let exec_result = {
          let depth = { Rc::clone(state.borrow().js_execution_depth()) };
          let _guard = JsExecutionGuard::enter(&depth);
          state
            .borrow_mut()
            .execute_script(host, event_loop, script_id, &source_text)
        };
        state.borrow_mut().finish_script_execution(script_id)?;
        exec_result?;

        // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
        // execution context stack is empty.
        if state.borrow().js_execution_depth.get() == 0 {
          event_loop.perform_microtask_checkpoint(host)?;
        }
      }
      ScriptSchedulerAction::QueueTask {
        script_id,
        source_text,
        ..
      } => {
        let state_for_task = Rc::clone(state);
        event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
          let depth = { Rc::clone(state_for_task.borrow().js_execution_depth()) };
          let _guard = JsExecutionGuard::enter(&depth);
          let result =
            state_for_task
              .borrow_mut()
              .execute_script(host, event_loop, script_id, &source_text);
          state_for_task
            .borrow_mut()
            .finish_script_execution(script_id)?;
          result
        })?;
      }
      ScriptSchedulerAction::QueueScriptEventTask { .. } => {
        // The browser integration harness does not currently assert `<script>` load/error events; it
        // only cares about script execution ordering and DOM side effects.
      }
    }
  }
  Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpOutcome {
  NeedMoreInput,
  Finished,
}

struct JsFixtureHarness {
  document_url: String,
  parser: StreamingHtmlParser,
  host: WindowHostState,
  state: SharedState,
  event_loop: EventLoop<WindowHostState>,
  parsing_finished: bool,
}

impl JsFixtureHarness {
  fn from_fixture(name: &str) -> Result<Self> {
    let fixture_url = file_url_for_path(&fixture_path(name))?;
    let parser = StreamingHtmlParser::new(Some(&fixture_url));
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(FileResourceFetcher::default());

    // The host DOM is overwritten with snapshots from the streaming parser as parsing progresses.
    // It only exists to seed the `WindowHostState` with a stable DOM backing store.
    let host = WindowHostState::new_with_fetcher(
      Document::new(QuirksMode::NoQuirks),
      fixture_url.clone(),
      fetcher.clone(),
    )?;

    let clock = Arc::new(VirtualClock::new());
    let clock_for_loop: Arc<dyn Clock> = clock;
    let event_loop = EventLoop::with_clock(clock_for_loop);

    Ok(Self {
      document_url: fixture_url,
      parser,
      host,
      state: Rc::new(std::cell::RefCell::new(SchedulerState::new(fetcher))),
      event_loop,
      parsing_finished: false,
    })
  }

  fn document_url(&self) -> &str {
    &self.document_url
  }

  fn push_str(&mut self, chunk: &str) {
    self.parser.push_str(chunk);
  }

  fn set_eof(&mut self) {
    self.parser.set_eof();
  }

  fn complete_url(&mut self, url: &str) -> Result<()> {
    // Async/defer fetch completion can happen while parsing is still in progress. Mirror the
    // production pipeline by ensuring the JS host sees the latest parser DOM snapshot before
    // scheduling any resulting script execution tasks.
    self.sync_host_dom_from_parser()?;

    let script_id = {
      let mut st = self.state.borrow_mut();
      st.pending_fetches.remove(url).ok_or_else(|| {
        Error::Other(format!(
          "attempted to complete unknown script fetch url={url:?} (was it already completed?)"
        ))
      })?
    };

    let source = {
      let fetcher = { Arc::clone(&self.state.borrow().fetcher) };
      fetch_script_source(fetcher.as_ref(), url)?
    };

    let actions = {
      self
        .state
        .borrow_mut()
        .scheduler
        .fetch_completed(script_id, source)?
    };
    apply_scheduler_actions(&self.state, &mut self.host, &mut self.event_loop, actions)?;

    // If parsing is still ongoing, propagate any DOM mutations back into the streaming parser's
    // document so subsequent parsing steps see the updated tree.
    self.sync_parser_dom_from_host()?;
    Ok(())
  }

  fn sync_host_dom_from_parser(&mut self) -> Result<()> {
    if self.parsing_finished {
      return Ok(());
    }
    let Some(doc) = self.parser.document() else {
      return Ok(());
    };
    *self.host.dom_mut() = doc.clone_with_events();
    Ok(())
  }

  fn sync_parser_dom_from_host(&mut self) -> Result<()> {
    if self.parsing_finished {
      return Ok(());
    }
    let Some(mut doc) = self.parser.document_mut() else {
      return Ok(());
    };
    *doc = self.host.dom().clone_with_events();
    Ok(())
  }

  fn on_script_boundary(
    &mut self,
    script: NodeId,
    base_url_at_this_point: Option<String>,
  ) -> Result<()> {
    let snapshot = {
      let Some(doc) = self.parser.document() else {
        return Err(Error::Other(
          "StreamingHtmlParser yielded a script without an active document".to_string(),
        ));
      };
      doc.clone_with_events()
    };
    *self.host.dom_mut() = snapshot;

    // HTML: before executing a parser-inserted script at a script end-tag boundary, perform a
    // microtask checkpoint when the JS execution context stack is empty.
    if self.state.borrow().js_execution_depth.get() == 0 {
      self
        .event_loop
        .perform_microtask_checkpoint(&mut self.host)?;
    }

    let spec = {
      let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
      build_parser_inserted_script_element_spec_dom2(self.host.dom(), script, &base)
    };

    let actions = {
      let mut st = self.state.borrow_mut();
      let (_id, actions) = st.register_script(script, spec)?;
      actions
    };
    apply_scheduler_actions(&self.state, &mut self.host, &mut self.event_loop, actions)?;

    self.sync_parser_dom_from_host()?;
    Ok(())
  }

  fn pump_until_stalled(&mut self) -> Result<PumpOutcome> {
    loop {
      match self.parser.pump()? {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          self.on_script_boundary(script, base_url_at_this_point)?;
          continue;
        }
        StreamingParserYield::NeedMoreInput => return Ok(PumpOutcome::NeedMoreInput),
        StreamingParserYield::Finished { document } => {
          *self.host.dom_mut() = document;
          self.parsing_finished = true;
          return Ok(PumpOutcome::Finished);
        }
      }
    }
  }

  fn pump_to_completion(&mut self) -> Result<()> {
    loop {
      match self.pump_until_stalled()? {
        PumpOutcome::NeedMoreInput => {
          return Err(Error::Other(
            "unexpected NeedMoreInput while pumping with EOF set".to_string(),
          ));
        }
        PumpOutcome::Finished => return Ok(()),
      }
    }
  }

  fn finish_parsing(&mut self) -> Result<()> {
    if !self.parsing_finished {
      return Err(Error::Other(
        "finish_parsing called before parser reached EOF".to_string(),
      ));
    }
    let actions = { self.state.borrow_mut().scheduler.parsing_completed()? };
    apply_scheduler_actions(&self.state, &mut self.host, &mut self.event_loop, actions)?;
    Ok(())
  }

  fn run_event_loop_until_idle(&mut self) -> Result<RunUntilIdleOutcome> {
    self.sync_host_dom_from_parser()?;
    let outcome = self.event_loop.run_until_idle(
      &mut self.host,
      RunLimits {
        max_tasks: 128,
        max_microtasks: 1024,
        max_wall_time: None,
      },
    )?;
    self.sync_parser_dom_from_host()?;
    Ok(outcome)
  }

  fn render(&self, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
    let mut renderer = offline_renderer()?;
    let dom = self.host.dom().to_renderer_dom();
    renderer.render_dom_with_options(&dom, options)
  }

  fn root_class(&self) -> Option<String> {
    let doc = self.host.dom();
    let root = doc.get_element_by_id("root")?;
    doc
      .get_attribute(root, "class")
      .ok()
      .flatten()
      .map(|s| s.to_string())
  }
}

#[test]
fn js_inline_script_mutation_affects_render() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("inline_mutation.html")?;
  let html = read_fixture("inline_mutation.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;

  harness.finish_parsing()?;
  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.root_class().as_deref(), Some("on"));

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("inline_mutation_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "inline script should mutate DOM and affect final pixels"
  );
  Ok(())
}

#[test]
fn js_external_defer_scripts_execute_in_order_after_parsing() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("external_defer.html")?;
  let html = read_fixture("external_defer.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;

  let doc_url = Url::parse(harness.document_url()).expect("fixture URL should parse");
  let defer1_url = doc_url
    .join("assets/defer1.js")
    .expect("resolve defer1")
    .to_string();
  let defer2_url = doc_url
    .join("assets/defer2.js")
    .expect("resolve defer2")
    .to_string();

  // Complete out of order to ensure `defer` still executes in document order.
  harness.complete_url(&defer2_url)?;
  harness.complete_url(&defer1_url)?;

  // Fetch completes, but defer scripts must not execute until parsing completion is signalled.
  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    harness.root_class().as_deref(),
    Some("off"),
    "defer scripts must not execute before parsing is marked finished"
  );

  harness.finish_parsing()?;
  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(harness.root_class().as_deref(), Some("step2"));

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("external_defer_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "defer scripts should run after parsing and in document order"
  );
  Ok(())
}

#[test]
fn js_external_async_script_runs_without_waiting_for_parsing_complete() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("external_async.html")?;
  let html = read_fixture("external_async.html")?;
  let marker = "<div style=\"display:none\">padding</div>";
  let (first, second) = html
    .split_once(marker)
    .ok_or_else(|| Error::Other("async fixture missing chunk marker".to_string()))?;

  harness.push_str(first);
  match harness.pump_until_stalled()? {
    PumpOutcome::NeedMoreInput => {}
    PumpOutcome::Finished => {
      return Err(Error::Other(
        "async fixture unexpectedly finished parsing before async load completed".to_string(),
      ));
    }
  }

  let doc_url = Url::parse(harness.document_url()).expect("fixture URL should parse");
  let async_url = doc_url
    .join("assets/async.js")
    .expect("resolve async")
    .to_string();
  harness.complete_url(&async_url)?;

  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    harness.root_class().as_deref(),
    Some("on"),
    "async scripts should be able to mutate the document before parsing completes"
  );

  harness.push_str(marker);
  harness.push_str(second);
  harness.set_eof();
  harness.pump_to_completion()?;

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("external_async_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "async script should mutate DOM even before parsing_completed"
  );

  harness.finish_parsing()?;
  Ok(())
}

#[test]
fn js_base_url_timing_script_before_base_href_uses_document_url() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut harness = JsFixtureHarness::from_fixture("base_url_timing.html")?;
  let html = read_fixture("base_url_timing.html")?;
  harness.push_str(&html);
  harness.set_eof();
  harness.pump_to_completion()?;
  harness.finish_parsing()?;

  assert_eq!(
    harness.run_event_loop_until_idle()?,
    RunUntilIdleOutcome::Idle
  );

  let actual = harness.render(options.clone())?;
  let expected = render_static_fixture("base_url_timing_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "script before <base href> should resolve against document URL and affect pixels"
  );
  Ok(())
}
