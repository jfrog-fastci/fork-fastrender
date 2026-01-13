use fastrender::dom::parse_html;
use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{JsExecutionOptions, WindowHost};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use vm_js::Value;

#[derive(Debug, Default)]
struct NoFetch;

impl ResourceFetcher for NoFetch {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!("unexpected fetch: {url}")))
  }
}

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time; keep a generous limit so tests remain stable
  // under parallel execution and CPU contention.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

fn host_with_empty_body() -> Result<WindowHost> {
  let renderer_dom = parse_html("<!doctype html><body></body>")?;
  let dom = Dom2Document::from_renderer_dom(&renderer_dom);
  // Ensure `document.body` exists and is non-null.
  debug_assert!(dom.body().is_some());
  WindowHost::new_with_fetcher_and_options(dom, "https://example.invalid/", Arc::new(NoFetch), js_opts_for_test())
}

#[test]
fn shadow_root_inserted_as_fragment_open() -> Result<()> {
  let mut host = host_with_empty_body()?;

  let value = host.exec_script(
    r#"
    (function () {
      const host = document.createElement('div');
      const sr = host.attachShadow({ mode: 'open' });
      const span = document.createElement('span');
      span.id = 'moved-open';
      sr.appendChild(span);

      document.body.appendChild(host);

      // ShadowRoot.remove() must be a no-op (ShadowRoot is never a tree child).
      sr.remove();
      if (host.shadowRoot !== sr) throw new Error('shadow root detached via remove()');
      if (sr.childNodes.length !== 1) throw new Error('shadow root children changed by remove()');

      // Inserting a ShadowRoot should behave like inserting a DocumentFragment: its children are
      // moved into the destination and the ShadowRoot is emptied.
      document.body.appendChild(sr);

      if (document.getElementById('moved-open') !== span) throw new Error('span was not moved to document');
      if (document.body.lastChild !== span) throw new Error('span not appended to body');
      if (sr.childNodes.length !== 0) throw new Error('shadow root was not emptied');
      if (host.shadowRoot !== sr) throw new Error('host.shadowRoot was detached');

      // ShadowRoot is not a tree child; it cannot be removed/replaced nor used as a reference child.
      try {
        host.removeChild(sr);
        throw new Error('removeChild did not throw');
      } catch (e) {
        if (!e || e.name !== 'NotFoundError') throw e;
      }

      try {
        host.insertBefore(document.createElement('i'), sr);
        throw new Error('insertBefore did not throw');
      } catch (e) {
        if (!e || e.name !== 'NotFoundError') throw e;
      }

      try {
        host.replaceChild(document.createElement('b'), sr);
        throw new Error('replaceChild did not throw');
      } catch (e) {
        if (!e || e.name !== 'NotFoundError') throw e;
      }

      return true;
    })()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn shadow_root_inserted_as_fragment_closed() -> Result<()> {
  let mut host = host_with_empty_body()?;

  let value = host.exec_script(
    r#"
    (function () {
      const host = document.createElement('div');
      const sr = host.attachShadow({ mode: 'closed' });
      const span = document.createElement('span');
      span.id = 'moved-closed';
      sr.appendChild(span);

      document.body.appendChild(host);
      if (host.shadowRoot !== null) throw new Error('closed shadow root should not be exposed on host.shadowRoot');

      // ShadowRoot.remove() must not detach the ShadowRoot from its host, even in closed mode. The
      // simplest observable check is that a second attachShadow call still fails.
      sr.remove();
      try {
        host.attachShadow({ mode: 'open' });
        throw new Error('attachShadow after remove() did not throw');
      } catch (e) {
        if (!e || e.name !== 'NotSupportedError') throw e;
      }

      document.body.appendChild(sr);

      if (document.getElementById('moved-closed') !== span) throw new Error('span was not moved to document');
      if (document.body.lastChild !== span) throw new Error('span not appended to body');
      if (sr.childNodes.length !== 0) throw new Error('shadow root was not emptied');

      try {
        host.removeChild(sr);
        throw new Error('removeChild did not throw');
      } catch (e) {
        if (!e || e.name !== 'NotFoundError') throw e;
      }

      return true;
    })()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
