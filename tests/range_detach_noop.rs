use fastrender::dom2::Document;
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::js::{Clock, JsExecutionOptions, WindowHost};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::time::Duration;
use vm_js::Value;

#[derive(Debug, Default)]
struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher does not support fetch: {url}"
    )))
  }
}

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time. Under parallel `cargo test`, the OS can
  // deschedule a test thread long enough for the VM to observe a false-positive deadline exceed.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

fn assert_range_detach_is_noop(backend: DomBindingsBackend) -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let clock: Arc<dyn Clock> = Arc::new(fastrender::js::VirtualClock::new());
  let mut host = WindowHost::new_with_fetcher_and_clock_and_options_and_dom_backend(
    dom,
    "https://example.invalid/",
    Arc::new(NoFetchResourceFetcher),
    clock,
    js_opts_for_test(),
    backend,
  )?;

  let script = match backend {
    DomBindingsBackend::Handwritten => r#"
      (() => {
        const r = document.createRange();
        const beforeStartContainer = r.startContainer;
        const beforeStartOffset = r.startOffset;
        const beforeEndContainer = r.endContainer;
        const beforeEndOffset = r.endOffset;

        if (typeof r.detach !== 'function') return false;
        const ret = r.detach();
        return ret === undefined
          && r.startContainer === beforeStartContainer
          && r.startOffset === beforeStartOffset
          && r.endContainer === beforeEndContainer
          && r.endOffset === beforeEndOffset
          && r.collapsed === true;
      })()
    "#,
    // WebIDL bindings for Range are still under development; keep this test focused on the legacy
    // requirement (Range.prototype.detach exists and is a no-op that does not throw).
    DomBindingsBackend::WebIdl => r#"
      (() => {
        const r = document.createRange();
        if (typeof r.detach !== 'function') return false;
        try {
          const ret = r.detach();
          return ret === undefined;
        } catch (_e) {
          return false;
        }
      })()
    "#,
  };

  let out = host.exec_script(script)?;

  assert_eq!(out, Value::Bool(true));
  Ok(())
}

#[test]
fn range_detach_is_noop_handwritten() -> Result<()> {
  assert_range_detach_is_noop(DomBindingsBackend::Handwritten)
}

#[test]
fn range_detach_is_noop_webidl() -> Result<()> {
  assert_range_detach_is_noop(DomBindingsBackend::WebIdl)
}
