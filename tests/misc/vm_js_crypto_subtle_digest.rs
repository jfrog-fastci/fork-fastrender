use fastrender::dom2::parse_html;
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome, WindowHostState};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256};
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
fn vm_js_crypto_subtle_digest_sha256() -> Result<()> {
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

  let expected_hex = {
    let digest = Sha256::digest([1u8, 2, 3]);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
      out.push_str(&format!("{b:02x}"));
    }
    out
  };

  let source = r#"
    globalThis.__digest_hex = "";

    var data = new Uint8Array(3);
    data[0] = 1;
    data[1] = 2;
    data[2] = 3;

    crypto.subtle.digest("SHA-256", data).then(function (buf) {
      var bytes = new Uint8Array(buf);
      var HEX = ["0","1","2","3","4","5","6","7","8","9","a","b","c","d","e","f"];
      var s = "";
      for (var i = 0; i < bytes.length; i++) {
        var b = bytes[i];
        s += HEX[b >> 4];
        s += HEX[b & 15];
      }
      globalThis.__digest_hex = s;
    });
  "#;

  host.exec_script_in_event_loop(&mut event_loop, source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
    RunUntilIdleOutcome::Idle
  );
  assert!(errors.is_empty(), "expected no JS errors; got {errors:?}");

  let ok_value = host.exec_script_in_event_loop(
    &mut event_loop,
    &format!("globalThis.__digest_hex === \"{expected_hex}\""),
  )?;
  assert!(
    matches!(ok_value, Value::Bool(true)),
    "expected digest to match {expected_hex:?}; got {ok_value:?}"
  );
  Ok(())
}

#[test]
fn vm_js_crypto_subtle_digest_sha1() -> Result<()> {
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

  let expected_hex = {
    let digest = Sha1::digest([1u8, 2, 3]);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
      out.push_str(&format!("{b:02x}"));
    }
    out
  };

  let source = r#"
    globalThis.__digest_hex = "";

    var data = new Uint8Array(3);
    data[0] = 1;
    data[1] = 2;
    data[2] = 3;

    crypto.subtle.digest("SHA-1", data).then(function (buf) {
      var bytes = new Uint8Array(buf);
      var HEX = ["0","1","2","3","4","5","6","7","8","9","a","b","c","d","e","f"];
      var s = "";
      for (var i = 0; i < bytes.length; i++) {
        var b = bytes[i];
        s += HEX[b >> 4];
        s += HEX[b & 15];
      }
      globalThis.__digest_hex = s;
    });
  "#;

  host.exec_script_in_event_loop(&mut event_loop, source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
    RunUntilIdleOutcome::Idle
  );
  assert!(errors.is_empty(), "expected no JS errors; got {errors:?}");

  let ok_value = host.exec_script_in_event_loop(
    &mut event_loop,
    &format!("globalThis.__digest_hex === \"{expected_hex}\""),
  )?;
  assert!(
    matches!(ok_value, Value::Bool(true)),
    "expected digest to match {expected_hex:?}; got {ok_value:?}"
  );
  Ok(())
}
