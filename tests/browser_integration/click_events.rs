use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};
use fastrender::js::RunLimits;

#[test]
fn click_prevent_default_blocks_link_navigation() -> Result<()> {
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
<script>
  var link = document.getElementById("link");
  link.addEventListener("click", function (ev) { ev.preventDefault(); });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved, None);
  Ok(())
}

#[test]
fn click_default_action_resolves_link_when_not_canceled() -> Result<()> {
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved.as_deref(), Some("https://example.com/next"));
  Ok(())
}

#[test]
fn click_listeners_can_schedule_tasks_via_event_loop_web_apis() -> Result<()> {
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
<script>
  var link = document.getElementById("link");
  link.addEventListener("click", function () {
    setTimeout(function () {
      document.body.setAttribute("data-fired", "1");
    }, 0);
  });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let _default_allowed = tab.dispatch_click_event(link)?;
  let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-fired").unwrap(),
    Some("1")
  );

  Ok(())
}

#[test]
fn script_load_listeners_can_schedule_microtasks_via_event_loop_web_apis() -> Result<()> {
  let html = r#"<!doctype html>
<script id="s" src="a.js" async></script>
<script>
  var s = document.getElementById("s");
  s.addEventListener("load", function () {
    queueMicrotask(function () {
      document.body.setAttribute("data-fired", "1");
    });
  });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;
  tab.register_script_source("a.js", "/* ok */");

  let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-fired").unwrap(),
    Some("1")
  );

  Ok(())
}
