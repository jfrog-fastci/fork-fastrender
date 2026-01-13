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
fn indexed_db_presence_shim_installs_and_fails_async() -> Result<()> {
  let dom = Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    Arc::new(NoFetch::default()),
    js_opts_for_test(),
  )?;

  host.exec_script(
    r#"
      globalThis.__idb_ok = false;
      globalThis.__idb_events = [];

      // Presence checks.
      const hasIndexedDb = typeof indexedDB === 'object' && indexedDB !== null;
      const aliasOk =
        indexedDB === webkitIndexedDB &&
        indexedDB === mozIndexedDB &&
        indexedDB === msIndexedDB &&
        indexedDB === OIndexedDB;
      const ctorOk =
        typeof IDBFactory === 'function' &&
        typeof IDBRequest === 'function' &&
        typeof IDBOpenDBRequest === 'function' &&
        typeof IDBDatabase === 'function' &&
        typeof IDBTransaction === 'function' &&
        typeof IDBObjectStore === 'function' &&
        typeof IDBKeyRange === 'function';

      // `open` must not throw synchronously.
      let req;
      let threw = false;
      try {
        req = indexedDB.open('x');
      } catch (e) {
        threw = true;
      }

      // Register an attribute handler + listeners. Attribute handler runs first and exceptions
      // must be swallowed so later listeners still run.
      if (req) {
        req.onerror = function (e) {
          globalThis.__idb_events.push('attr');
          throw new Error('handler failure should be swallowed');
        };
        req.addEventListener('error', function (e) {
          globalThis.__idb_events.push('listener1');
          globalThis.__idb_error_name = req.error && req.error.name;
          globalThis.__idb_ready_state = req.readyState;
          globalThis.__idb_event_type = e && e.type;
          globalThis.__idb_event_target_ok = !!e && e.target === req && e.currentTarget === req;
          globalThis.__idb_result_nullish = (req.result === undefined || req.result === null);
        });
        req.addEventListener('error', function (_e) {
          globalThis.__idb_events.push('listener2');
        });
      }

      globalThis.__idb_ok = hasIndexedDb && aliasOk && ctorOk && !threw;
    "#,
  )?;

  host.run_until_idle(RunLimits::unbounded())?;

  let ok = host.exec_script(
    r#"
      globalThis.__idb_ok === true
        && globalThis.__idb_error_name === 'NotSupportedError'
        && globalThis.__idb_ready_state === 'done'
        && globalThis.__idb_event_type === 'error'
        && globalThis.__idb_event_target_ok === true
        && globalThis.__idb_result_nullish === true
        && Array.isArray(globalThis.__idb_events)
        && globalThis.__idb_events.join(',') === 'attr,listener1,listener2'
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

