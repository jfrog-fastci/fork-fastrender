use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use fastrender::api::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions,
};
use fastrender::dom2::{NodeId, NodeKind};
use fastrender::error::Result;
use fastrender::js::{
  EventLoop, RunLimits, ScriptElementSpec, TaskSource, WindowRealm, WindowRealmConfig,
  WindowRealmHost,
};

struct ExecutorWithWindow<E> {
  inner: E,
  host_ctx: (),
  window: WindowRealm,
}

impl<E> ExecutorWithWindow<E> {
  fn new(inner: E) -> Self {
    let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create WindowRealm");
    Self {
      inner,
      host_ctx: (),
      window,
    }
  }
}

impl<E: BrowserTabJsExecutor> BrowserTabJsExecutor for ExecutorWithWindow<E> {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

impl<E> WindowRealmHost for ExecutorWithWindow<E> {
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
    let ExecutorWithWindow {
      host_ctx, window, ..
    } = self;
    (host_ctx, window)
  }
}

#[derive(Clone)]
struct InterleavingExecutor {
  assertions_ran: Arc<AtomicUsize>,
}

impl BrowserTabJsExecutor for InterleavingExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    if script_text.trim() != "setup" {
      return Ok(());
    }

    let assertions_ran = Arc::clone(&self.assertions_ran);
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      {
        let dom = host.dom_mut();
        let mut stack = vec![dom.root()];
        while let Some(id) = stack.pop() {
          let node = dom.node_mut(id);
          if let NodeKind::Text { content } = &mut node.kind {
            content.push_str("!");
            break;
          }
          for &child in node.children.iter().rev() {
            stack.push(child);
          }
        }
      }

      event_loop.queue_task(TaskSource::Script, move |host, _event_loop| {
        assert!(
          !host.document_is_dirty(),
          "expected BrowserTab to recompute render/layout between tasks"
        );
        assertions_ran.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })?;
      Ok(())
    })?;

    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_renders_between_tasks() -> Result<()> {
  let assertions_ran = Arc::new(AtomicUsize::new(0));
  let executor = InterleavingExecutor {
    assertions_ran: Arc::clone(&assertions_ran),
  };

  let html = "<!doctype html><html><body><div>Hello</div><script>setup</script></body></html>";
  let mut tab = BrowserTab::from_html(
    html,
    RenderOptions::new().with_viewport(32, 32),
    ExecutorWithWindow::new(executor),
  )?;

  tab.render_frame()?;

  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 8)?;
  assert!(
    matches!(
      outcome,
      fastrender::api::RunUntilStableOutcome::Stable { .. }
    ),
    "expected BrowserTab to reach a stable state; got {outcome:?}"
  );
  assert_eq!(
    assertions_ran.load(Ordering::SeqCst),
    1,
    "expected Task 2 to run exactly once"
  );
  Ok(())
}
