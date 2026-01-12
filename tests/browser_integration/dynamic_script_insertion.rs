use fastrender::api::{ConsoleMessageLevel, DiagnosticsLevel};
use fastrender::js::{RunLimits, RunUntilIdleOutcome};
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
fn browser_tab_executes_dynamically_inserted_inline_scripts_synchronously() -> fastrender::Result<()>
{
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <body>
        <script>
          var s = document.createElement("script");
          s.setAttribute("id", "dyn");
          s.textContent = "console.log(document.currentScript.getAttribute('id')); queueMicrotask(() => console.log('micro'));";
          document.body.appendChild(s);
          console.log("after");
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(html, options, executor)?;

  // Force any pending rendering work, but the ordering assertions are based on console messages
  // observed during parsing/insertion steps.
  let _ = tab.render_frame()?;

  assert_eq!(
    console_logs(&tab),
    vec!["dyn".to_string(), "after".to_string(), "micro".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_executes_dynamically_inserted_external_scripts_asynchronously(
) -> fastrender::Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let external_url = "https://example.invalid/a.js";

  let html = format!(
    r#"<!doctype html>
      <html>
        <body>
          <script>
            var s = document.createElement("script");
            s.setAttribute("id", "dyn");
            s.src = "{external_url}";
            document.body.appendChild(s);
            console.log("after");
          </script>
        </body>
      </html>"#
  );

  let options = RenderOptions::new()
    .with_viewport(32, 32)
    .with_diagnostics_level(DiagnosticsLevel::Basic);
  let executor = VmJsBrowserTabExecutor::default();
  let mut tab = BrowserTab::from_html(&html, options, executor)?;
  tab.register_script_source(external_url, "console.log('ext');");

  let _ = tab.render_frame()?;
  assert_eq!(console_logs(&tab), vec!["after".to_string()]);

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    console_logs(&tab),
    vec!["after".to_string(), "ext".to_string()]
  );
  Ok(())
}
