use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::dom2::parse_html;
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome, WindowHostState};
use fastrender::resource::{
  origin_from_url, FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource,
  ResourceFetcher,
};
use fastrender::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vm_js::Value;

#[derive(Debug, Clone)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  client_origin: Option<String>,
  credentials_mode: FetchCredentialsMode,
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

fn run_dynamic_script_case(
  crossorigin_attr: Option<&str>,
  response_allow_origin: Option<&str>,
  response_allow_credentials: bool,
) -> Result<(RecordedRequest, bool, Vec<String>)> {
  let document_url = "https://example.com/index.html";
  let script_url = "https://cross.example/script.js";
  let html = "<!doctype html><html><head></head><body></body></html>";

  let expected_origin = origin_from_url(document_url)
    .expect("origin_from_url")
    .to_string();

  let mut resource = FetchedResource::with_final_url(
    b"globalThis.__ran = true;".to_vec(),
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

  let dom = parse_html(html)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let clock = event_loop.clock();
  let mut host = WindowHostState::new_with_fetcher_and_clock(dom, document_url, fetcher, clock)?;

  let mut source = format!(
    "globalThis.__ran = false;\n\
     const s = document.createElement('script');\n\
     s.src = '{script_url}';\n"
  );
  if let Some(value) = crossorigin_attr {
    source.push_str(&format!("s.setAttribute('crossorigin', '{value}');\n"));
  }
  source.push_str("document.body.appendChild(s);\n");

  host.exec_script_in_event_loop(&mut event_loop, &source)?;

  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
      errors.push(err.to_string());
    })?,
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

  let ran_value = host.exec_script_in_event_loop(&mut event_loop, "globalThis.__ran === true")?;
  let ran = matches!(ran_value, Value::Bool(true));
  Ok((request, ran, errors))
}

#[test]
fn vmjs_dynamic_script_crossorigin_missing_does_not_enforce_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    let (request, ran, errors) = run_dynamic_script_case(None, None, false)?;
    assert_eq!(request.destination, FetchDestination::Script);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert!(ran, "expected dynamic script to execute");
    assert!(errors.is_empty(), "expected no error; got {errors:?}");
    Ok(())
  })
}

#[test]
fn vmjs_dynamic_script_crossorigin_anonymous_enforces_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    let (request, ran, errors) = run_dynamic_script_case(Some("anonymous"), None, false)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::SameOrigin);
    assert!(!ran, "expected dynamic script to be blocked by CORS");
    assert_eq!(errors.len(), 1, "expected a CORS error; got {errors:?}");
    assert!(
      errors[0].contains("blocked by CORS"),
      "expected CORS error message; got {errors:?}"
    );
    Ok(())
  })
}

#[test]
fn vmjs_dynamic_script_crossorigin_anonymous_allows_wildcard_acao() -> Result<()> {
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    let (request, ran, errors) = run_dynamic_script_case(Some("anonymous"), Some("*"), false)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::SameOrigin);
    assert!(ran, "expected dynamic script to execute");
    assert!(errors.is_empty(), "expected no error; got {errors:?}");
    Ok(())
  })
}

#[test]
fn vmjs_dynamic_script_crossorigin_use_credentials_requires_acac_and_rejects_wildcard() -> Result<()>
{
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || {
    // Wildcard ACAO should fail for credentialed requests.
    let (request, ran, errors) = run_dynamic_script_case(Some("use-credentials"), Some("*"), true)?;
    assert_eq!(request.destination, FetchDestination::ScriptCors);
    assert_eq!(request.credentials_mode, FetchCredentialsMode::Include);
    assert!(!ran, "expected wildcard ACAO to be rejected");
    assert_eq!(errors.len(), 1, "expected a CORS error; got {errors:?}");

    // Matching origin requires Access-Control-Allow-Credentials: true.
    let (_request, ran, errors) =
      run_dynamic_script_case(Some("use-credentials"), Some("https://example.com"), false)?;
    assert!(!ran, "expected missing ACAC to fail");
    assert_eq!(errors.len(), 1, "expected a CORS error; got {errors:?}");

    let (_request, ran, errors) =
      run_dynamic_script_case(Some("use-credentials"), Some("https://example.com"), true)?;
    assert!(ran, "expected dynamic script to execute with ACAC=true");
    assert!(errors.is_empty(), "expected no error; got {errors:?}");
    Ok(())
  })
}
