use fastrender::api::{ConsoleMessageLevel, DiagnosticsLevel};
use fastrender::{BrowserTab, RenderOptions, VmJsBrowserTabExecutor};

fn console_logs(tab: &BrowserTab) -> Vec<String> {
  let diagnostics = tab
    .diagnostics_snapshot()
    .expect("expected diagnostics to be enabled");
  diagnostics
    .console_messages
    .into_iter()
    .filter(|m| m.level == ConsoleMessageLevel::Log)
    .map(|m| m.message)
    .collect()
}

#[test]
fn browser_tab_formats_console_percent_placeholders() -> fastrender::Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>
          console.log('hello %s %% %d', 'world', 1);
          console.log('x%cy', 'color:red');
          console.log('x=%s');
          console.log('x', 1, 2);
          let called = false;
          let o = { get message() { called = true; return 'boom'; } };
          console.log('%o', o);
          console.log(String(called));
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;

  // Force any pending rendering work so inline scripts have executed and their console output has
  // been recorded in diagnostics.
  let _ = tab.render_frame()?;

  assert_eq!(
    console_logs(&tab),
    vec![
      "hello world % 1".to_string(),
      "xy".to_string(),
      "x=%s".to_string(),
      "x 1 2".to_string(),
      "[object]".to_string(),
      "false".to_string(),
    ]
  );
  Ok(())
}
