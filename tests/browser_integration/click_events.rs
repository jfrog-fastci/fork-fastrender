use fastrender::js::RunLimits;
use fastrender::web::events::{EventInit, MouseEvent};
use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};

#[test]
fn click_prevent_default_blocks_link_navigation() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
fn click_prevent_default_document_onclick_blocks_link_navigation() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
<script>
  document.onclick = function (ev) { ev.preventDefault(); };
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
fn click_default_action_resolves_empty_href_against_base_url() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<base href="https://example.com/dir/page.html#frag">
<a id="link" href="">reload</a>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved.as_deref(), Some("https://example.com/dir/page.html"));
  Ok(())
}

#[test]
fn click_default_action_resolves_whitespace_href_against_base_url() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<base href="https://example.com/dir/page.html#frag">
<a id="link" href="   ">reload</a>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved.as_deref(), Some("https://example.com/dir/page.html"));
  Ok(())
}

#[test]
fn click_listeners_can_schedule_tasks_via_event_loop_web_apis() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

#[test]
fn click_listener_registered_with_abort_signal_is_removed_when_signal_aborts() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
  <div id="target"></div>
  <script>
   var t = document.getElementById("target");
   var c = new AbortController();
   t.addEventListener("click", function () {
     t.setAttribute("data-fired", "1");
   }, { signal: c.signal });
   c.abort();
 </script>
 "#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let target = tab
    .dom()
    .get_element_by_id("target")
    .expect("expected <div id=target> to be present");

  let _default_allowed = tab.dispatch_click_event(target)?;
  let _ = tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert_eq!(
    tab.dom().get_attribute(target, "data-fired").unwrap(),
    None,
    "expected click listener to be removed when its AbortSignal aborts"
  );
  Ok(())
}

#[test]
fn click_listener_receives_mouse_event_with_ui_event_detail() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
 <div id="target"></div>
 <script>
   var t = document.getElementById("target");
   t.addEventListener("click", function (ev) {
    t.setAttribute("data-is-mouse-event", String(ev instanceof MouseEvent));
    t.setAttribute("data-detail", String(ev.detail));
  });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let target = tab
    .dom()
    .get_element_by_id("target")
    .expect("expected <div id=target> to be present");

  tab.dispatch_click_event(target)?;
  assert_eq!(
    tab
      .dom()
      .get_attribute(target, "data-is-mouse-event")
      .unwrap(),
    Some("true")
  );
  assert_eq!(
    tab.dom().get_attribute(target, "data-detail").unwrap(),
    Some("1")
  );
  Ok(())
}

#[test]
fn mousemove_handler_property_on_body_fires_for_descendant_target() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<div id="child"></div>
<script>
  document.body.onmousemove = function (ev) {
    document.body.setAttribute("data-fired", "1");
    document.body.setAttribute("data-target-id", String(ev.target && ev.target.id));
  };
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;
  let child = tab
    .dom()
    .get_element_by_id("child")
    .expect("expected <div id=child> to be present");

  tab.dispatch_mouse_event(
    child,
    "mousemove",
    EventInit {
      bubbles: true,
      cancelable: false,
      composed: false,
    },
    MouseEvent::default(),
  )?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-fired").unwrap(),
    Some("1"),
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-target-id").unwrap(),
    Some("child"),
  );
  Ok(())
}
