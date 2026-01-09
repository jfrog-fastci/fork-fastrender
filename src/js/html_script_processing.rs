//! Classic HTML `<script>` processing wired to the streaming `dom2` parser.
//!
//! This module is the glue between:
//! - [`crate::html::streaming_parser::StreamingHtmlParser`] (pause at `</script>`),
//! - [`super::script_scheduler::ScriptScheduler`] (classic script ordering model),
//! - [`super::event_loop::EventLoop`] (tasks + microtasks),
//! - [`super::orchestrator::ScriptOrchestrator`] (`Document.currentScript` bookkeeping).
//!
//! Today this is intentionally scoped to **classic scripts** and is designed as a deterministic,
//! unit-testable harness. It does **not** yet attempt to run async scripts *during* parsing (it
//! queues their execution to run after parsing completes). This keeps the DOM borrowing model
//! simple while still exercising the spec-shaped scheduler and event loop semantics.

use crate::dom2::{Document, NodeId};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};

use super::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};
use super::orchestrator::{CurrentScriptHost, ScriptBlockExecutor, ScriptOrchestrator};
use super::script_scheduler::{ScriptId, ScriptLoader, ScriptScheduler, ScriptSchedulerAction};
use super::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use super::{DomHost, ScriptExecutionLog};
use super::ScriptType;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

trait InnerHostAccess<Host> {
  fn inner(&self) -> &Host;
  fn inner_mut(&mut self) -> &mut Host;
}

struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl JsExecutionGuard {
  fn enter(depth: &Rc<Cell<usize>>) -> Self {
    let cur = depth.get();
    depth.set(cur + 1);
    Self {
      depth: Rc::clone(depth),
    }
  }
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let cur = self.depth.get();
    self
      .depth
      .set(cur.checked_sub(1).expect("js execution depth underflow"));
  }
}

struct ParserHost<'a, Host> {
  inner: &'a mut Host,
  parser: &'a StreamingHtmlParser,
}

impl<'a, Host> InnerHostAccess<Host> for ParserHost<'a, Host> {
  fn inner(&self) -> &Host {
    &*self.inner
  }

  fn inner_mut(&mut self) -> &mut Host {
    &mut *self.inner
  }
}

impl<'a, Host: CurrentScriptHost> CurrentScriptHost for ParserHost<'a, Host> {
  fn current_script_state(&self) -> &super::orchestrator::CurrentScriptStateHandle {
    self.inner.current_script_state()
  }

  fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    self.inner.script_execution_log()
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
    self.inner.script_execution_log_mut()
  }
}

impl<'a, Host> DomHost for ParserHost<'a, Host> {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    let dom = self.parser.document();
    f(&dom)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    let mut dom = self.parser.document_mut();
    let (result, _changed) = f(&mut dom);
    result
  }
}

struct RcDomHost<'a, Host> {
  inner: &'a mut Host,
  dom: Rc<RefCell<Document>>,
}

impl<'a, Host> InnerHostAccess<Host> for RcDomHost<'a, Host> {
  fn inner(&self) -> &Host {
    &*self.inner
  }

  fn inner_mut(&mut self) -> &mut Host {
    &mut *self.inner
  }
}

impl<'a, Host: CurrentScriptHost> CurrentScriptHost for RcDomHost<'a, Host> {
  fn current_script_state(&self) -> &super::orchestrator::CurrentScriptStateHandle {
    self.inner.current_script_state()
  }

  fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    self.inner.script_execution_log()
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
    self.inner.script_execution_log_mut()
  }
}

impl<'a, Host> DomHost for RcDomHost<'a, Host> {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    let dom = self.dom.borrow();
    f(&dom)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    let mut dom = self.dom.borrow_mut();
    let (result, _changed) = f(&mut dom);
    result
  }
}

struct ScriptRunnerExecutor<'a, Host: 'static, Runner>
where
  Runner:
    Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  runner: Rc<Runner>,
  event_loop: &'a mut EventLoop<Host>,
  source_text: &'a str,
  dom: &'a Document,
}

impl<'a, Host: 'static, Runner, HostWrapper> ScriptBlockExecutor<HostWrapper>
  for ScriptRunnerExecutor<'a, Host, Runner>
where
  HostWrapper: CurrentScriptHost + InnerHostAccess<Host>,
  Runner:
    Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  fn execute_script(
    &mut self,
    host: &mut HostWrapper,
    _orchestrator: &mut ScriptOrchestrator,
    script: NodeId,
    script_type: ScriptType,
  ) -> Result<()> {
    (self.runner)(
      host.inner_mut(),
      self.dom,
      script,
      script_type,
      self.source_text,
      self.event_loop,
    )
  }
}

fn execute_now<Host, Runner, HostWrapper>(
  host: &mut HostWrapper,
  dom: &Document,
  script: NodeId,
  source_text: &str,
  event_loop: &mut EventLoop<Host>,
  runner: Rc<Runner>,
  js_execution_depth: &Rc<Cell<usize>>,
) -> Result<()>
where
  HostWrapper: CurrentScriptHost + DomHost + InnerHostAccess<Host>,
  Runner:
    Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  // HTML `</script>` handling performs a microtask checkpoint *before* preparing/executing a
  // parser-inserted script when the JS execution context stack is empty.
  if js_execution_depth.get() == 0 {
    event_loop.perform_microtask_checkpoint(host.inner_mut())?;
  }

  let mut orchestrator = ScriptOrchestrator::new();
  let mut exec = ScriptRunnerExecutor {
    runner,
    event_loop,
    source_text,
    dom,
  };
  {
    let _guard = JsExecutionGuard::enter(js_execution_depth);
    orchestrator.execute_script_element(host, script, ScriptType::Classic, &mut exec)?;
  }

  // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
  // execution context stack is empty. Nested (re-entrant) script execution must not drain
  // microtasks until the outermost script returns.
  if js_execution_depth.get() == 0 {
    event_loop.perform_microtask_checkpoint(host.inner_mut())?;
  }
  Ok(())
}

fn apply_actions<Host, HostWrapper, Loader, Runner>(
  scheduler: &mut ScriptScheduler<NodeId>,
  host: &mut HostWrapper,
  dom: &Document,
  loader: &mut Loader,
  pending_fetches: &mut HashMap<Loader::Handle, ScriptId>,
  queued_task_scripts: &mut Vec<(NodeId, String)>,
  event_loop: &mut EventLoop<Host>,
  runner: &Rc<Runner>,
  js_execution_depth: &Rc<Cell<usize>>,
  actions: Vec<ScriptSchedulerAction<NodeId>>,
) -> Result<()>
where
  HostWrapper: CurrentScriptHost + DomHost + InnerHostAccess<Host>,
  Loader: ScriptLoader,
  Runner:
    Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  let mut start_fetches: Vec<(ScriptId, String)> = Vec::new();
  let mut blocking: HashSet<ScriptId> = HashSet::new();

  for action in actions {
    match action {
      ScriptSchedulerAction::StartFetch { script_id, url, .. } => {
        start_fetches.push((script_id, url));
      }
      ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
        blocking.insert(script_id);
      }
      ScriptSchedulerAction::ExecuteNow {
        node_id,
        source_text,
        ..
      } => {
        execute_now(
          host,
          dom,
          node_id,
          &source_text,
          event_loop,
          Rc::clone(runner),
          js_execution_depth,
        )?;
      }
      ScriptSchedulerAction::QueueTask {
        node_id,
        source_text,
        ..
      } => {
        queued_task_scripts.push((node_id, source_text));
      }
    }
  }

  for (script_id, url) in start_fetches {
    if blocking.contains(&script_id) {
      let source_text = loader.load_blocking(&url)?;
      let actions = scheduler.fetch_completed(script_id, source_text)?;
      apply_actions(
        scheduler,
        host,
        dom,
        loader,
        pending_fetches,
        queued_task_scripts,
        event_loop,
        runner,
        js_execution_depth,
        actions,
      )?;
    } else {
      let handle = loader.start_load(&url)?;
      if pending_fetches.insert(handle, script_id).is_some() {
        return Err(Error::Other(format!(
          "Script loader returned duplicate handle {handle:?} for url={url}"
        )));
      }
    }
  }

  Ok(())
}

fn poll_fetch_completions<Host, HostWrapper, Loader, Runner>(
  scheduler: &mut ScriptScheduler<NodeId>,
  host: &mut HostWrapper,
  dom: &Document,
  loader: &mut Loader,
  pending_fetches: &mut HashMap<Loader::Handle, ScriptId>,
  queued_task_scripts: &mut Vec<(NodeId, String)>,
  event_loop: &mut EventLoop<Host>,
  runner: &Rc<Runner>,
  js_execution_depth: &Rc<Cell<usize>>,
) -> Result<()>
where
  HostWrapper: CurrentScriptHost + DomHost + InnerHostAccess<Host>,
  Loader: ScriptLoader,
  Runner:
    Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  while let Some((handle, source_text)) = loader.poll_complete()? {
    let Some(script_id) = pending_fetches.remove(&handle) else {
      return Err(Error::Other(format!(
        "Script loader returned completion for unknown handle: {handle:?}"
      )));
    };
    let actions = scheduler.fetch_completed(script_id, source_text)?;
    apply_actions(
      scheduler,
      host,
      dom,
      loader,
      pending_fetches,
      queued_task_scripts,
      event_loop,
      runner,
      js_execution_depth,
      actions,
    )?;
  }
  Ok(())
}

/// Parse HTML with a streaming `dom2` parser and execute classic scripts using the HTML ordering
/// model.
///
/// This function is intended as an early, testable integration point for `<script>` processing.
///
/// Current limitations:
/// - Only classic scripts are executed (`type="module"` and `importmap` are ignored).
/// - Async scripts are queued to run after parsing completes (no mid-parse interruption yet).
///
/// Returns the final parsed document and the outcome of running the queued event-loop work.
pub fn parse_html_with_classic_script_processing<Host, Loader, Runner>(
  html: &str,
  document_url: Option<&str>,
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  loader: &mut Loader,
  limits: RunLimits,
  runner: Runner,
) -> Result<(Document, RunUntilIdleOutcome)>
where
  Host: CurrentScriptHost + 'static,
  Loader: ScriptLoader,
  Runner: Fn(&mut Host, &Document, NodeId, ScriptType, &str, &mut EventLoop<Host>) -> Result<()> + 'static,
{
  let runner = Rc::new(runner);
  let mut parser = StreamingHtmlParser::new(document_url);
  parser.push_str(html);
  parser.set_eof();

  let mut scheduler = ScriptScheduler::<NodeId>::new();
  let mut pending_fetches: HashMap<Loader::Handle, ScriptId> = HashMap::new();
  let mut queued_task_scripts: Vec<(NodeId, String)> = Vec::new();
  let js_execution_depth = Rc::new(Cell::new(0));

  let document = loop {
    match parser.pump() {
      StreamingParserYield::Script {
        script,
        base_url_at_this_point,
      } => {
        // HTML `</script>` handling performs a microtask checkpoint *before* preparing the script,
        // but only when the JS execution context stack is empty.
        if js_execution_depth.get() == 0 {
          event_loop.perform_microtask_checkpoint(host)?;
        }

        let doc = parser.document();
        let base_tracker = BaseUrlTracker::new(base_url_at_this_point.as_deref());
        let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base_tracker);
        let discovered =
          scheduler.discovered_parser_script(spec, script, base_url_at_this_point)?;
        let mut host_wrapper = ParserHost {
          inner: host,
          parser: &parser,
        };
        apply_actions(
          &mut scheduler,
          &mut host_wrapper,
          &doc,
          loader,
          &mut pending_fetches,
          &mut queued_task_scripts,
          event_loop,
          &runner,
          &js_execution_depth,
          discovered.actions,
        )?;
      }
      StreamingParserYield::NeedMoreInput => {
        return Err(Error::Other(
          "StreamingHtmlParser unexpectedly requested more input after EOF".to_string(),
        ));
      }
      StreamingParserYield::Finished { document } => break document,
    }
  };

  let dom_rc = Rc::new(RefCell::new(document));

  // End-of-parsing hook: allows defer scripts to start queuing once their fetch completes.
  let actions = scheduler.parsing_completed()?;
  {
    let dom = dom_rc.borrow();
    let mut host_wrapper = RcDomHost {
      inner: host,
      dom: Rc::clone(&dom_rc),
    };
    apply_actions(
      &mut scheduler,
      &mut host_wrapper,
      &dom,
      loader,
      &mut pending_fetches,
      &mut queued_task_scripts,
      event_loop,
      &runner,
      &js_execution_depth,
      actions,
    )?;

    // Drain all pending fetch completions (async/defer externals) now that parsing is complete.
    poll_fetch_completions(
      &mut scheduler,
      &mut host_wrapper,
      &dom,
      loader,
      &mut pending_fetches,
      &mut queued_task_scripts,
      event_loop,
      &runner,
      &js_execution_depth,
    )?;
  }

  // Queue any script-execution tasks now that we have a stable document snapshot to use for
  // currentScript bookkeeping.
  for (node_id, source_text) in queued_task_scripts.drain(..) {
    let dom = Rc::clone(&dom_rc);
    let runner = Rc::clone(&runner);
    let js_execution_depth = Rc::clone(&js_execution_depth);
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let dom_for_host = Rc::clone(&dom);
      let dom_ref = dom.borrow();
      let mut host_wrapper = RcDomHost {
        inner: host,
        dom: dom_for_host,
      };
      let mut orchestrator = ScriptOrchestrator::new();
      let mut exec = ScriptRunnerExecutor {
        runner,
        event_loop,
        source_text: &source_text,
        dom: &dom_ref,
      };
      let _guard = JsExecutionGuard::enter(&js_execution_depth);
      orchestrator.execute_script_element(&mut host_wrapper, node_id, ScriptType::Classic, &mut exec)?;
      Ok(())
    })?;
  }

  let outcome = event_loop.run_until_idle(host, limits)?;
  let document = Rc::try_unwrap(dom_rc)
    .map(|cell| cell.into_inner())
    .unwrap_or_else(|rc| rc.borrow().clone());
  Ok((document, outcome))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::orchestrator::CurrentScriptStateHandle;
  use crate::js::RunUntilIdleOutcome;

  #[derive(Default)]
  struct Host {
    script_state: CurrentScriptStateHandle,
    log: Vec<String>,
  }

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.script_state
    }
  }

  #[derive(Default)]
  struct ManualLoader {
    next_handle: usize,
    // url -> source text
    sources: HashMap<String, String>,
    // url completion order for poll_complete()
    completion_plan: Vec<String>,
    started: Vec<String>,
    handle_by_url: HashMap<String, usize>,
    completion_queue: Vec<(usize, String)>,
    completions_built: bool,
    call_log: Option<Rc<RefCell<Vec<String>>>>,
  }

  impl ManualLoader {
    fn with_sources(mut self, sources: &[(&str, &str)]) -> Self {
      for (url, source) in sources {
        self.sources.insert((*url).to_string(), (*source).to_string());
      }
      self
    }

    fn with_completion_plan(mut self, urls: &[&str]) -> Self {
      self.completion_plan = urls.iter().map(|u| (*u).to_string()).collect();
      self
    }

    fn with_call_log(mut self, log: Rc<RefCell<Vec<String>>>) -> Self {
      self.call_log = Some(log);
      self
    }
  }

  impl ScriptLoader for ManualLoader {
    type Handle = usize;

    fn load_blocking(&mut self, url: &str) -> Result<String> {
      self.started.push(url.to_string());
      if let Some(log) = &self.call_log {
        log.borrow_mut()
          .push(format!("load_blocking:{url}"));
      }
      self
        .sources
        .get(url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("no script source for blocking url={url}")))
    }

    fn start_load(&mut self, url: &str) -> Result<Self::Handle> {
      self.started.push(url.to_string());
      if let Some(log) = &self.call_log {
        log.borrow_mut()
          .push(format!("start_load:{url}"));
      }
      let handle = self.next_handle;
      self.next_handle += 1;
      self.handle_by_url.insert(url.to_string(), handle);
      Ok(handle)
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      if !self.completions_built {
        self.completions_built = true;
        for url in self.completion_plan.clone() {
          let Some(handle) = self.handle_by_url.get(&url).copied() else {
            continue;
          };
          let Some(source) = self.sources.get(&url).cloned() else {
            return Err(Error::Other(format!("no script source for url={url}")));
          };
          self.completion_queue.push((handle, source));
        }
      }
      if self.completion_queue.is_empty() {
        return Ok(None);
      }
      Ok(Some(self.completion_queue.remove(0)))
    }
  }

  fn run_driver(
    html: &str,
    document_url: Option<&str>,
    loader: &mut ManualLoader,
    host: &mut Host,
  ) -> Result<(Document, RunUntilIdleOutcome)> {
    let mut event_loop = EventLoop::<Host>::new();
    parse_html_with_classic_script_processing(
      html,
      document_url,
      host,
      &mut event_loop,
      loader,
      RunLimits::unbounded(),
      |host, _dom, script, _ty, source, event_loop| {
        host.log.push(format!("script:{source}"));
        let micro = format!("microtask:{source}");
        let expected_current = Some(script);
        assert_eq!(
          host.current_script(),
          expected_current,
          "expected currentScript to be set while classic script executes"
        );
        event_loop.queue_microtask(move |host, _| {
          host.log.push(micro);
          Ok(())
        })?;
        Ok(())
      },
    )
  }

  #[test]
  fn microtasks_flush_after_each_parser_blocking_inline_script() -> Result<()> {
    let html = "<!doctype html><script>a</script><script>b</script>";
    let mut loader = ManualLoader::default();
    let mut host = Host::default();

    let (_doc, outcome) = run_driver(html, None, &mut loader, &mut host)?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    assert_eq!(
      host.log,
      vec![
        "script:a".to_string(),
        "microtask:a".to_string(),
        "script:b".to_string(),
        "microtask:b".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn microtasks_checkpoint_before_parser_blocking_script_when_invoked_from_task() -> Result<()> {
    // HTML performs a microtask checkpoint before preparing a parser-inserted script when the JS
    // execution context stack is empty. This must be independent of whether parsing is happening
    // inside an event loop task.
    let mut dom = Document::new(selectors::context::QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom.append_child(dom.root(), script).expect("append_child");
    let dom = Rc::new(RefCell::new(dom));

    let js_execution_depth = Rc::new(Cell::new(0));
    let runner = Rc::new(
      |host: &mut Host,
       _dom: &Document,
       _script: NodeId,
       _ty: ScriptType,
       source: &str,
       _event_loop: &mut EventLoop<Host>| {
        host.log.push(format!("script:{source}"));
        Ok(())
      },
    );

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_microtask(|host, _| {
      host.log.push("microtask".to_string());
      Ok(())
    })?;

    let dom_for_task = Rc::clone(&dom);
    let runner = Rc::clone(&runner);
    let js_execution_depth_for_task = Rc::clone(&js_execution_depth);
    event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      let dom_ref = dom_for_task.borrow();
      let dom_for_host = Rc::clone(&dom_for_task);
      let mut host_wrapper = RcDomHost {
        inner: host,
        dom: dom_for_host,
      };
      execute_now(
        &mut host_wrapper,
        &dom_ref,
        script,
        "a",
        event_loop,
        runner,
        &js_execution_depth_for_task,
      )?;
      Ok(())
    })?;

    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      host.log,
      vec!["microtask".to_string(), "script:a".to_string()]
    );
    Ok(())
  }

  #[test]
  fn microtasks_do_not_drain_during_nested_script_execution() -> Result<()> {
    // HTML runs microtask checkpoints before and after executing a parser-inserted script only when
    // the JS execution context stack is empty. When script execution is re-entrant, inner script
    // execution must not drain microtasks until the outermost script returns.
    let mut doc = Document::new(selectors::context::QuirksMode::NoQuirks);
    let html = doc.create_element("html", "");
    doc.append_child(doc.root(), html).expect("append_child");
    let script_a = doc.create_element("script", "");
    let script_b = doc.create_element("script", "");
    doc.append_child(html, script_a).expect("append_child");
    doc.append_child(html, script_b).expect("append_child");
    let dom = Rc::new(RefCell::new(doc));

    let js_execution_depth = Rc::new(Cell::new(0));
    let runner_b = Rc::new(
      |host: &mut Host,
       _dom: &Document,
       _script: NodeId,
       _ty: ScriptType,
       source: &str,
       event_loop: &mut EventLoop<Host>| {
        assert_eq!(source, "b");
        host.log.push("script:b".to_string());
        event_loop.queue_microtask(|host, _| {
          host.log.push("microtask:b".to_string());
          Ok(())
        })?;
        Ok(())
      },
    );

    let runner_a = {
      let dom = Rc::clone(&dom);
      let js_execution_depth = Rc::clone(&js_execution_depth);
      let runner_b = Rc::clone(&runner_b);
      Rc::new(
        move |host: &mut Host,
              _dom: &Document,
              _script: NodeId,
              _ty: ScriptType,
              source: &str,
              event_loop: &mut EventLoop<Host>| {
          assert_eq!(source, "a");
          host.log.push("script:a:before".to_string());
          event_loop.queue_microtask(|host, _| {
            host.log.push("microtask:a".to_string());
            Ok(())
          })?;

          // Nested script execution should not run microtasks even if it invokes `execute_now`.
          {
            let dom_ref = dom.borrow();
            let dom_for_host = Rc::clone(&dom);
            let mut host_wrapper = RcDomHost {
              inner: host,
              dom: dom_for_host,
            };
            execute_now(
              &mut host_wrapper,
              &dom_ref,
              script_b,
              "b",
              event_loop,
              Rc::clone(&runner_b),
              &js_execution_depth,
            )?;
          }

          host.log.push("script:a:after".to_string());
          Ok(())
        },
      )
    };

    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();
    let dom_ref = dom.borrow();
    let dom_for_host = Rc::clone(&dom);
    let mut host_wrapper = RcDomHost {
      inner: &mut host,
      dom: dom_for_host,
    };
    execute_now(
      &mut host_wrapper,
      &dom_ref,
      script_a,
      "a",
      &mut event_loop,
      runner_a,
      &js_execution_depth,
    )?;

    assert_eq!(
      host.log,
      vec![
        "script:a:before".to_string(),
        "script:b".to_string(),
        "script:a:after".to_string(),
        "microtask:a".to_string(),
        "microtask:b".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn parser_pauses_at_script_end_before_parsing_later_markup() -> Result<()> {
    let html = "<!doctype html><script>a</script><div id=after></div>";
    let mut loader = ManualLoader::default();
    let mut host = Host::default();

    let mut event_loop = EventLoop::<Host>::new();
    let (doc, _outcome) = parse_html_with_classic_script_processing(
      html,
      None,
      &mut host,
      &mut event_loop,
      &mut loader,
      RunLimits::unbounded(),
      |host, dom, _script, _ty, source, event_loop| {
        if source == "a" {
          let after = dom.get_element_by_id("after");
          assert!(
            after.is_none(),
            "expected <div id=after> not to exist while first <script> executes"
          );
        }
        host.log.push(format!("script:{source}"));
        event_loop.queue_microtask(|_host, _| Ok(()))?;
        Ok(())
      },
    )?;

    assert!(doc.get_element_by_id("after").is_some());
    Ok(())
  }

  #[test]
  fn blocking_external_script_executes_before_later_inline_script() -> Result<()> {
    let html = "<!doctype html><script src=\"https://example.com/a.js\"></script><script>b</script>";
    let mut loader = ManualLoader::default().with_sources(&[("https://example.com/a.js", "ext-a")]);
    let mut host = Host::default();

    let (_doc, outcome) = run_driver(html, None, &mut loader, &mut host)?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    assert_eq!(
      host.log,
      vec![
        "script:ext-a".to_string(),
        "microtask:ext-a".to_string(),
        "script:b".to_string(),
        "microtask:b".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn async_and_defer_external_scripts_queue_in_spec_order_after_parsing() -> Result<()> {
    let html = concat!(
      "<!doctype html>",
      "<script src=\"d1.js\" defer></script>",
      "<script src=\"a1.js\" async></script>",
      "<script src=\"d2.js\" defer></script>",
      "<script src=\"a2.js\" async></script>",
    );
    let mut loader = ManualLoader::default()
      .with_sources(&[
        ("d1.js", "d1"),
        ("d2.js", "d2"),
        ("a1.js", "a1"),
        ("a2.js", "a2"),
      ])
      // Completion order chosen to interleave async scripts with deferred scripts.
      .with_completion_plan(&["d1.js", "a2.js", "d2.js", "a1.js"]);
    let mut host = Host::default();

    let (_doc, outcome) = run_driver(html, None, &mut loader, &mut host)?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    assert_eq!(
      host.log,
      vec![
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "script:a2".to_string(),
        "microtask:a2".to_string(),
        "script:d2".to_string(),
        "microtask:d2".to_string(),
        "script:a1".to_string(),
        "microtask:a1".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn microtasks_run_before_starting_blocking_external_script_fetch_at_script_end_boundary() -> Result<()> {
    let html = "<!doctype html><script src=\"https://example.com/a.js\"></script>";
    let call_log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut loader = ManualLoader::default()
      .with_sources(&[("https://example.com/a.js", "ext-a")])
      .with_call_log(Rc::clone(&call_log));
    let mut host = Host::default();
    let mut event_loop = EventLoop::<Host>::new();

    // Queue a microtask *before* parsing begins. HTML requires it to run at the `</script>`
    // boundary *before* the external script is prepared (and thus before the blocking fetch
    // starts).
    let call_log_for_microtask = Rc::clone(&call_log);
    event_loop.queue_microtask(move |_host, _event_loop| {
      call_log_for_microtask
        .borrow_mut()
        .push("microtask".to_string());
      Ok(())
    })?;

    let (_doc, outcome) = parse_html_with_classic_script_processing(
      html,
      None,
      &mut host,
      &mut event_loop,
      &mut loader,
      RunLimits::unbounded(),
      |host, _dom, _script, _ty, source, _event_loop| {
        host.log.push(format!("script:{source}"));
        Ok(())
      },
    )?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);
    assert_eq!(host.log, vec!["script:ext-a".to_string()]);

    assert_eq!(
      &*call_log.borrow(),
      &[
        "microtask".to_string(),
        "load_blocking:https://example.com/a.js".to_string(),
      ]
    );
    Ok(())
  }
}
