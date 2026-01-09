use super::event_loop::{EventLoop, TaskSource};
use super::promise::JsPromise;
use crate::error::{Error, Result};
use crate::resource::web_fetch::{
  execute_web_fetch, Headers as WebHeaders, Request as WebRequest, Response as WebResponse, WebFetchError,
  WebFetchExecutionContext,
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

  fn with_headers<R>(&self, f: impl FnOnce(&WebHeaders) -> std::result::Result<R, WebFetchError>) -> Result<R> {
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
