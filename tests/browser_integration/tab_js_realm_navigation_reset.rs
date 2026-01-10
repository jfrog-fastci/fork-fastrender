use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::dom2::NodeId;
use fastrender::error::{Error, Result};
use fastrender::js::RunLimits;
use fastrender::{BrowserTab, RenderOptions};
use std::time::Duration;

fn attr(doc: &fastrender::dom2::Document, node: NodeId, name: &str) -> Result<Option<String>> {
  doc
    .get_attribute(node, name)
    .map(|value| value.map(|s| s.to_string()))
    .map_err(|err| Error::Other(err.to_string()))
}

#[test]
fn tab_navigation_resets_vm_js_realm_and_current_script() -> Result<()> {
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
  let mut tab = BrowserTab::from_html(html_a, options.clone(), VmJsBrowserTabExecutor::new())?;

  let run_limits = RunLimits {
    max_wall_time: Some(Duration::from_secs(1)),
    ..RunLimits::unbounded()
  };
  tab.run_until_stable_with_run_limits(run_limits, 8)?;

  let marker = tab
    .dom()
    .get_element_by_id("marker")
    .ok_or_else(|| Error::Other("missing marker element in HTML A".to_string()))?;
  assert_eq!(attr(tab.dom(), marker, "data-from")?.as_deref(), Some("A"));

  tab.navigate_to_html(html_b, options)?;
  tab.run_until_stable_with_run_limits(run_limits, 8)?;

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
