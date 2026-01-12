use fastrender::api::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions,
};
use fastrender::dom2::NodeId;
use fastrender::error::Result;
use fastrender::js::{
  CurrentScriptStateHandle, EventLoop, JsExecutionOptions, RunLimits, ScriptElementSpec,
  WindowRealm, WindowRealmConfig, WindowRealmHost,
};
use std::cell::RefCell;
use std::rc::Rc;

struct LogExecutor {
  log: Rc<RefCell<Vec<String>>>,
}

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

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_module_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_import_map_script(script_text, spec, current_script, document, event_loop)
  }

  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script_state: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    self.inner.reset_for_navigation(
      document_url,
      document,
      current_script_state,
      js_execution_options,
    )
  }

  fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
    if let Some(realm) = self.inner.window_realm_mut() {
      Some(realm)
    } else {
      Some(&mut self.window)
    }
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

impl BrowserTabJsExecutor for LogExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.log.borrow_mut().push(script_text.to_string());
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.log.borrow_mut().push(script_text.to_string());
    Ok(())
  }
}

#[test]
fn csp_script_blocks_external_data_url_when_disallowed() -> Result<()> {
  let log = Rc::new(RefCell::new(Vec::<String>::new()));

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta http-equiv="Content-Security-Policy" content="script-src https:">
        <script async src="data:,EXT"></script>
      </head>
    </html>"#;

  let mut tab = BrowserTab::from_html(
    html,
    RenderOptions::default(),
    ExecutorWithWindow::new(LogExecutor {
      log: Rc::clone(&log),
    }),
  )?;
  let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert!(
    log.borrow().is_empty(),
    "expected CSP to block external data: script; got log={:?}",
    log.borrow()
  );
  Ok(())
}

#[test]
fn csp_script_allows_external_data_url_when_permitted() -> Result<()> {
  let log = Rc::new(RefCell::new(Vec::<String>::new()));

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta http-equiv="Content-Security-Policy" content="script-src data:">
        <script async src="data:,EXT"></script>
      </head>
    </html>"#;

  let mut tab = BrowserTab::from_html(
    html,
    RenderOptions::default(),
    ExecutorWithWindow::new(LogExecutor {
      log: Rc::clone(&log),
    }),
  )?;
  let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert_eq!(&*log.borrow(), &["EXT".to_string()]);
  Ok(())
}

#[test]
fn csp_script_blocks_inline_without_nonce() -> Result<()> {
  let log = Rc::new(RefCell::new(Vec::<String>::new()));

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta http-equiv="Content-Security-Policy" content="script-src 'nonce-abc'">
        <script>INLINE</script>
      </head>
    </html>"#;

  let _tab = BrowserTab::from_html(
    html,
    RenderOptions::default(),
    ExecutorWithWindow::new(LogExecutor {
      log: Rc::clone(&log),
    }),
  )?;

  assert!(
    log.borrow().is_empty(),
    "expected CSP to block inline script without nonce; got log={:?}",
    log.borrow()
  );
  Ok(())
}

#[test]
fn csp_script_allows_inline_with_matching_nonce() -> Result<()> {
  let log = Rc::new(RefCell::new(Vec::<String>::new()));

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta http-equiv="Content-Security-Policy" content="script-src 'nonce-abc'">
        <script nonce="abc">INLINE</script>
      </head>
    </html>"#;

  let _tab = BrowserTab::from_html(
    html,
    RenderOptions::default(),
    ExecutorWithWindow::new(LogExecutor {
      log: Rc::clone(&log),
    }),
  )?;

  assert_eq!(&*log.borrow(), &["INLINE".to_string()]);
  Ok(())
}

#[test]
fn csp_script_allows_inline_with_matching_sha256_hash() -> Result<()> {
  use base64::{engine::general_purpose, Engine as _};
  use sha2::{Digest, Sha256};

  let log = Rc::new(RefCell::new(Vec::<String>::new()));
  let script_text = "HASHME";
  let digest = Sha256::digest(script_text.as_bytes());
  let hash = general_purpose::STANDARD.encode(digest);

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta http-equiv="Content-Security-Policy" content="script-src 'sha256-{hash}'">
          <script>{script_text}</script>
        </head>
      </html>"#
  );

  let _tab = BrowserTab::from_html(
    &html,
    RenderOptions::default(),
    ExecutorWithWindow::new(LogExecutor {
      log: Rc::clone(&log),
    }),
  )?;

  assert_eq!(&*log.borrow(), &[script_text.to_string()]);
  Ok(())
}
