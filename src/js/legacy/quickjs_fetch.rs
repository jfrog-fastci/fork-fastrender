//! QuickJS harness bindings for WHATWG Fetch (`fetch()` + minimal `Response`).
//!
//! This module is **test-only scaffolding** used to validate the Rust Fetch core in
//! `src/resource/web_fetch` against JavaScript-facing expectations.
//!
//! It intentionally keeps the JS binding surface small and self-contained so it can be replaced by
//! IDL-generated bindings later.
#![cfg(all(test, feature = "quickjs"))]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::resource::web_fetch::{
  execute_web_fetch, Body, Request, RequestCredentials, RequestMode, RequestRedirect, Response,
  WebFetchExecutionContext,
};
use crate::resource::{DocumentOrigin, ResourceFetcher};
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::Opt;
use rquickjs::{Array, Class, Ctx, Error, Exception, Function, Object, Value};

#[derive(Debug, Clone, Default)]
pub struct FetchHarnessConfig {
  pub referrer_url: Option<String>,
  pub client_origin: Option<DocumentOrigin>,
}

#[derive(Clone)]
#[rquickjs::class(rename = "Response")]
struct JsResponse {
  inner: Rc<RefCell<Response>>,
}

impl<'js> Trace<'js> for JsResponse {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> rquickjs::JsLifetime<'js> for JsResponse {
  type Changed<'to> = JsResponse;
}

#[rquickjs::methods]
impl JsResponse {
  #[qjs(constructor)]
  pub fn new<'js>(ctx: Ctx<'js>) -> Result<Self, Error> {
    // The real Fetch `Response` constructor supports many overloads. For this test-only harness we
    // only need host-created `Response` objects returned by `fetch()`.
    Err(Exception::throw_type(&ctx, "Illegal constructor"))
  }

  #[qjs(get)]
  pub fn status(&self) -> u16 {
    self.inner.borrow().status
  }

  #[qjs(get)]
  pub fn ok(&self) -> bool {
    let status = self.inner.borrow().status;
    (200..300).contains(&status)
  }

  #[qjs(get)]
  pub fn url(&self) -> String {
    self.inner.borrow().url.clone()
  }

  #[qjs(get)]
  pub fn redirected(&self) -> bool {
    self.inner.borrow().redirected
  }

  pub fn __consume_text<'js>(&self, ctx: Ctx<'js>) -> Result<String, Error> {
    let mut response = self.inner.borrow_mut();
    let Some(body) = response.body.as_mut() else {
      return Ok(String::new());
    };
    body
      .text_utf8()
      .map_err(|err| Exception::throw_type(&ctx, &format!("Failed to read response body: {err}")))
  }
}

fn parse_mode<'js>(ctx: &Ctx<'js>, raw: &str) -> Result<RequestMode, Error> {
  match raw {
    "cors" => Ok(RequestMode::Cors),
    "no-cors" => Ok(RequestMode::NoCors),
    "same-origin" => Ok(RequestMode::SameOrigin),
    "navigate" => Ok(RequestMode::Navigate),
    _ => Err(Exception::throw_type(ctx, "Invalid fetch mode")),
  }
}

fn parse_credentials<'js>(ctx: &Ctx<'js>, raw: &str) -> Result<RequestCredentials, Error> {
  match raw {
    "omit" => Ok(RequestCredentials::Omit),
    "same-origin" => Ok(RequestCredentials::SameOrigin),
    "include" => Ok(RequestCredentials::Include),
    _ => Err(Exception::throw_type(ctx, "Invalid fetch credentials")),
  }
}

fn parse_redirect<'js>(ctx: &Ctx<'js>, raw: &str) -> Result<RequestRedirect, Error> {
  match raw {
    "follow" => Ok(RequestRedirect::Follow),
    "error" => Ok(RequestRedirect::Error),
    "manual" => Ok(RequestRedirect::Manual),
    _ => Err(Exception::throw_type(ctx, "Invalid fetch redirect")),
  }
}

fn apply_headers_init<'js>(
  ctx: &Ctx<'js>,
  request: &mut Request,
  init: Value<'js>,
) -> Result<(), Error> {
  if init.is_array() {
    let array = Array::from_value(init)?;
    let len = array.len();
    let mut pairs: Vec<Vec<String>> = Vec::with_capacity(len);
    for idx in 0..len {
      let pair_val: Value = array.get(idx)?;
      let pair = Array::from_value(pair_val)
        .map_err(|_| Exception::throw_type(ctx, "Invalid fetch headers init"))?;
      if pair.len() != 2 {
        return Err(Exception::throw_type(ctx, "Invalid fetch headers init"));
      }
      let name: String = pair.get(0)?;
      let value: String = pair.get(1)?;
      pairs.push(vec![name, value]);
    }
    request
      .headers
      .fill_from_sequence(pairs)
      .map_err(|err| Exception::throw_type(ctx, &format!("Invalid headers: {err}")))?;
    return Ok(());
  }

  if init.is_object() {
    let obj = Object::from_value(init)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    for key in obj.keys::<String>() {
      let key = key?;
      let value: String = obj.get(key.as_str())?;
      pairs.push((key, value));
    }
    request
      .headers
      .fill_from_pairs(pairs)
      .map_err(|err| Exception::throw_type(ctx, &format!("Invalid headers: {err}")))?;
    return Ok(());
  }

  Err(Exception::throw_type(ctx, "Invalid fetch headers init"))
}

fn apply_body_init<'js>(
  ctx: &Ctx<'js>,
  request: &mut Request,
  init: Value<'js>,
) -> Result<(), Error> {
  if init.is_null() || init.is_undefined() {
    return Ok(());
  }
  let body: String = init.get()?;
  request.body = Some(
    Body::new(body.into_bytes())
      .map_err(|err| Exception::throw_type(ctx, &format!("Invalid fetch body: {err}")))?,
  );
  Ok(())
}

pub fn install_fetch_bindings<'js>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
  fetcher: Arc<dyn ResourceFetcher>,
  config: FetchHarnessConfig,
) -> Result<(), Error> {
  Class::<JsResponse>::define(globals)?;

  let config = Arc::new(config);
  globals.set(
    "__fastrender_fetch_sync",
    Function::new(ctx.clone(), {
      let fetcher = Arc::clone(&fetcher);
      let config = Arc::clone(&config);
      move |ctx: Ctx<'js>, input: Value<'js>, init: Opt<Value<'js>>| -> Result<Value<'js>, Error> {
        let url: String = if input.is_string() {
          input.get()?
        } else if input.is_object() {
          let obj = Object::from_value(input)?;
          let url: Option<String> = obj.get("url")?;
          url.ok_or_else(|| Exception::throw_type(&ctx, "Invalid fetch input"))?
        } else {
          return Err(Exception::throw_type(&ctx, "Invalid fetch input"));
        };

        let mut method = "GET".to_string();
        let mut mode = RequestMode::Cors;
        let mut credentials = RequestCredentials::SameOrigin;
        let mut redirect = RequestRedirect::Follow;
        let mut headers_init: Option<Value<'js>> = None;
        let mut body_init: Option<Value<'js>> = None;

        if let Some(init) = init.0.filter(|v| !v.is_undefined() && !v.is_null()) {
          let init_obj = Object::from_value(init)
            .map_err(|_| Exception::throw_type(&ctx, "Invalid fetch init"))?;

          if let Some(v) = init_obj.get::<_, Option<Value<'js>>>("method")? {
            if !v.is_undefined() && !v.is_null() {
              method = v.get::<String>()?;
            }
          }

          if let Some(v) = init_obj.get::<_, Option<Value<'js>>>("mode")? {
            if !v.is_undefined() && !v.is_null() {
              mode = parse_mode(&ctx, &v.get::<String>()?)?;
            }
          }

          if let Some(v) = init_obj.get::<_, Option<Value<'js>>>("credentials")? {
            if !v.is_undefined() && !v.is_null() {
              credentials = parse_credentials(&ctx, &v.get::<String>()?)?;
            }
          }

          if let Some(v) = init_obj.get::<_, Option<Value<'js>>>("redirect")? {
            if !v.is_undefined() && !v.is_null() {
              redirect = parse_redirect(&ctx, &v.get::<String>()?)?;
            }
          }

          headers_init = init_obj.get::<_, Option<Value<'js>>>("headers")?;
          body_init = init_obj.get::<_, Option<Value<'js>>>("body")?;
        }

        let mut request = Request::new(method, url);
        request.mode = mode;
        request.credentials = credentials;
        request.redirect = redirect;

        if let Some(headers_init) = headers_init.filter(|v| !v.is_undefined() && !v.is_null()) {
          apply_headers_init(&ctx, &mut request, headers_init)?;
        }

        if let Some(body_init) = body_init.filter(|v| !v.is_undefined() && !v.is_null()) {
          apply_body_init(&ctx, &mut request, body_init)?;
        }

        let exec_ctx = WebFetchExecutionContext {
          referrer_url: config.referrer_url.as_deref(),
          client_origin: config.client_origin.as_ref(),
          ..WebFetchExecutionContext::default()
        };

        let response = execute_web_fetch(fetcher.as_ref(), &request, exec_ctx)
          .map_err(|err| Exception::throw_type(&ctx, &format!("Failed to fetch: {err}")))?;

        let inst = Class::instance(
          ctx.clone(),
          JsResponse {
            inner: Rc::new(RefCell::new(response)),
          },
        )?;
        Ok(inst.into_value())
      }
    })?,
  )?;

  ctx.eval::<(), _>(
    r#"
    (() => {
      if (globalThis.__fastrender_fetch_installed) return;
      globalThis.__fastrender_fetch_installed = true;

      const fetchSync = globalThis.__fastrender_fetch_sync;
      if (typeof fetchSync !== "function") {
        throw new Error("missing fetch host function");
      }

      globalThis.fetch = function (input, init) {
        return Promise.resolve().then(() => fetchSync(input, init));
      };

      if (typeof globalThis.Response === "function") {
        Response.prototype.text = function () {
          return Promise.resolve().then(() => this.__consume_text());
        };
        Response.prototype.json = function () {
          return this.text().then((t) => JSON.parse(t));
        };
      }
    })();
    "#,
  )?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{install_fetch_bindings, FetchHarnessConfig};

  use crate::error::Result;
  use crate::resource::web_fetch::RequestRedirect;
  use crate::resource::{
    origin_from_url, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher,
  };
  use rquickjs::{Context, Runtime};
  use std::sync::{Arc, Mutex};

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
    resource.response_headers = Some(vec![(
      "content-type".to_string(),
      "application/json".to_string(),
    )]);
    let fetcher = Arc::new(RecordingFetcher::new(resource));

    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx
      .with(|ctx| {
        let globals = ctx.globals();
        install_fetch_bindings(
          ctx.clone(),
          &globals,
          fetcher.clone(),
          FetchHarnessConfig::default(),
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
        install_fetch_bindings(
          ctx.clone(),
          &globals,
          fetcher.clone(),
          FetchHarnessConfig::default(),
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
        install_fetch_bindings(
          ctx.clone(),
          &globals,
          fetcher.clone(),
          FetchHarnessConfig {
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
}
