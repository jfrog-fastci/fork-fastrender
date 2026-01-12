use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::dom2::NodeId;
use fastrender::error::{Error, Result};
use fastrender::js::{JsExecutionOptions, RunLimits};
use fastrender::{BrowserTab, RenderOptions};

fn attr(doc: &fastrender::dom2::Document, node: NodeId, name: &str) -> Result<Option<String>> {
  doc
    .get_attribute(node, name)
    .map(|value| value.map(|s| s.to_string()))
    .map_err(|err| Error::Other(err.to_string()))
}

#[test]
fn tab_navigation_resets_vm_js_realm_and_current_script() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html_a = r#"<!doctype html>
    <html>
      <head>
        <script id="script-a">
          document.addEventListener("DOMContentLoaded", () => {
            const marker = document.getElementById("marker");
            if (marker) marker.setAttribute("data-from", "A");
          });
        </script>
      </head>
      <body>
        <div id="marker" data-from="A-initial"></div>
      </body>
    </html>"#;

  let html_b = r#"<!doctype html>
    <html>
      <body>
        <div id="marker" data-from="B-initial"></div>
        <script id="script-b">
          const current = document.currentScript ? document.currentScript.id : "null";
          document.documentElement.setAttribute("data-script-id", current);
          const marker = document.getElementById("marker");
          if (marker) marker.setAttribute("data-from", "B-script");
        </script>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let js_execution_options = JsExecutionOptions {
    event_loop_run_limits: RunLimits::unbounded(),
    ..JsExecutionOptions::default()
  };
  let mut tab = BrowserTab::from_html_with_js_execution_options(
    html_a,
    options.clone(),
    VmJsBrowserTabExecutor::new(),
    js_execution_options,
  )?;

  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 8)?;
  assert!(
    matches!(outcome, fastrender::RunUntilStableOutcome::Stable { .. }),
    "expected navigation A to reach Stable, got {outcome:?}"
  );

  let marker = tab
    .dom()
    .get_element_by_id("marker")
    .ok_or_else(|| Error::Other("missing marker element in HTML A".to_string()))?;
  assert_eq!(attr(tab.dom(), marker, "data-from")?.as_deref(), Some("A"));

  tab.navigate_to_html(html_b, options)?;
  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 8)?;
  assert!(
    matches!(outcome, fastrender::RunUntilStableOutcome::Stable { .. }),
    "expected navigation B to reach Stable, got {outcome:?}"
  );

  let marker = tab
    .dom()
    .get_element_by_id("marker")
    .ok_or_else(|| Error::Other("missing marker element in HTML B".to_string()))?;

  // The listener from navigation A must not fire for navigation B.
  assert_eq!(
    attr(tab.dom(), marker, "data-from")?.as_deref(),
    Some("B-script")
  );

  // `document.currentScript` must refer to the currently executing script element in B.
  let doc_el = tab
    .dom()
    .document_element()
    .ok_or_else(|| Error::Other("missing documentElement in HTML B".to_string()))?;
  assert_eq!(
    attr(tab.dom(), doc_el, "data-script-id")?.as_deref(),
    Some("script-b")
  );

  Ok(())
}
