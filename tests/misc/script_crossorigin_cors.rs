use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::dom2::{Document, NodeId};
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome};
use fastrender::resource::{
  origin_from_url, FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource,
  ResourceFetcher,
};
use fastrender::web::events::{
  AddEventListenerOptions, DomError, Event, EventListenerInvoker, EventTargetId, ListenerId,
};
use fastrender::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, Error, RenderOptions,
  Result,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn find_element_by_id(dom: &Document, target: &str) -> NodeId {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    if dom.id(id).ok().flatten() == Some(target) {
      return id;
    }
    let node = dom.node(id);
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={target:?}");
}

#[derive(Debug, Clone)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  client_origin: Option<String>,
  credentials_mode: FetchCredentialsMode,
}

#[derive(Debug, Clone)]
struct RecordedInvocation {
  listener_id: ListenerId,
  event_type: String,
  event_target: Option<EventTargetId>,
  current_target: Option<EventTargetId>,
  is_trusted: bool,
}

#[derive(Clone)]
struct RecordingExecutor {
  executed: Arc<Mutex<Vec<String>>>,
}

impl BrowserTabJsExecutor for RecordingExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .executed
      .lock()
      .expect("executor log lock")
      .push(script_text.to_string());
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .executed
      .lock()
      .expect("executor log lock")
      .push(script_text.to_string());
    Ok(())
  }
}

struct RecordingInvoker {
  invocations: Arc<Mutex<Vec<RecordedInvocation>>>,
}

impl EventListenerInvoker for RecordingInvoker {
  fn invoke(
    &mut self,
    listener_id: ListenerId,
    event: &mut Event,
  ) -> std::result::Result<(), DomError> {
    self
      .invocations
      .lock()
      .expect("invocations lock")
      .push(RecordedInvocation {
        listener_id,
        event_type: event.type_.clone(),
        event_target: event.target,
        current_target: event.current_target,
        is_trusted: event.is_trusted,
      });
    Ok(())
  }
}

struct MockScriptFetcher {
  expected_url: String,
  recorded: Arc<Mutex<Vec<RecordedRequest>>>,
  response: FetchedResource,
}

impl ResourceFetcher for MockScriptFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "MockScriptFetcher.fetch unexpectedly called for {url:?}"
    )))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    assert_eq!(
      req.url, self.expected_url,
      "unexpected script fetch URL (got {}, expected {})",
      req.url, self.expected_url
    );
    self
      .recorded
      .lock()
      .expect("recorded request lock")
      .push(RecordedRequest {
        url: req.url.to_string(),
        destination: req.destination,
        referrer_url: req.referrer_url.map(|v| v.to_string()),
        client_origin: req.client_origin.map(|o| o.to_string()),
        credentials_mode: req.credentials_mode,
      });
    Ok(self.response.clone())
  }
}

fn run_script_case(
  crossorigin_attr: Option<&str>,
  response_allow_origin: Option<&str>,
  response_allow_credentials: bool,
) -> Result<(
  NodeId,
  RecordedRequest,
  Vec<String>,
  Vec<RecordedInvocation>,
)> {
  let document_url = "https://example.com/index.html";
  let script_url = "https://cross.example/script.js";
  let crossorigin_snippet = crossorigin_attr
    .map(|v| format!("crossorigin=\"{v}\""))
    .unwrap_or_default();

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <script id="s" async src="{script_url}" {crossorigin_snippet}></script>
        </head>
      </html>"#
  );

  let expected_origin = origin_from_url(document_url)
    .expect("origin_from_url")
    .to_string();

  let mut resource = FetchedResource::with_final_url(
    b"// script body".to_vec(),
    Some("text/javascript".to_string()),
    Some(script_url.to_string()),
  );
  resource.status = Some(200);
  resource.access_control_allow_origin = response_allow_origin.map(|v| v.to_string());
  resource.access_control_allow_credentials = response_allow_credentials;

  let recorded_requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(MockScriptFetcher {
    expected_url: script_url.to_string(),
    recorded: Arc::clone(&recorded_requests),
    response: resource,
  });

  let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
  let executor = RecordingExecutor {
    executed: Arc::clone(&executed),
  };

  let invocations: Arc<Mutex<Vec<RecordedInvocation>>> = Arc::new(Mutex::new(Vec::new()));

  let options = RenderOptions::default();
  let mut tab = BrowserTab::from_html_with_document_url_and_fetcher(
    &html,
    document_url,
    options,
    executor,
    fetcher,
  )?;

  let script_node = find_element_by_id(tab.dom(), "s");
  tab.dom_mut().events_mut().add_event_listener(
    EventTargetId::Node(script_node),
    "error",
    ListenerId::new(1),
    AddEventListenerOptions::default(),
  );

  tab.set_event_listener_invoker(Box::new(RecordingInvoker {
    invocations: Arc::clone(&invocations),
  }));

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let requests = recorded_requests.lock().expect("requests lock").clone();
  assert_eq!(
    requests.len(),
    1,
    "expected exactly one script fetch; got {requests:?}"
  );
  let request = requests[0].clone();
  assert_eq!(request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );

  let executed = executed.lock().expect("executed lock").clone();
  let invocations = invocations.lock().expect("invocations lock").clone();
  Ok((script_node, request, executed, invocations))
}

#[test]
fn script_crossorigin_missing_does_not_enforce_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    let (_script_node, request, executed, invocations) = run_script_case(None, None, false)?;
    assert_eq!(request.destination, FetchDestination::Script);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert_eq!(executed.len(), 1, "expected script to execute");
    assert!(
      invocations.is_empty(),
      "expected no error event; got {invocations:?}"
    );
    Ok(())
  })
}

#[test]
fn script_crossorigin_anonymous_enforces_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    // Missing ACAO should fail.
    let (script_node, request, executed, invocations) =
      run_script_case(Some("anonymous"), None, false)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::SameOrigin);
    assert!(executed.is_empty(), "expected script to be blocked by CORS");
    assert_eq!(invocations.len(), 1, "expected an error event");
    assert_eq!(invocations[0].event_type, "error");
    assert_eq!(
      invocations[0].event_target,
      Some(EventTargetId::Node(script_node)),
      "expected `error` event target to be the <script> element"
    );
    assert_eq!(
      invocations[0].current_target,
      Some(EventTargetId::Node(script_node)),
      "expected `error` event currentTarget to be the <script> element"
    );
    assert!(
      invocations[0].is_trusted,
      "expected script error event to be trusted"
    );
    Ok(())
  })
}

#[test]
fn script_crossorigin_anonymous_allows_wildcard_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    let (_script_node, request, executed, invocations) =
      run_script_case(Some("anonymous"), Some("*"), false)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::SameOrigin);
    assert_eq!(executed.len(), 1, "expected script to execute");
    assert!(
      invocations.is_empty(),
      "expected no error event; got {invocations:?}"
    );
    Ok(())
  })
}

#[test]
fn script_crossorigin_use_credentials_requires_acac_and_rejects_wildcard() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    // Wildcard ACAO should fail for credentialed requests.
    let (_script_node, request, executed, invocations) =
      run_script_case(Some("use-credentials"), Some("*"), true)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert!(executed.is_empty(), "expected wildcard ACAO to be rejected");
    assert_eq!(
      invocations.len(),
      1,
      "expected an error event for wildcard ACAO"
    );

    // Matching origin requires Access-Control-Allow-Credentials: true.
    let (_script_node, request, executed, invocations) =
      run_script_case(Some("use-credentials"), Some("https://example.com"), false)?;
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert!(executed.is_empty(), "expected missing ACAC to fail");
    assert_eq!(
      invocations.len(),
      1,
      "expected an error event for missing ACAC"
    );

    let (_script_node, request, executed, invocations) =
      run_script_case(Some("use-credentials"), Some("https://example.com"), true)?;
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert_eq!(
      executed.len(),
      1,
      "expected script to execute with ACAC=true"
    );
    assert!(
      invocations.is_empty(),
      "expected no error event; got {invocations:?}"
    );
    Ok(())
  })
}
