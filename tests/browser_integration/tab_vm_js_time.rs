use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::js::{Clock, EventLoop, RunLimits, VirtualClock};
use fastrender::{BrowserTab, Error, RenderOptions, Result};
use std::sync::Arc;
use std::time::Duration;

fn attr(
  doc: &fastrender::dom2::Document,
  node: fastrender::dom2::NodeId,
  name: &str,
) -> Result<Option<String>> {
  doc
    .get_attribute(node, name)
    .map(|value| value.map(|s| s.to_string()))
    .map_err(|err| Error::Other(err.to_string()))
}

#[test]
fn tab_vm_js_time_apis_follow_event_loop_clock() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <div id="marker"></div>
        <script>
          const m = document.getElementById("marker");
          m.setAttribute("data-origin", String(performance.timeOrigin));
          m.setAttribute("data-now", String(performance.now()));
          m.setAttribute("data-date", String(Date.now()));
          m.setAttribute("data-new-date", String(new Date().getTime()));
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  clock.set_now(Duration::from_millis(5_000));
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<fastrender::BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::new(),
    event_loop,
  )?;

  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 4)?;
  assert!(
    matches!(outcome, fastrender::RunUntilStableOutcome::Stable { .. }),
    "expected run_until_stable to reach Stable, got {outcome:?}"
  );

  let marker = tab
    .dom()
    .get_element_by_id("marker")
    .ok_or_else(|| Error::Other("missing marker element".to_string()))?;

  assert_eq!(
    attr(tab.dom(), marker, "data-origin")?.as_deref(),
    Some("0")
  );
  assert_eq!(
    attr(tab.dom(), marker, "data-now")?.as_deref(),
    Some("5000"),
    "performance.now() must use the tab event loop clock"
  );
  assert_eq!(
    attr(tab.dom(), marker, "data-date")?.as_deref(),
    Some("5000"),
    "Date.now() must use the tab event loop clock (origin defaults to 0 in tests)"
  );
  assert_eq!(
    attr(tab.dom(), marker, "data-new-date")?.as_deref(),
    Some("5000"),
    "new Date().getTime() must use the tab event loop clock (origin defaults to 0 in tests)"
  );
  Ok(())
}
