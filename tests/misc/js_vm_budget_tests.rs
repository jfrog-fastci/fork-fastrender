use fastrender::api::{BrowserTab, RenderOptions, VmJsBrowserTabExecutor};
use fastrender::dom2;
use fastrender::js::window_realm::{WindowRealm, WindowRealmConfig};
use fastrender::js::RunLimits;
use fastrender::js::{JsExecutionOptions, WindowHost};
use fastrender::{Error, FetchedResource, ResourceFetcher};
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::time::Duration;
use vm_js::{Job, RealmId, VmError, VmHostHooks};

#[derive(Debug, Default)]
struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher does not support fetch: {url}"
    )))
  }
}

#[derive(Default)]
struct NoopHostHooks;

impl VmHostHooks for NoopHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}

fn exec_script(
  realm: &mut WindowRealm,
  source: &str,
) -> std::result::Result<vm_js::Value, VmError> {
  let mut host_ctx = ();
  let mut hooks = NoopHostHooks::default();
  realm.exec_script_with_host_and_hooks(&mut host_ctx, &mut hooks, source)
}

#[test]
fn exec_script_infinite_loop_is_terminated_by_fuel_budget() -> fastrender::Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut opts = JsExecutionOptions::default();
  opts.max_instruction_count = Some(50);
  // Keep wall-time generous so we reliably hit fuel termination first.
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.invalid/",
    Arc::new(NoFetchResourceFetcher),
    opts,
  )?;
  let err = host
    .exec_script("for(;;){}")
    .expect_err("expected script to terminate");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("out of fuel"),
    "expected OutOfFuel termination, got: {msg}"
  );
  Ok(())
}

#[test]
fn exec_script_deadline_budget_can_terminate_immediately() -> fastrender::Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut opts = JsExecutionOptions::default();
  // Force an already-expired wall-time deadline so the first `tick()` fails.
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_millis(0));

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.invalid/",
    Arc::new(NoFetchResourceFetcher),
    opts,
  )?;
  let err = host
    .exec_script("for(;;){}")
    .expect_err("expected deadline termination");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("deadline exceeded"),
    "expected DeadlineExceeded termination, got: {msg}"
  );
  Ok(())
}

#[test]
fn window_realm_exec_script_infinite_loop_is_terminated_by_fuel_budget() {
  let mut opts = JsExecutionOptions::default();
  opts.max_instruction_count = Some(50);
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));

  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.invalid/"),
    opts,
  )
  .expect("create WindowRealm");

  let err = exec_script(&mut realm, "for(;;){}").expect_err("expected script to terminate");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("out of fuel"),
    "expected OutOfFuel termination, got: {msg}"
  );
}

#[test]
fn window_realm_exec_script_deadline_budget_can_terminate_immediately() {
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_millis(0));

  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.invalid/"),
    opts,
  )
  .expect("create WindowRealm");

  let err = exec_script(&mut realm, "for(;;){}").expect_err("expected deadline termination");
  let msg = err.to_string().to_ascii_lowercase();
  assert!(
    msg.contains("deadline exceeded"),
    "expected DeadlineExceeded termination, got: {msg}"
  );
}

#[test]
fn module_script_budget_deadline_is_refreshed_relative_to_execution_time() -> fastrender::Result<()>
{
  let mut opts = JsExecutionOptions::default();
  opts.supports_module_scripts = true;
  // Ensure this test remains stable even if defaults change.
  opts.max_instruction_count = Some(1_000_000);
  // Keep the per-spin deadline short so the realm's construction-time deadline expires before the
  // module script is executed.
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_millis(50));

  let mut tab = BrowserTab::from_html_with_js_execution_options(
    r#"<!doctype html><body>
      <script type="module">
        let acc = 0;
        for (let i = 0; i < 200; i++) {
          acc += i;
        }
        // Only write the marker after enough ticks that the stale construction-time deadline would
        // have been checked.
        document.body.setAttribute("data-module-ran", "1");
      </script>
    </body>"#,
    RenderOptions::default(),
    VmJsBrowserTabExecutor::default(),
    opts,
  )?;

  let dom = tab.dom();
  let body = dom.body().expect("body should exist");
  assert_eq!(
    dom
      .get_attribute(body, "data-module-ran")
      .expect("get_attribute should succeed"),
    None
  );

  // Sleep long enough that the VM's default deadline (set at realm creation time) is guaranteed to
  // be in the past.
  std::thread::sleep(Duration::from_millis(100));

  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  let dom = tab.dom();
  let body = dom.body().expect("body should exist");
  assert_eq!(
    dom
      .get_attribute(body, "data-module-ran")
      .expect("get_attribute should succeed"),
    Some("1")
  );
  Ok(())
}
