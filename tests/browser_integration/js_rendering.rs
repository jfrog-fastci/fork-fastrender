use fastrender::dom2::Document;
use fastrender::error::{Error, Result};
use fastrender::js::{
  JsExecutionOptions, ParseBudget, RunLimits, RunUntilIdleOutcome, RunUntilIdleStopReason,
};
use fastrender::resource::ResourceFetcher;
use fastrender::{BrowserTab, FastRender, RenderOptions, ResourcePolicy};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;

use super::support::FileResourceFetcher;

fn offline_renderer() -> Result<FastRender> {
  super::support::deterministic_renderer_builder()
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .build()
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/html/js")
}

fn fixture_path(name: &str) -> PathBuf {
  fixtures_dir().join(name)
}

fn read_fixture(name: &str) -> Result<String> {
  std::fs::read_to_string(fixture_path(name))
    .map_err(|err| Error::Other(format!("failed to read fixture {name}: {err}")))
}

fn file_url_for_path(path: &Path) -> Result<String> {
  Url::from_file_path(path)
    .map(|url| url.to_string())
    .map_err(|()| Error::Other(format!("failed to convert path to file:// URL: {path:?}")))
}

fn render_static_fixture(name: &str, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
  let html = read_fixture(name)?;
  let mut renderer = offline_renderer()?;
  renderer.render_html_with_options(&html, options)
}

fn fetch_script_source(fetcher: &dyn ResourceFetcher, url: &str) -> Result<String> {
  let res = fetcher.fetch(url)?;
  String::from_utf8(res.bytes).map_err(|err| {
    Error::Other(format!(
      "script source was not valid UTF-8: url={url:?} err={err}"
    ))
  })
}

fn render_dom_snapshot(dom: &Document, options: RenderOptions) -> Result<tiny_skia::Pixmap> {
  let mut renderer = offline_renderer()?;
  let dom = dom.to_renderer_dom();
  renderer.render_dom_with_options(&dom, options)
}

fn root_class(dom: &Document) -> Option<String> {
  let root = dom.get_element_by_id("root")?;
  dom
    .get_attribute(root, "class")
    .ok()
    .flatten()
    .map(|s| s.to_string())
}

fn tab_from_fixture(
  name: &str,
  options: RenderOptions,
  js_execution_options: JsExecutionOptions,
) -> Result<BrowserTab> {
  let html = read_fixture(name)?;
  let document_url = file_url_for_path(&fixture_path(name))?;
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(FileResourceFetcher::default());
  BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher_and_js_execution_options(
    &html,
    &document_url,
    options,
    fetcher,
    js_execution_options,
  )
}

#[test]
fn js_inline_script_mutation_affects_render() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut tab = tab_from_fixture("inline_mutation.html", options.clone(), JsExecutionOptions::default())?;
  let _ = tab.run_until_stable(/* max_frames */ 10)?;
  assert_eq!(root_class(tab.dom()).as_deref(), Some("on"));

  let actual = render_dom_snapshot(tab.dom(), options.clone())?;
  let expected = render_static_fixture("inline_mutation_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "inline script should mutate DOM and affect final pixels"
  );
  Ok(())
}

#[test]
fn js_external_defer_scripts_execute_in_order_after_parsing() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  // Use a tiny parse budget so HTML parsing yields back to the event loop before reaching EOF. This
  // lets us run the script fetch tasks while parsing is still in progress, exercising the HTML rule
  // that `defer` scripts must not execute until parsing completes.
  let mut js_opts = JsExecutionOptions::default();
  js_opts.dom_parse_budget = ParseBudget::new(3);
  let mut tab = tab_from_fixture("external_defer.html", options.clone(), js_opts)?;

  assert!(
    tab.dom().get_element_by_id("box").is_none(),
    "expected parsing to be incomplete immediately after construction with a tiny parse budget"
  );

  // Run just enough tasks for the two defer script fetches to complete, but stop before resuming
  // parsing (the parse-resume task is queued after the fetch tasks).
  let outcome = tab.run_event_loop_until_idle(RunLimits {
    max_tasks: 2,
    max_microtasks: 1024,
    max_wall_time: None,
  })?;
  assert!(
    matches!(
      outcome,
      RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. })
    ),
    "expected event loop to stop after 2 tasks (fetch tasks) with parsing still pending; got {outcome:?}"
  );
  assert!(
    tab.dom().get_element_by_id("box").is_none(),
    "expected parsing to still be incomplete after running only fetch tasks"
  );
  assert_eq!(
    root_class(tab.dom()).as_deref(),
    Some("off"),
    "defer scripts must not execute before parsing completes"
  );

  let _ = tab.run_until_stable(/* max_frames */ 10)?;
  assert_eq!(root_class(tab.dom()).as_deref(), Some("step2"));

  let actual = render_dom_snapshot(tab.dom(), options.clone())?;
  let expected = render_static_fixture("external_defer_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "defer scripts should run after parsing and in document order"
  );
  Ok(())
}

#[test]
fn js_external_async_script_runs_without_waiting_for_parsing_complete() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut tab = tab_from_fixture("external_async.html", options.clone(), JsExecutionOptions::default())?;

  // `BrowserTab::from_html_*` uses the streaming parser driver and yields at async script boundaries
  // so "fast" async scripts can execute before later HTML is parsed.
  assert_eq!(
    root_class(tab.dom()).as_deref(),
    Some("on"),
    "async scripts should be able to mutate the document before parsing completes"
  );
  assert!(
    tab.dom().get_element_by_id("box").is_none(),
    "expected parsing to pause at the async script boundary before reaching the body"
  );

  let _ = tab.run_until_stable(/* max_frames */ 10)?;
  assert!(
    tab.dom().get_element_by_id("box").is_some(),
    "expected parsing to complete after running until stable"
  );

  let actual = render_dom_snapshot(tab.dom(), options.clone())?;
  let expected = render_static_fixture("external_async_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "async script should mutate DOM even before parsing_completed"
  );
  Ok(())
}

#[test]
fn js_base_url_timing_script_before_base_href_uses_document_url() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let options = RenderOptions::new().with_viewport(64, 64);

  let mut tab = tab_from_fixture("base_url_timing.html", options.clone(), JsExecutionOptions::default())?;
  let _ = tab.run_until_stable(/* max_frames */ 10)?;

  assert_eq!(root_class(tab.dom()).as_deref(), Some("after"));

  let actual = render_dom_snapshot(tab.dom(), options.clone())?;
  let expected = render_static_fixture("base_url_timing_static.html", options)?;
  assert_eq!(
    actual.data(),
    expected.data(),
    "script before <base href> should resolve against document URL and affect pixels"
  );
  Ok(())
}
