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
fn vm_js_string_replace_all() -> Result<()> {
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
      "a a".replaceAll(" ", "_") === "a_a"
        && "ababab".replaceAll("ab", "x") === "xxx"
        && "aaaa".replaceAll("aa", "b") === "bb"
        && "aaa".replaceAll("aa", "b") === "ba"
        && "abc".replaceAll("", "-") === "-a-b-c-"
        && "abc".replaceAll("", "") === "abc"
        && "abc".replaceAll("d", "x") === "abc"
        && "abc".replaceAll() === "abc"
        && "abc".replaceAll("a") === "undefinedbc"
    "#,
  )?;

  assert!(
    matches!(value, Value::Bool(true)),
    "expected replaceAll test expression to be true; got {value:?}"
  );
  Ok(())
}

