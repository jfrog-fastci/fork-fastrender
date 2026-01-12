use fastrender::dom2::parse_html;
use fastrender::js::{EventLoop, WindowHostState};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use std::sync::Arc;
use vm_js::Value;

struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher.fetch unexpectedly called for {url:?}"
    )))
  }
}

#[test]
fn vm_js_string_pad_start_end() -> Result<()> {
  let html = "<!doctype html><html><head></head><body></body></html>";
  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();

  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(NoFetchResourceFetcher);
  let mut host = WindowHostState::new_with_fetcher_and_clock(
    dom,
    "https://example.com/index.html",
    fetcher,
    clock,
  )?;

  let value = host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
      "a".padStart(3, "0") === "00a"
        && "a".padEnd(3, "0") === "a00"
        && "a".padStart(3) === "  a"
        && "a".padEnd(3) === "a  "
        && "abc".padStart(6, "01") === "010abc"
        && "abc".padEnd(6, "01") === "abc010"
        && "a".padStart(3, "") === "a"
        && "a".padEnd(3, "") === "a"
        && "abcd".padStart(2, "0") === "abcd"
        && "abcd".padEnd(2, "0") === "abcd"
        && "abc".padStart() === "abc"
        && "abc".padEnd() === "abc"
    "#,
  )?;

  assert!(
    matches!(value, Value::Bool(true)),
    "expected padStart/padEnd to return true; got {value:?}"
  );
  Ok(())
}

