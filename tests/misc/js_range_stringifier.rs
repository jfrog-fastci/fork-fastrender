use fastrender::dom2::parse_html;
use fastrender::js::{EventLoop, JsExecutionOptions, WindowHostState};
use fastrender::js::window_realm::DomBindingsBackend;
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

fn value_to_string(host: &mut WindowHostState, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  let window = host.window_mut();
  let (_vm, heap) = window.vm_and_heap_mut();
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn build_host(
  dom_backend: DomBindingsBackend,
) -> Result<(WindowHostState, EventLoop<WindowHostState>)> {
  let html = "<!doctype html><html><body>\
    <div id=\"t\">a😀b</div>\
    <div id=\"root\"><span>a</span><span>b</span><span>c</span></div>\
  </body></html>";
  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetch::default());
  let host = WindowHostState::new_with_fetcher_and_clock_and_options_and_dom_backend(
    dom,
    "https://example.invalid/",
    fetcher,
    clock,
    js_opts_for_test(),
    dom_backend,
  )?;
  Ok((host, event_loop))
}

fn assert_range_stringifier(dom_backend: DomBindingsBackend) -> Result<()> {
  let (mut host, mut event_loop) = build_host(dom_backend)?;

  // Single-text-node substring with UTF-16 code unit offsets.
  let out = host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
    (() => {
      const r = new Range();
      const text = document.getElementById("t").firstChild;
      r.setStart(text, 1);
      r.setEnd(text, 3);
      return String(r);
    })()
    "#,
  )?;
  assert_eq!(value_to_string(&mut host, out), "😀");

  // Concatenation across multiple Text nodes in tree order.
  let out = host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
    (() => {
      const r = new Range();
      const root = document.getElementById("root");
      const start = root.childNodes[0].firstChild; // "a"
      const end = root.childNodes[2].firstChild;   // "c"
      r.setStart(start, 0);
      r.setEnd(end, 1);
      return r + "";
    })()
    "#,
  )?;
  assert_eq!(value_to_string(&mut host, out), "abc");

  Ok(())
}

#[test]
fn range_stringifier_handwritten_backend() -> Result<()> {
  assert_range_stringifier(DomBindingsBackend::Handwritten)
}

#[test]
fn range_stringifier_webidl_backend() -> Result<()> {
  // WebIDL backend should expose `document.createRange()` as well as `new Range()`.
  let (mut host, mut event_loop) = build_host(DomBindingsBackend::WebIdl)?;

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
    (() => {
      const r = document.createRange();
      if (!(r instanceof Range)) return false;
      const text = document.getElementById("t").firstChild;
      r.setStart(text, 1);
      r.setEnd(text, 3);
      return r.toString() === "😀";
    })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));

  assert_range_stringifier(DomBindingsBackend::WebIdl)
}

