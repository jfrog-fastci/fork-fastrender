use fastrender::dom2::NodeId;
use fastrender::error::{Error, Result};
use fastrender::js::{EventLoop, RunLimits, ScriptElementSpec};
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::{BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct LogExecutor {
  scripts: Arc<Mutex<Vec<String>>>,
}

impl LogExecutor {
  fn take_scripts(&self) -> Vec<String> {
    std::mem::take(
      &mut *self
        .scripts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
  }
}

impl BrowserTabJsExecutor for LogExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .scripts
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(script_text.to_string());
    Ok(())
  }
}

#[derive(Clone)]
struct RecordingFetcher {
  requests: Arc<Mutex<Vec<(FetchDestination, String)>>>,
  script_url: String,
  script_body: String,
}

impl RecordingFetcher {
  fn script_response(&self, url: &str) -> FetchedResource {
    let mut res = FetchedResource::new(
      self.script_body.as_bytes().to_vec(),
      Some("application/javascript".to_string()),
    );
    // Mirror HTTP fetches so downstream validations (status/CORS) remain deterministic.
    res.status = Some(200);
    res.final_url = Some(url.to_string());
    // Allow CORS-mode scripts/modules to pass enforcement when enabled.
    res.access_control_allow_origin = Some("*".to_string());
    res.access_control_allow_credentials = true;
    res
  }

  fn record(&self, destination: FetchDestination, url: &str) {
    self
      .requests
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push((destination, url.to_string()));
  }
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.record(FetchDestination::Other, url);
    if url == self.script_url {
      return Ok(self.script_response(url));
    }
    Err(Error::Other(format!(
      "unexpected fetch in RecordingFetcher: url={url:?}"
    )))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.record(req.destination, req.url);
    if req.url == self.script_url {
      return Ok(self.script_response(req.url));
    }
    Err(Error::Other(format!(
      "unexpected fetch in RecordingFetcher: destination={:?} url={:?}",
      req.destination, req.url
    )))
  }
}

#[test]
fn browser_tab_external_script_fetch_uses_injected_fetcher() -> Result<()> {
  let script_url = "https://example.invalid/external.js";
  let script_body = "console.log('from fetcher');";
  let html = format!(
    r#"<!doctype html><html><head><script src="{script_url}"></script></head><body></body></html>"#
  );

  let requests: Arc<Mutex<Vec<(FetchDestination, String)>>> = Arc::new(Mutex::new(Vec::new()));
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(RecordingFetcher {
    requests: Arc::clone(&requests),
    script_url: script_url.to_string(),
    script_body: script_body.to_string(),
  });

  let executor = LogExecutor::default();
  let mut tab = BrowserTab::from_html_with_fetcher(
    &html,
    RenderOptions::default(),
    executor.clone(),
    fetcher,
  )?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  let requested = requests
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone();
  assert!(
    requested
      .iter()
      .any(|(dest, url)| *dest == FetchDestination::Script && url == script_url),
    "expected script fetch request for {script_url:?}, got: {requested:?}",
  );

  assert_eq!(
    executor.take_scripts(),
    vec![script_body.to_string()],
    "expected fetched script body to be executed"
  );

  Ok(())
}

