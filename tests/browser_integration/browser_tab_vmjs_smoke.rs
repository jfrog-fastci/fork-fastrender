use fastrender::dom2::Document;
use fastrender::error::{Error, Result};
use fastrender::js::{RunLimits, RunUntilIdleOutcome};
use fastrender::{BrowserTab, RenderOptions};
use std::time::Duration;

#[test]
fn browser_tab_vmjs_smoke_runs_inline_script_and_mutates_dom() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <body>
        <script>document.body.setAttribute("data-ok", "1")</script>
      </body>
    </html>"#;

  let mut tab = BrowserTab::from_html_with_vmjs(html, RenderOptions::new().with_viewport(32, 32))?;

  let run_limits = RunLimits {
    max_tasks: 128,
    max_microtasks: 1024,
    max_wall_time: Some(Duration::from_millis(500)),
  };
  assert_eq!(
    tab.run_event_loop_until_idle(run_limits)?,
    RunUntilIdleOutcome::Idle
  );

  let dom: &Document = tab.dom();
  let body = dom
    .body()
    .ok_or_else(|| Error::Other("expected document.body to exist".to_string()))?;
  let value = dom
    .get_attribute(body, "data-ok")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(value, Some("1"));
  Ok(())
}
