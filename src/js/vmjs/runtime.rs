use crate::error::Result;

use super::event_loop::EventLoop;
use vm_js::Value;

/// JS value type used by FastRender's `vm-js` embedding.
///
/// This replaces the old placeholder `JsValue` enum previously defined in `window_timers.rs`.
pub type JsValue = Value;

pub type NativeFunction<Host> =
  Box<dyn Fn(&mut Host, &mut EventLoop<Host>) -> Result<JsValue> + 'static>;

pub trait JsObject<Host: 'static> {
  fn define_method(&mut self, name: &str, func: NativeFunction<Host>);
}

pub trait JsRuntime<Host: 'static> {
  type Object: JsObject<Host>;

  fn global_object(&mut self, name: &str) -> &mut Self::Object;

  fn define_global_function(&mut self, name: &str, func: NativeFunction<Host>);
}

#[cfg(test)]
mod tests {
  use crate::dom2::parse_html;
  use crate::error::{Error, Result};
  use crate::js::{EventLoop, WindowHostState};
  use crate::resource::{FetchedResource, ResourceFetcher};
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

  #[test]
  fn vm_js_exec_script_surfaces_syntax_error() -> Result<()> {
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

    let err = host
      .exec_script_in_event_loop(&mut event_loop, "function {")
      .expect_err("expected syntax error");
    assert!(
      err.to_string().to_lowercase().contains("syntax"),
      "expected error to mention syntax, got: {err}"
    );
    Ok(())
  }
}
