use fastrender::js::{WindowRealm, WindowRealmConfig};
use fastrender::{BrowserDocumentDom2, Error, RenderOptions, Result};
use vm_js::{Job, RealmId, Value, VmHostHooks};

use super::support;

#[derive(Default)]
struct NoopHostHooks;

impl VmHostHooks for NoopHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}

fn exec_script(
  realm: &mut WindowRealm,
  host: &mut BrowserDocumentDom2,
  source: &str,
) -> Result<Value> {
  let mut hooks = NoopHostHooks::default();
  realm
    .exec_script_with_host_and_hooks(host, &mut hooks, source)
    .map_err(|err| Error::Other(format!("vm-js error: {err:?}")))
}

fn value_to_string(realm: &WindowRealm, value: Value) -> String {
  match value {
    Value::String(s) => realm
      .heap()
      .get_string(s)
      .map(|s| s.to_utf8_lossy())
      .unwrap_or_default(),
    other => panic!("expected string, got {other:?}"),
  }
}

#[test]
fn document_element_from_point_returns_topmost_element() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #a, #b { position: absolute; left: 0; top: 0; width: 50px; height: 50px; }
          #a { background: red; z-index: 1; }
          #b { background: blue; z-index: 2; }
        </style>
      </head>
      <body>
        <div id="a"></div>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let mut doc = BrowserDocumentDom2::new(
    support::deterministic_renderer(),
    html,
    RenderOptions::new().with_viewport(100, 100),
  )?;
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");

  let value = exec_script(
    &mut realm,
    &mut doc,
    "(() => { const el = document.elementFromPoint(10, 10); return el ? el.id : 'null'; })()",
  )?;
  assert_eq!(value_to_string(&realm, value), "b");
  Ok(())
}

#[test]
fn document_element_from_point_skips_pointer_events_none() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #a, #b { position: absolute; left: 0; top: 0; width: 50px; height: 50px; }
          #a { background: red; z-index: 1; }
          #b { background: blue; z-index: 2; pointer-events: none; }
        </style>
      </head>
      <body>
        <div id="a"></div>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let mut doc = BrowserDocumentDom2::new(
    support::deterministic_renderer(),
    html,
    RenderOptions::new().with_viewport(100, 100),
  )?;
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");

  let value = exec_script(
    &mut realm,
    &mut doc,
    "(() => { const el = document.elementFromPoint(10, 10); return el ? el.id : 'null'; })()",
  )?;
  assert_eq!(value_to_string(&realm, value), "a");
  Ok(())
}

#[test]
fn document_element_from_point_out_of_viewport_returns_null() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #a { position: absolute; left: 0; top: 0; width: 50px; height: 50px; background: red; }
        </style>
      </head>
      <body>
        <div id="a"></div>
      </body>
    </html>
  "#;

  let mut doc = BrowserDocumentDom2::new(
    support::deterministic_renderer(),
    html,
    RenderOptions::new().with_viewport(100, 100),
  )?;
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");

  let value = exec_script(
    &mut realm,
    &mut doc,
    "(() => {\n\
      return document.elementFromPoint(-1, 10) === null\n\
        && document.elementFromPoint(100, 10) === null\n\
        && document.elementFromPoint(10, -1) === null\n\
        && document.elementFromPoint(10, 100) === null;\n\
    })()",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn document_elements_from_point_matches_element_from_point() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #a, #b { position: absolute; left: 0; top: 0; width: 50px; height: 50px; }
          #a { background: red; z-index: 1; }
          #b { background: blue; z-index: 2; }
        </style>
      </head>
      <body>
        <div id="a"></div>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let mut doc = BrowserDocumentDom2::new(
    support::deterministic_renderer(),
    html,
    RenderOptions::new().with_viewport(100, 100),
  )?;
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");

  let value = exec_script(
    &mut realm,
    &mut doc,
    "(() => {\n\
      const top = document.elementFromPoint(10, 10);\n\
      const all = document.elementsFromPoint(10, 10);\n\
      return top !== null && all.length > 0 && top === all[0];\n\
    })()",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn document_element_from_point_is_safe_noop_without_renderer_host() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");
  let mut host_ctx = ();
  let mut hooks = NoopHostHooks::default();
  let value = realm
    .exec_script_with_host_and_hooks(
      &mut host_ctx,
      &mut hooks,
      "document.elementFromPoint(10, 10)",
    )
    .map_err(|err| Error::Other(format!("vm-js error: {err:?}")))?;
  assert_eq!(value, Value::Null);
  Ok(())
}
