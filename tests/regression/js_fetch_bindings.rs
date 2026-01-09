#[path = "../../src/js/quickjs_fetch.rs"]
mod quickjs_fetch;

use std::sync::{Arc, Mutex};

use fastrender::error::Result;
use fastrender::resource::web_fetch::RequestRedirect;
use fastrender::resource::{origin_from_url, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use rquickjs::{Context, Runtime};

#[derive(Debug, Clone)]
struct CapturedRequest {
  url: String,
  method: String,
  redirect: RequestRedirect,
  headers: Vec<(String, String)>,
  body: Option<Vec<u8>>,
}

struct RecordingFetcher {
  response: FetchedResource,
  captured: Mutex<Vec<CapturedRequest>>,
}

impl RecordingFetcher {
  fn new(response: FetchedResource) -> Self {
    Self {
      response,
      captured: Mutex::new(Vec::new()),
    }
  }

  fn take_captured(&self) -> Vec<CapturedRequest> {
    std::mem::take(&mut *self.captured.lock().unwrap())
  }
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, _url: &str) -> Result<FetchedResource> {
    Ok(self.response.clone())
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.captured.lock().unwrap().push(CapturedRequest {
      url: req.url.to_string(),
      method: "GET".to_string(),
      redirect: RequestRedirect::Follow,
      headers: Vec::new(),
      body: None,
    });
    Ok(self.response.clone())
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    self.captured.lock().unwrap().push(CapturedRequest {
      url: req.fetch.url.to_string(),
      method: req.method.to_string(),
      redirect: req.redirect,
      headers: req.headers.to_vec(),
      body: req.body.map(|b| b.to_vec()),
    });
    Ok(self.response.clone())
  }
}

fn drain_microtasks(rt: &Runtime) {
  for _ in 0..1000 {
    match rt.execute_pending_job() {
      Ok(true) => continue,
      Ok(false) => break,
      Err(err) => panic!("execute_pending_job failed: {err}"),
    }
  }
}

#[test]
fn quickjs_fetch_resolves_and_json_parses() {
  let mut resource = FetchedResource::new(br#"{"hello":"world"}"#.to_vec(), None);
  resource.response_headers = Some(vec![("content-type".to_string(), "application/json".to_string())]);
  let fetcher = Arc::new(RecordingFetcher::new(resource));

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx
    .with(|ctx| {
      let globals = ctx.globals();
      quickjs_fetch::install_fetch_bindings(
        ctx.clone(),
        &globals,
        fetcher.clone(),
        quickjs_fetch::FetchHarnessConfig::default(),
      )
      .unwrap();

      ctx
        .eval::<(), _>(
          r#"
          globalThis.__out = "pending";
          fetch("https://example.com/data")
            .then((r) => r.json())
            .then((obj) => { globalThis.__out = obj.hello; })
            .catch((e) => { globalThis.__out = "err:" + String(e && e.name); });
        "#,
        )
        .unwrap();
      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();

  drain_microtasks(&rt);

  ctx
    .with(|ctx| {
      let out: String = ctx.eval("globalThis.__out").unwrap();
      assert_eq!(out, "world");
      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();
}

#[test]
fn quickjs_fetch_drops_forbidden_request_headers() {
  let fetcher = Arc::new(RecordingFetcher::new(FetchedResource::new(b"ok".to_vec(), None)));

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx
    .with(|ctx| {
      let globals = ctx.globals();
      quickjs_fetch::install_fetch_bindings(
        ctx.clone(),
        &globals,
        fetcher.clone(),
        quickjs_fetch::FetchHarnessConfig::default(),
      )
      .unwrap();

      ctx
        .eval::<(), _>(
          r#"
          globalThis.__out = "pending";
          fetch("https://example.com/submit", {
            method: "POST",
            headers: { "X-Test": "ok", "Cookie": "a=b" },
          })
            .then(() => { globalThis.__out = "ok"; })
            .catch((e) => { globalThis.__out = "err:" + String(e && e.name); });
        "#,
        )
        .unwrap();
      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();

  drain_microtasks(&rt);

  let captured = fetcher.take_captured();
  assert_eq!(captured.len(), 1, "expected 1 request, got {captured:?}");
  let req = &captured[0];
  assert_eq!(req.method.to_ascii_uppercase(), "POST");
  assert!(req.headers.iter().any(|(k, v)| k == "x-test" && v == "ok"));
  assert!(!req.headers.iter().any(|(k, _)| k == "cookie"));
}

#[test]
fn quickjs_fetch_rejects_on_cors_failure() {
  let fetcher = Arc::new(RecordingFetcher::new(FetchedResource::new(b"ok".to_vec(), None)));
  let origin = origin_from_url("https://client.example/").expect("origin");

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx
    .with(|ctx| {
      let globals = ctx.globals();
      quickjs_fetch::install_fetch_bindings(
        ctx.clone(),
        &globals,
        fetcher.clone(),
        quickjs_fetch::FetchHarnessConfig {
          referrer_url: None,
          client_origin: Some(origin.clone()),
        },
      )
      .unwrap();

      ctx
        .eval::<(), _>(
          r#"
          globalThis.__out = "pending";
          fetch("https://other.example/data")
            .then(() => { globalThis.__out = "resolved"; })
            .catch((e) => { globalThis.__out = String(e && e.name); });
        "#,
        )
        .unwrap();
      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();

  drain_microtasks(&rt);

  ctx
    .with(|ctx| {
      let out: String = ctx.eval("globalThis.__out").unwrap();
      assert_eq!(out, "TypeError");
      Ok::<(), rquickjs::Error>(())
    })
    .unwrap();
}
