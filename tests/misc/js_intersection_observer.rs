use fastrender::dom2::Document;
use fastrender::js::{JsExecutionOptions, RunLimits, WindowHost};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
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
  // `vm-js` budgets are based on wall-clock time. The library default is intentionally aggressive,
  // but under parallel `cargo test` the OS can deschedule a test thread long enough for the VM to
  // observe a false-positive deadline exceed. Use a generous limit to keep integration tests
  // deterministic while still bounding infinite loops.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

#[test]
fn intersection_observer_observe_invokes_callback() -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    Arc::new(NoFetch::default()),
    js_opts_for_test(),
  )?;

  host.exec_script(
    r#"
      globalThis.__called = false;

      const target = document.createElement('div');
      const observer = new IntersectionObserver((entries, obs) => {
        if (!Array.isArray(entries)) throw new Error('entries is not an array');
        if (entries.length !== 1) throw new Error('expected one entry');
        if (obs !== observer) throw new Error('observer argument mismatch');
        if (entries[0].target !== target) throw new Error('target mismatch');
        if (entries[0].isIntersecting !== true) throw new Error('isIntersecting mismatch');
        if (entries[0].intersectionRatio !== 1) throw new Error('intersectionRatio mismatch');
        globalThis.__called = true;
      });
      observer.observe(target);
    "#,
  )?;

  host.run_until_idle(RunLimits::unbounded())?;

  let called = host.exec_script("globalThis.__called")?;
  assert_eq!(called, Value::Bool(true));
  Ok(())
}

