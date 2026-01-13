use fastrender::dom2::Document;
use fastrender::error::{Error, Result};
use fastrender::js::{RunLimits, RunUntilIdleOutcome};
use fastrender::{BrowserTab, RenderOptions, SelectionAction};
use std::time::Duration;

#[test]
fn a11y_selection_action_updates_native_select_and_dispatches_events() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <body>
        <select id="s">
          <option id="o1" value="one">One</option>
          <option id="o2" value="two">Two</option>
        </select>
        <script>
          const s = document.getElementById("s");
          s.addEventListener("input", () => { s.setAttribute("data-input", s.value); });
          s.addEventListener("change", () => { s.setAttribute("data-change", s.value); });
        </script>
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
  let select = dom
    .get_element_by_id("s")
    .ok_or_else(|| Error::Other("expected <select id=s> to exist".to_string()))?;
  let option_two = dom
    .get_element_by_id("o2")
    .ok_or_else(|| Error::Other("expected <option id=o2> to exist".to_string()))?;

  assert!(tab.perform_selection_action(option_two, SelectionAction::SetSelection)?);

  assert_eq!(
    tab.run_event_loop_until_idle(run_limits)?,
    RunUntilIdleOutcome::Idle
  );

  assert!(
    tab
      .dom()
      .option_selected(option_two)
      .map_err(|e| Error::Other(e.to_string()))?
  );

  let data_input = tab
    .dom()
    .get_attribute(select, "data-input")
    .map_err(|e| Error::Other(e.to_string()))?;
  let data_change = tab
    .dom()
    .get_attribute(select, "data-change")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(data_input, Some("two"));
  assert_eq!(data_change, Some("two"));

  Ok(())
}

