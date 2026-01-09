use fastrender::js::{
  fetch, EventLoop, FetchInit, HeadersInit, JsHeaders, JsPromiseValue, RunLimits, RunUntilIdleOutcome,
  TaskSource, VirtualClock, WebFetchHost,
};
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use fastrender::Result;
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
      .ok_or_else(|| fastrender::error::Error::Other(format!("no stubbed response for {url}")))
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
    fetcher: InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
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

    let _ = promise.then(event_loop, |_host, _event_loop, _response| Ok(JsPromiseValue::Value(())))?;
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
    fetcher: InMemoryFetcher::new().with_response("https://example.com/json", br#"{"ok": true}"#, 200),
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
