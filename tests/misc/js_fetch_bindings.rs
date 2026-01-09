use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rquickjs::{Context, Runtime};

use fastrender::resource::{
  origin_from_url, FetchCredentialsMode, FetchDestination, FetchedResource,
  HttpRequest, ReferrerPolicy, ResourceFetcher,
};

#[path = "../../src/js/quickjs/fetch.rs"]
mod quickjs_fetch;

use quickjs_fetch::{install_fetch_bindings, QuickjsFetchEnv};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedHttpRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  credentials_mode: FetchCredentialsMode,
  method: String,
  headers: Vec<(String, String)>,
  body: Option<Vec<u8>>,
}

#[derive(Default)]
struct StubFetcher {
  responses: Mutex<HashMap<String, FetchedResource>>,
  last: Mutex<Option<CapturedHttpRequest>>,
}

impl StubFetcher {
  fn with_response(mut self, url: &str, resource: FetchedResource) -> Self {
    self.responses.get_mut().unwrap().insert(url.to_string(), resource);
    self
  }

  fn take_last(&self) -> Option<CapturedHttpRequest> {
    self.last.lock().unwrap().take()
  }
}

impl ResourceFetcher for StubFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    Ok(
      self
        .responses
        .lock()
        .unwrap()
        .get(url)
        .cloned()
        .unwrap_or_else(|| FetchedResource::new(Vec::new(), None)),
    )
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> fastrender::Result<FetchedResource> {
    let captured = CapturedHttpRequest {
      url: req.fetch.url.to_string(),
      destination: req.fetch.destination,
      referrer_url: req.fetch.referrer_url.map(str::to_string),
      credentials_mode: req.fetch.credentials_mode,
      method: req.method.to_string(),
      headers: req.headers.to_vec(),
      body: req.body.map(<[u8]>::to_vec),
    };
    *self.last.lock().unwrap() = Some(captured);
    self.fetch(req.fetch.url)
  }
}

fn drain_promise_jobs(rt: &Runtime) -> Result<(), String> {
  loop {
    match rt.execute_pending_job() {
      Ok(true) => continue,
      Ok(false) => return Ok(()),
      Err(err) => return Err(err.to_string()),
    }
  }
}

#[test]
fn fetch_text_roundtrip() {
  let fetcher = Arc::new(
    StubFetcher::default().with_response(
      "https://client.example/hello",
      FetchedResource::new(b"hello".to_vec(), Some("text/plain".to_string())),
    ),
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    let globals = ctx.globals();
    install_fetch_bindings(
      ctx.clone(),
      &globals,
      QuickjsFetchEnv {
        fetcher: fetcher.clone(),
        document_url: Some("https://client.example/page".to_string()),
        document_origin: origin_from_url("https://client.example/page"),
        referrer_policy: ReferrerPolicy::NoReferrer,
        access_policy: None,
      },
    )
    .unwrap();

    ctx
      .eval::<(), _>(
        r#"
        globalThis.__out = null;
        globalThis.__err = null;
        (async () => {
          try {
            const r = await fetch("https://client.example/hello");
            globalThis.__out = await r.text();
          } catch (e) {
            globalThis.__err = e;
          }
        })();
        "#,
      )
      .unwrap();
  });

  drain_promise_jobs(&rt).unwrap();

  ctx.with(|ctx| {
    let globals = ctx.globals();
    let err: Option<rquickjs::Value> = globals.get("__err").unwrap();
    assert!(err.is_none(), "unexpected error: {:?}", err);
    let out: String = globals.get("__out").unwrap();
    assert_eq!(out, "hello");
  });
}

#[test]
fn headers_methods_and_iteration_are_spec_shaped() {
  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    let globals = ctx.globals();
    install_fetch_bindings(
      ctx.clone(),
      &globals,
      QuickjsFetchEnv {
        fetcher: Arc::new(StubFetcher::default()),
        document_url: None,
        document_origin: None,
        referrer_policy: ReferrerPolicy::NoReferrer,
        access_policy: None,
      },
    )
    .unwrap();

    ctx
      .eval::<(), _>(
        r#"
        const h = new Headers([["X-Test", "a"], ["a-test", "b"], ["x-test", "c"], ["Set-Cookie", "x=y"], ["set-cookie", "a=b"]]);
        if (h.get("x-test") !== "a, c") throw new Error("get() should combine duplicates");
        if (h.has("x-test") !== true) throw new Error("has() should be true");
        const cookies = h.getSetCookie();
        if (cookies.length !== 2 || cookies[0] !== "x=y" || cookies[1] !== "a=b") throw new Error("getSetCookie ordering");
        h.set("x-test", "z");
        if (h.get("x-test") !== "z") throw new Error("set() should replace");
        h.append("x-test", "y");
        if (h.get("x-test") !== "z, y") throw new Error("append() should add");
        h.delete("a-test");
        if (h.has("a-test")) throw new Error("delete() should remove");

        const iter = Array.from(h);
        // Deterministic: sort_and_combine lowercases and sorts by name.
        if (iter[0][0] !== "set-cookie" || iter[1][0] !== "set-cookie" || iter[2][0] !== "x-test") {
          throw new Error("iterator ordering");
        }
        "#,
      )
      .unwrap();
  });
}

#[test]
fn request_init_propagates_to_fetcher() {
  let fetcher = Arc::new(
    StubFetcher::default().with_response(
      "https://client.example/echo",
      FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string())),
    ),
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    let globals = ctx.globals();
    install_fetch_bindings(
      ctx.clone(),
      &globals,
      QuickjsFetchEnv {
        fetcher: fetcher.clone(),
        document_url: Some("https://client.example/page".to_string()),
        document_origin: origin_from_url("https://client.example/page"),
        referrer_policy: ReferrerPolicy::NoReferrer,
        access_policy: None,
      },
    )
    .unwrap();

    ctx
      .eval::<(), _>(
        r#"
        (async () => {
          const r = await fetch("https://client.example/echo", {
            method: "POST",
            headers: { "X-Test": "hello" },
            body: "payload",
            credentials: "include",
          });
          await r.text();
        })();
        "#,
      )
      .unwrap();
  });

  drain_promise_jobs(&rt).unwrap();

  let captured = fetcher.take_last().expect("expected captured request");
  assert_eq!(captured.url, "https://client.example/echo");
  assert_eq!(captured.destination, FetchDestination::Fetch);
  assert_eq!(
    captured.referrer_url.as_deref(),
    Some("https://client.example/page")
  );
  assert_eq!(captured.credentials_mode, FetchCredentialsMode::Include);
  assert_eq!(captured.method, "POST");
  assert!(
    captured.headers.iter().any(|(k, v)| k == "x-test" && v == "hello"),
    "missing user header in {:?}",
    captured.headers
  );
  assert_eq!(captured.body, Some(b"payload".to_vec()));
}

#[test]
fn cors_failure_rejects_with_type_error() {
  let fetcher = Arc::new(
    StubFetcher::default().with_response(
      "https://other.example/res",
      // Missing access_control_allow_origin triggers CORS rejection.
      FetchedResource::new(b"blocked".to_vec(), Some("text/plain".to_string())),
    ),
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    let globals = ctx.globals();
    install_fetch_bindings(
      ctx.clone(),
      &globals,
      QuickjsFetchEnv {
        fetcher: fetcher.clone(),
        document_url: Some("https://client.example/page".to_string()),
        document_origin: origin_from_url("https://client.example/page"),
        referrer_policy: ReferrerPolicy::NoReferrer,
        access_policy: None,
      },
    )
    .unwrap();

    ctx
      .eval::<(), _>(
        r#"
        globalThis.__cors = null;
        (async () => {
          try {
            await fetch("https://other.example/res");
            globalThis.__cors = false;
          } catch (e) {
            globalThis.__cors = (e instanceof TypeError);
          }
        })();
        "#,
      )
      .unwrap();
  });

  drain_promise_jobs(&rt).unwrap();

  ctx.with(|ctx| {
    let globals = ctx.globals();
    let cors: bool = globals.get("__cors").unwrap();
    assert!(cors, "expected CORS failure to reject with TypeError");
  });
}

#[test]
fn response_body_used_rejects_second_consumption() {
  let fetcher = Arc::new(
    StubFetcher::default().with_response(
      "https://client.example/once",
      FetchedResource::new(b"hello".to_vec(), Some("text/plain".to_string())),
    ),
  );

  let rt = Runtime::new().unwrap();
  let ctx = Context::full(&rt).unwrap();
  ctx.with(|ctx| {
    let globals = ctx.globals();
    install_fetch_bindings(
      ctx.clone(),
      &globals,
      QuickjsFetchEnv {
        fetcher: fetcher.clone(),
        document_url: Some("https://client.example/page".to_string()),
        document_origin: origin_from_url("https://client.example/page"),
        referrer_policy: ReferrerPolicy::NoReferrer,
        access_policy: None,
      },
    )
    .unwrap();

    ctx
      .eval::<(), _>(
        r#"
        globalThis.__bodyUsedOk = null;
        (async () => {
          const r = await fetch("https://client.example/once");
          const t1 = await r.text();
          if (t1 !== "hello") throw new Error("unexpected first text");
          if (r.bodyUsed !== true) throw new Error("bodyUsed should flip");
          try {
            await r.text();
            globalThis.__bodyUsedOk = false;
          } catch (e) {
            globalThis.__bodyUsedOk = (e instanceof TypeError);
          }
        })();
        "#,
      )
      .unwrap();
  });

  drain_promise_jobs(&rt).unwrap();

  ctx.with(|ctx| {
    let globals = ctx.globals();
    let ok: bool = globals.get("__bodyUsedOk").unwrap();
    assert!(ok, "expected second text() call to reject with TypeError");
  });
}
