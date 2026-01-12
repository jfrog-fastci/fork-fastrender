use fastrender::api::{ConsoleMessageLevel, DiagnosticsLevel};
use fastrender::{BrowserTab, RenderOptions, VmJsBrowserTabExecutor};

#[test]
fn browser_tab_records_js_exception_and_continues_rendering() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>throw "boom"</script>
        <div>ok</div>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;

  // Rendering should still succeed after the exception.
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert_eq!(diagnostics.js_exceptions.len(), 1);
  assert!(
    diagnostics.js_exceptions[0].message.contains("boom"),
    "unexpected exception message: {:?}",
    diagnostics.js_exceptions[0].message
  );
  Ok(())
}

#[test]
fn browser_tab_records_console_messages_with_levels() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>console.error("x")</script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert!(
    diagnostics
      .console_messages
      .iter()
      .any(|m| m.level == ConsoleMessageLevel::Error && m.message == "x"),
    "expected console.error('x') to be recorded; got {:?}",
    diagnostics.console_messages
  );
  Ok(())
}

#[test]
fn browser_tab_console_assert_preserves_substitution_string_formatting() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>console.assert(false, 'x=%d', 1)</script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert!(
    diagnostics.console_messages.iter().any(|m| {
      m.level == ConsoleMessageLevel::Error && m.message == "Assertion failed: x=1"
    }),
    "expected console.assert(false, 'x=%d', 1) to be recorded; got {:?}",
    diagnostics.console_messages
  );
  Ok(())
}

#[test]
fn browser_tab_exceptions_do_not_abort_subsequent_scripts() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>throw "boom"</script>
        <script>console.log("after")</script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert!(
    diagnostics
      .js_exceptions
      .iter()
      .any(|e| e.message.contains("boom")),
    "expected thrown exception to be recorded; got {:?}",
    diagnostics.js_exceptions
  );
  assert!(
    diagnostics
      .console_messages
      .iter()
      .any(|m| m.level == ConsoleMessageLevel::Log && m.message == "after"),
    "expected console.log('after') to run after the exception; got {:?}",
    diagnostics.console_messages
  );
  Ok(())
}

#[test]
fn browser_tab_js_exception_stack_includes_inline_script_name() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>throw new Error("boom")</script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert_eq!(
    diagnostics.js_exceptions.len(),
    1,
    "expected one exception; got {:?}",
    diagnostics.js_exceptions
  );
  let exception = &diagnostics.js_exceptions[0];
  assert!(
    exception.message.contains("boom"),
    "unexpected exception message: {:?}",
    exception.message
  );
  let stack = exception
    .stack
    .as_deref()
    .expect("expected stack trace to be captured for Error throws");
  assert!(
    stack.contains("<inline script"),
    "expected stack trace to include inline script name; got {stack:?}"
  );
  Ok(())
}

#[test]
fn browser_tab_js_exception_stack_includes_external_script_url() -> fastrender::Result<()> {
  let url = "https://example.com/a.js";

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html("<!doctype html><html></html>", options.clone(), executor)?;
  tab.register_script_source(url, r#"throw new Error("boom");"#);

  let html = format!(
    r#"<!doctype html>
    <html>
      <body>
        <script src="{url}"></script>
      </body>
    </html>"#
  );
  tab.navigate_to_html(&html, options)?;
  let _ = tab.render_frame()?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert_eq!(
    diagnostics.js_exceptions.len(),
    1,
    "expected one exception; got {:?}",
    diagnostics.js_exceptions
  );
  let exception = &diagnostics.js_exceptions[0];
  assert!(
    exception.message.contains("boom"),
    "unexpected exception message: {:?}",
    exception.message
  );
  let stack = exception
    .stack
    .as_deref()
    .expect("expected stack trace to be captured for Error throws");
  assert!(
    stack.contains(url),
    "expected stack trace to include script URL {url:?}; got {stack:?}"
  );
  Ok(())
}

#[test]
fn browser_tab_module_script_exception_stack_includes_inline_module_url() -> fastrender::Result<()>
{
  let html = r#"<!doctype html>
    <html>
      <body>
        <script type="module">throw new Error("boom")</script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let mut js_options = fastrender::js::JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_js_execution_options(
    html,
    "https://example.com/doc.html",
    options,
    js_options,
  )?;
  // Module scripts are deferred until parsing completes; drain tasks so the module executes.
  tab.run_event_loop_until_idle(fastrender::js::RunLimits::unbounded())?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert_eq!(
    diagnostics.js_exceptions.len(),
    1,
    "expected one exception; got {:?}",
    diagnostics.js_exceptions
  );
  let exception = &diagnostics.js_exceptions[0];
  assert!(
    exception.message.contains("boom"),
    "unexpected exception message: {:?}",
    exception.message
  );
  let stack = exception
    .stack
    .as_deref()
    .expect("expected stack trace to be captured for Error throws");
  assert!(
    stack.contains("https://example.com/doc.html#inline-module-"),
    "expected stack trace to include synthesized inline module URL; got {stack:?}"
  );
  Ok(())
}

#[test]
fn browser_tab_module_script_exception_stack_includes_external_module_url() -> fastrender::Result<()>
{
  let url = "https://example.com/a.js";

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let mut js_options = fastrender::js::JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut tab =
    BrowserTab::from_html_with_vmjs_and_js_execution_options("", options.clone(), js_options)?;
  tab.register_script_source(url, r#"throw new Error("boom");"#);

  let html = format!(
    r#"<!doctype html>
    <html>
      <body>
        <script type="module" src="{url}"></script>
      </body>
    </html>"#
  );
  tab.navigate_to_html(&html, options)?;
  tab.run_event_loop_until_idle(fastrender::js::RunLimits::unbounded())?;

  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  assert_eq!(
    diagnostics.js_exceptions.len(),
    1,
    "expected one exception; got {:?}",
    diagnostics.js_exceptions
  );
  let exception = &diagnostics.js_exceptions[0];
  assert!(
    exception.message.contains("boom"),
    "unexpected exception message: {:?}",
    exception.message
  );
  let stack = exception
    .stack
    .as_deref()
    .expect("expected stack trace to be captured for Error throws");
  assert!(
    stack.contains(url),
    "expected stack trace to include module URL {url:?}; got {stack:?}"
  );
  Ok(())
}
