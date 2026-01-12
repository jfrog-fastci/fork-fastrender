use super::event_loop::{EventLoop, TaskSource};
use super::promise::JsPromise;
use crate::error::{Error, Result};
use crate::resource::web_fetch::{
  execute_web_fetch, Headers as WebHeaders, Request as WebRequest, Response as WebResponse,
  WebFetchError, WebFetchExecutionContext,
};
use crate::resource::ResourceFetcher;
use std::cell::RefCell;
use std::rc::Rc;

fn type_error(message: impl Into<String>) -> Error {
  Error::Other(format!("TypeError: {}", message.into()))
}

fn syntax_error(message: impl Into<String>) -> Error {
  Error::Other(format!("SyntaxError: {}", message.into()))
}

fn map_web_fetch_error(err: WebFetchError) -> Error {
  match err {
    WebFetchError::BodyInvalidJson(err) => syntax_error(err.to_string()),
    other => type_error(other.to_string()),
  }
}

fn map_web_fetch_result<T>(result: std::result::Result<T, WebFetchError>) -> Result<T> {
  result.map_err(map_web_fetch_error)
}

/// Host surface required for JS `fetch()` integration.
pub trait WebFetchHost {
  fn resource_fetcher(&self) -> &dyn ResourceFetcher;

  fn web_fetch_execution_context(&self) -> WebFetchExecutionContext<'_> {
    WebFetchExecutionContext::default()
  }
}

/// Minimal `HeadersInit` representation for deterministic Fetch bindings tests.
pub enum HeadersInit {
  Pairs(Vec<(String, String)>),
  Record(Vec<(String, String)>),
  Headers(JsHeaders),
}

#[derive(Clone)]
pub enum JsHeaders {
  Owned(Rc<RefCell<WebHeaders>>),
  Request(Rc<RefCell<WebRequest>>),
  Response(Rc<RefCell<WebResponse>>),
}

impl JsHeaders {
  pub fn new(init: Option<HeadersInit>) -> Result<Self> {
    let mut headers = WebHeaders::new();
    if let Some(init) = init {
      match init {
        HeadersInit::Pairs(pairs) | HeadersInit::Record(pairs) => {
          map_web_fetch_result(headers.fill_from_pairs(pairs))?;
        }
        HeadersInit::Headers(existing) => {
          let pairs = existing.sort_and_combine()?;
          map_web_fetch_result(headers.fill_from_pairs(pairs))?;
        }
      }
    }
    Ok(JsHeaders::Owned(Rc::new(RefCell::new(headers))))
  }

  fn with_headers<R>(
    &self,
    f: impl FnOnce(&WebHeaders) -> std::result::Result<R, WebFetchError>,
  ) -> Result<R> {
    match self {
      JsHeaders::Owned(h) => map_web_fetch_result(f(&h.borrow())),
      JsHeaders::Request(r) => map_web_fetch_result(f(&r.borrow().headers)),
      JsHeaders::Response(r) => map_web_fetch_result(f(&r.borrow().headers)),
    }
  }

  fn with_headers_mut<R>(
    &self,
    f: impl FnOnce(&mut WebHeaders) -> std::result::Result<R, WebFetchError>,
  ) -> Result<R> {
    match self {
      JsHeaders::Owned(h) => map_web_fetch_result(f(&mut h.borrow_mut())),
      JsHeaders::Request(r) => map_web_fetch_result(f(&mut r.borrow_mut().headers)),
      JsHeaders::Response(r) => map_web_fetch_result(f(&mut r.borrow_mut().headers)),
    }
  }

  pub fn append(&self, name: &str, value: &str) -> Result<()> {
    self.with_headers_mut(|headers| headers.append(name, value))
  }

  pub fn set(&self, name: &str, value: &str) -> Result<()> {
    self.with_headers_mut(|headers| headers.set(name, value))
  }

  pub fn get(&self, name: &str) -> Result<Option<String>> {
    self.with_headers(|headers| headers.get(name))
  }

  pub fn get_set_cookie(&self) -> Result<Vec<String>> {
    self.with_headers(|headers| Ok(headers.get_set_cookie()))
  }

  pub fn has(&self, name: &str) -> Result<bool> {
    self.with_headers(|headers| headers.has(name))
  }

  pub fn delete(&self, name: &str) -> Result<()> {
    self.with_headers_mut(|headers| headers.delete(name))
  }

  pub fn for_each(&self, mut f: impl FnMut(&str, &str)) -> Result<()> {
    let pairs = self.sort_and_combine()?;
    for (name, value) in &pairs {
      f(name, value);
    }
    Ok(())
  }

  pub fn sort_and_combine(&self) -> Result<Vec<(String, String)>> {
    self.with_headers(|headers| Ok(headers.sort_and_combine()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn js_headers_get_set_cookie_returns_values_in_order() -> Result<()> {
    let headers = JsHeaders::new(Some(HeadersInit::Pairs(vec![
      ("set-cookie".to_string(), "a=b".to_string()),
      ("x-test".to_string(), "1".to_string()),
      ("Set-Cookie".to_string(), "c=d".to_string()),
    ])))?;

    assert_eq!(
      headers.get_set_cookie()?,
      vec!["a=b".to_string(), "c=d".to_string()]
    );
    Ok(())
  }

  mod js_fetch_tests {
    use super::*;
    use crate::js::{JsPromiseValue, RunLimits, RunUntilIdleOutcome, VirtualClock};
    use crate::resource::{
      FetchDestination, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct StubResponse {
      bytes: Vec<u8>,
      status: u16,
    }

    #[derive(Debug)]
    struct InMemoryFetcher {
      routes: HashMap<String, StubResponse>,
      last_request_headers: Mutex<Vec<(String, String)>>,
    }

    impl InMemoryFetcher {
      fn new() -> Self {
        Self {
          routes: HashMap::new(),
          last_request_headers: Mutex::new(Vec::new()),
        }
      }

      fn with_response(mut self, url: &str, bytes: impl Into<Vec<u8>>, status: u16) -> Self {
        self.routes.insert(
          url.to_string(),
          StubResponse {
            bytes: bytes.into(),
            status,
          },
        );
        self
      }

      fn lookup(&self, url: &str) -> Result<StubResponse> {
        self
          .routes
          .get(url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("no stubbed response for {url}")))
      }

      fn last_request_headers(&self) -> Vec<(String, String)> {
        self
          .last_request_headers
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .clone()
      }
    }

    impl Default for InMemoryFetcher {
      fn default() -> Self {
        Self::new()
      }
    }

    impl ResourceFetcher for InMemoryFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        // Delegate through `fetch_http_request` so tests stay representative of the JS fetch path.
        let fetch = FetchRequest::new(url, FetchDestination::Fetch);
        self.fetch_http_request(HttpRequest::new(fetch, "GET"))
      }

      fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
        {
          let mut lock = self
            .last_request_headers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
          *lock = req.headers.to_vec();
        }

        let stub = self.lookup(req.fetch.url)?;
        let mut resource = FetchedResource::new(stub.bytes, None);
        resource.status = Some(stub.status);
        // Echo request headers back as response headers so tests can assert request header forwarding
        // without inspecting internal adapter plumbing.
        resource.response_headers = Some(req.headers.to_vec());
        Ok(resource)
      }
    }

    #[derive(Default)]
    struct Host {
      fetcher: InMemoryFetcher,
      log: Vec<String>,
      observed_json_ok: Option<bool>,
    }

    impl WebFetchHost for Host {
      fn resource_fetcher(&self) -> &dyn ResourceFetcher {
        &self.fetcher
      }
    }

    #[test]
    fn fetch_text_orders_microtasks_before_networking() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock);

      let mut host = Host {
        fetcher: InMemoryFetcher::new().with_response("https://example.com/x", b"hello", 200),
        ..Host::default()
      };

      event_loop.queue_task(TaskSource::Script, |host, event_loop| {
        host.log.push("sync".to_string());
        event_loop.queue_microtask(|host, _| {
          host.log.push("micro".to_string());
          Ok(())
        })?;

        let promise = fetch(host, event_loop, "https://example.com/x", None)?;
        let promise = promise.then(event_loop, |_host, event_loop, response| {
          Ok(JsPromiseValue::Promise(response.text(event_loop)?))
        })?;
        let _ = promise.then(event_loop, |host, _event_loop, text| {
          host.log.push(text);
          Ok(JsPromiseValue::Value(()))
        })?;
        Ok(())
      })?;

      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.log, vec!["sync", "micro", "hello"]);
      Ok(())
    }

    #[test]
    fn fetch_forwards_request_headers() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock);

      let mut host = Host {
        fetcher: InMemoryFetcher::new()
          .with_response("https://example.com/headers", b"ok", 200),
        ..Host::default()
      };

      event_loop.queue_task(TaskSource::Script, |host, event_loop| {
        let headers = JsHeaders::new(Some(HeadersInit::Pairs(vec![(
          "x-test".to_string(),
          "1".to_string(),
        )])))?;

        let promise = fetch(
          host,
          event_loop,
          "https://example.com/headers",
          Some(FetchInit {
            method: None,
            headers: Some(HeadersInit::Headers(headers)),
          }),
        )?;

        let _ = promise.then(event_loop, |_host, _event_loop, _response| {
          Ok(JsPromiseValue::Value(()))
        })?;
        Ok(())
      })?;

      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert!(
        host
          .fetcher
          .last_request_headers()
          .iter()
          .any(|(name, value)| name == "x-test" && value == "1"),
        "expected ResourceFetcher::fetch_http_request to receive x-test: 1"
      );
      Ok(())
    }

    #[test]
    fn fetch_response_json_parses_body() -> Result<()> {
      let clock = Arc::new(VirtualClock::new());
      let mut event_loop = EventLoop::<Host>::with_clock(clock);

      let mut host = Host {
        fetcher: InMemoryFetcher::new().with_response(
          "https://example.com/json",
          br#"{"ok": true}"#,
          200,
        ),
        ..Host::default()
      };

      event_loop.queue_task(TaskSource::Script, |host, event_loop| {
        let promise = fetch(host, event_loop, "https://example.com/json", None)?;
        let promise = promise.then(event_loop, |_host, event_loop, response| {
          Ok(JsPromiseValue::Promise(response.json(event_loop)?))
        })?;
        let _ = promise.then(event_loop, |host, _event_loop, value| {
          host.observed_json_ok = value.get("ok").and_then(|v| v.as_bool());
          Ok(JsPromiseValue::Value(()))
        })?;
        Ok(())
      })?;

      assert_eq!(
        event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
      assert_eq!(host.observed_json_ok, Some(true));
      Ok(())
    }
  }
}

#[derive(Clone)]
pub struct JsRequest(Rc<RefCell<WebRequest>>);

pub struct RequestInit {
  pub method: Option<String>,
  pub headers: Option<HeadersInit>,
}

impl JsRequest {
  pub fn new(url: &str, init: Option<RequestInit>) -> Result<Self> {
    let mut request = WebRequest::new("GET", url);

    if let Some(init) = init {
      if let Some(method) = init.method {
        request.method = method;
      }
      if let Some(headers) = init.headers {
        let pairs = match headers {
          HeadersInit::Pairs(pairs) | HeadersInit::Record(pairs) => pairs,
          HeadersInit::Headers(existing) => existing.sort_and_combine()?,
        };
        map_web_fetch_result(request.headers.fill_from_pairs(pairs))?;
      }
    }

    Ok(Self(Rc::new(RefCell::new(request))))
  }

  pub fn method(&self) -> String {
    self.0.borrow().method.clone()
  }

  pub fn url(&self) -> String {
    self.0.borrow().url.clone()
  }

  pub fn headers(&self) -> JsHeaders {
    JsHeaders::Request(Rc::clone(&self.0))
  }

  fn snapshot(&self) -> WebRequest {
    self.0.borrow().clone()
  }
}

#[derive(Clone)]
pub struct JsResponse(Rc<RefCell<WebResponse>>);

impl JsResponse {
  fn from_core(response: WebResponse) -> Self {
    Self(Rc::new(RefCell::new(response)))
  }

  pub fn ok(&self) -> bool {
    let status = self.0.borrow().status;
    (200..=299).contains(&status)
  }

  pub fn status(&self) -> u16 {
    self.0.borrow().status
  }

  pub fn status_text(&self) -> String {
    self.0.borrow().status_text.clone()
  }

  pub fn url(&self) -> String {
    self.0.borrow().url.clone()
  }

  pub fn headers(&self) -> JsHeaders {
    JsHeaders::Response(Rc::clone(&self.0))
  }

  pub fn text<Host: 'static>(
    &self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<JsPromise<Host, String>>
  where
    Host: 'static,
  {
    let (promise, resolver) = JsPromise::<Host, String>::new();
    let response = self.clone();
    event_loop.queue_microtask(move |_host, event_loop| {
      let result = {
        let mut response = response.0.borrow_mut();
        match response.body.as_mut() {
          Some(body) => body.text_utf8().map_err(map_web_fetch_error),
          None => Ok(String::new()),
        }
      };
      match result {
        Ok(text) => resolver.resolve(event_loop, text)?,
        Err(err) => resolver.reject(event_loop, err)?,
      }
      Ok(())
    })?;
    Ok(promise)
  }

  pub fn json<Host: 'static>(
    &self,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<JsPromise<Host, serde_json::Value>>
  where
    Host: 'static,
  {
    let (promise, resolver) = JsPromise::<Host, serde_json::Value>::new();
    let response = self.clone();
    event_loop.queue_microtask(move |_host, event_loop| {
      let result = {
        let mut response = response.0.borrow_mut();
        let Some(body) = response.body.as_mut() else {
          return Ok(resolver.reject(event_loop, type_error("Response body is null"))?);
        };
        body.json().map_err(map_web_fetch_error)
      };
      match result {
        Ok(value) => resolver.resolve(event_loop, value)?,
        Err(err) => resolver.reject(event_loop, err)?,
      }
      Ok(())
    })?;
    Ok(promise)
  }
}

pub struct FetchInit {
  pub method: Option<String>,
  pub headers: Option<HeadersInit>,
}

/// Minimal, event-loop-integrated `fetch()` implementation.
///
/// Supported schemes depend on the underlying [`ResourceFetcher`] implementation (commonly:
/// `http(s)`, `file`, and `data`).
pub fn fetch<Host>(
  _host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  input: &str,
  init: Option<FetchInit>,
) -> Result<JsPromise<Host, JsResponse>>
where
  Host: WebFetchHost + 'static,
{
  let request = JsRequest::new(
    input,
    init.map(|init| RequestInit {
      method: init.method,
      headers: init.headers,
    }),
  )?;

  let (promise, resolver) = JsPromise::<Host, JsResponse>::new();
  let request_snapshot = request.snapshot();

  event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
    let fetcher = host.resource_fetcher();
    let ctx = host.web_fetch_execution_context();

    let result = execute_web_fetch(fetcher, &request_snapshot, ctx);
    match result {
      Ok(response) => {
        let response = JsResponse::from_core(response);
        let resolver = resolver.clone();
        event_loop.queue_microtask(move |_host, event_loop| {
          resolver.resolve(event_loop, response)?;
          Ok(())
        })?;
      }
      Err(err) => {
        // Fetch spec: network errors reject with a TypeError.
        let err = type_error(format!("fetch failed: {err}"));
        let resolver = resolver.clone();
        event_loop.queue_microtask(move |_host, event_loop| {
          resolver.reject(event_loop, err)?;
          Ok(())
        })?;
      }
    }

    Ok(())
  })?;

  // Prevent unused warnings for callers that don't need access to the request wrapper yet.
  let _ = request;

  Ok(promise)
}
