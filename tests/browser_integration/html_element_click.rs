use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};

#[test]
fn html_element_click_toggles_checkbox_and_dispatches_mouse_event() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<body>
  <input id="cb" type="checkbox">
  <script>
    const cb = document.getElementById("cb");
    cb.addEventListener("click", function (ev) {
      document.body.setAttribute("data-fired", "1");
      document.body.setAttribute("data-is-trusted", String(ev.isTrusted));
      document.body.setAttribute("data-is-mouse", String(ev instanceof MouseEvent));
      document.body.setAttribute("data-detail", String(ev.detail));
    });
    cb.click();
    document.body.setAttribute("data-checked", String(cb.checked));
  </script>
</body>"#;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-fired").unwrap(),
    Some("1")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-is-trusted").unwrap(),
    Some("false")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-is-mouse").unwrap(),
    Some("true")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-detail").unwrap(),
    Some("1")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-checked").unwrap(),
    Some("true")
  );

  let cb = tab
    .dom()
    .get_element_by_id("cb")
    .expect("expected <input id=cb> to be present");
  assert_eq!(tab.dom().input_checked(cb).unwrap(), true);
  Ok(())
}

#[test]
fn html_element_click_checkbox_respects_prevent_default() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<body>
  <input id="cb" type="checkbox">
  <script>
    const cb = document.getElementById("cb");
    cb.addEventListener("click", function (ev) { ev.preventDefault(); });
    cb.click();
    document.body.setAttribute("data-checked", String(cb.checked));
  </script>
</body>"#;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-checked").unwrap(),
    Some("false")
  );
  Ok(())
}

#[test]
fn html_element_click_selects_radio_group() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<body>
  <input id="r1" type="radio" name="g" checked>
  <input id="r2" type="radio" name="g">
  <script>
    const r1 = document.getElementById("r1");
    const r2 = document.getElementById("r2");
    r2.addEventListener("click", function () {
      document.body.setAttribute("data-click-fired", "1");
    });
    r2.click();
    document.body.setAttribute("data-r1", String(r1.checked));
    document.body.setAttribute("data-r2", String(r2.checked));
  </script>
</body>"#;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-click-fired").unwrap(),
    Some("1")
  );
  assert_eq!(tab.dom().get_attribute(body, "data-r1").unwrap(), Some("false"));
  assert_eq!(tab.dom().get_attribute(body, "data-r2").unwrap(), Some("true"));

  let r1 = tab
    .dom()
    .get_element_by_id("r1")
    .expect("expected <input id=r1> to be present");
  let r2 = tab
    .dom()
    .get_element_by_id("r2")
    .expect("expected <input id=r2> to be present");
  assert_eq!(tab.dom().input_checked(r1).unwrap(), false);
  assert_eq!(tab.dom().input_checked(r2).unwrap(), true);
  Ok(())
}

#[test]
fn html_element_click_resets_form() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<body>
  <form id="f">
    <input id="t" value="a">
    <button id="reset" type="reset">reset</button>
  </form>
  <script>
    const t = document.getElementById("t");
    t.value = "b";
    const btn = document.getElementById("reset");
    btn.addEventListener("click", function () {
      document.body.setAttribute("data-click-fired", "1");
    });
    btn.click();
    document.body.setAttribute("data-value", t.value);
  </script>
</body>"#;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-click-fired").unwrap(),
    Some("1")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-value").unwrap(),
    Some("a")
  );

  let t = tab
    .dom()
    .get_element_by_id("t")
    .expect("expected <input id=t> to be present");
  assert_eq!(tab.dom().input_value(t).unwrap(), "a");
  Ok(())
}

#[test]
fn html_element_click_submits_form_and_respects_prevent_default() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
<body>
  <form id="f" action="https://example.com/next">
    <input id="q" name="q" value="x">
    <button id="submit" type="submit">submit</button>
  </form>
  <script>
    const form = document.getElementById("f");
    const btn = document.getElementById("submit");
    form.addEventListener("submit", function (ev) {
      document.body.setAttribute("data-submit-fired", "1");
      ev.preventDefault();
    });
    btn.addEventListener("click", function () {
      document.body.setAttribute("data-click-fired", "1");
    });
    btn.click();
    document.body.setAttribute("data-after-click", "1");
  </script>
</body>"#;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-click-fired").unwrap(),
    Some("1")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-submit-fired").unwrap(),
    Some("1")
  );
  assert_eq!(
    tab.dom().get_attribute(body, "data-after-click").unwrap(),
    Some("1"),
    "expected navigation to be suppressed so script continues running"
  );
  Ok(())
}

#[test]
fn html_element_click_navigates_anchor_fragment_when_not_canceled() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r##"<!doctype html>
<body>
  <a id="link" href="#next">next</a>
  <script>
    const link = document.getElementById("link");
    link.click();
    document.body.setAttribute("data-href", location.href);
  </script>
</body>"##;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-href").unwrap(),
    Some("about:blank#next")
  );
  Ok(())
}

#[test]
fn html_element_click_anchor_respects_prevent_default() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r##"<!doctype html>
<body>
  <a id="link" href="#next">next</a>
  <script>
    const link = document.getElementById("link");
    link.addEventListener("click", function (ev) { ev.preventDefault(); });
    link.click();
    document.body.setAttribute("data-href", location.href);
  </script>
</body>"##;

  let executor = VmJsBrowserTabExecutor::new();
  let tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let body = tab.dom().body().expect("expected <body> element");
  assert_eq!(
    tab.dom().get_attribute(body, "data-href").unwrap(),
    Some("about:blank")
  );
  Ok(())
}
