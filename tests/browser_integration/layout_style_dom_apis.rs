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

#[test]
fn layout_style_dom_shims_expose_basic_metrics() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller { width: 10px; height: 20px; overflow: scroll; }
          #inner { width: 30px; height: 40px; }
        </style>
      </head>
      <body>
        <div id="scroller"><div id="inner"></div></div>
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
      const el = document.getElementById('scroller');\n\
      const rect = el.getBoundingClientRect();\n\
      const close = (a, b) => Math.abs(a - b) < 0.01;\n\
      if (!(rect.width > 0 && rect.height > 0)) return false;\n\
      if (!(el.offsetWidth > 0 && el.offsetHeight > 0)) return false;\n\
      if (!(el.clientWidth >= 0 && el.clientHeight >= 0)) return false;\n\
      if (!(el.clientWidth <= el.offsetWidth && el.clientHeight <= el.offsetHeight)) return false;\n\
      if (!(el.scrollWidth >= el.clientWidth && el.scrollHeight >= el.clientHeight)) return false;\n\
      if (!close(rect.width, el.offsetWidth) || !close(rect.height, el.offsetHeight)) return false;\n\
      el.scrollTop = 5;\n\
      el.scrollLeft = 7;\n\
      if (el.scrollTop !== 5) return false;\n\
      if (el.scrollLeft !== 7) return false;\n\
      return true;\n\
    })()",
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

