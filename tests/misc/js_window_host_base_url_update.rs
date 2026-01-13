use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{JsExecutionOptions, RunLimits, TaskSource, WindowHost};
use fastrender::resource::{FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vm_js::{PropertyKey, Value};

#[derive(Default)]
struct RecordingFetcher {
  routes: Mutex<HashMap<String, FetchedResource>>,
  request_urls: Mutex<Vec<String>>,
}

impl RecordingFetcher {
  fn insert(&self, url: &str, resource: FetchedResource) {
    self
      .routes
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .insert(url.to_string(), resource);
  }

  fn request_urls(&self) -> Vec<String> {
    self
      .request_urls
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn fetch_inner(&self, url: &str) -> Result<FetchedResource> {
    {
      let mut lock = self
        .request_urls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      lock.push(url.to_string());
    }
    self
      .routes
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no stubbed response for {url}")))
  }
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.fetch_inner(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch_inner(req.url)
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    self.fetch_inner(req.fetch.url)
  }
}

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time; keep a generous limit so tests remain stable
  // under parallel execution and CPU contention.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

fn get_global_string(host: &mut WindowHost, name: &str) -> Option<String> {
  let window = host.host_mut().window_mut();
  let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();

  scope.push_root(Value::Object(global)).ok()?;
  let key_s = scope.alloc_string(name).ok()?;
  scope.push_root(Value::String(key_s)).ok()?;
  let key = PropertyKey::from_string(key_s);

  let value = scope
    .heap()
    .object_get_own_data_property_value(global, &key)
    .ok()?
    .unwrap_or(Value::Undefined);
  match value {
    Value::String(s) => scope.heap().get_string(s).ok().map(|s| s.to_utf8_lossy()),
    _ => None,
  }
}

#[test]
fn window_host_base_url_propagates_to_document_base_uri_and_fetch() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/c",
    FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string())),
  );

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    // Intentionally not a directory URL so we can distinguish it from the base URL override.
    "https://example.com/original/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    js_opts_for_test(),
  )?;

  host
    .host_mut()
    .set_document_base_url(Some("https://example.com/a/b/".to_string()));

  host.exec_script(
    r#"
    globalThis.__href = new URL('c', document.baseURI).href;
    globalThis.__err = '';
    globalThis.__text = '';
    fetch('c')
      .then(r => r.text())
      .then(t => { globalThis.__text = t; })
      .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 10,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(get_global_string(&mut host, "__href").as_deref(), Some("https://example.com/a/b/c"));
  assert_eq!(get_global_string(&mut host, "__text").as_deref(), Some("ok"));
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/c".to_string()]);
  Ok(())
}

#[test]
fn window_host_base_url_propagates_to_dynamic_import_resolution() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/mod.js",
    FetchedResource::new(
      b"export default import.meta.url;".to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut options = js_opts_for_test();
  options.supports_module_scripts = true;

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/original/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    options,
  )?;

  host
    .host_mut()
    .set_document_base_url(Some("https://example.com/a/b/".to_string()));

  host.exec_script(
    r#"
    globalThis.__err = '';
    globalThis.__url = '';
    import('./mod.js')
      .then(m => { globalThis.__url = m.default; })
      .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 20,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(
    get_global_string(&mut host, "__url").as_deref(),
    Some("https://example.com/a/b/mod.js")
  );
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/mod.js".to_string()]);
  Ok(())
}

#[test]
fn window_host_script_url_propagates_to_dynamic_import_in_microtask() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/scriptdir/mod.js",
    FetchedResource::new(
      b"export default import.meta.url;".to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut options = js_opts_for_test();
  options.supports_module_scripts = true;

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    // Use a document URL in a different directory from the classic script URL so we can verify
    // Promise microtasks run with the correct incumbent script base.
    "https://example.com/docdir/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    options,
  )?;

  // Execute a classic script with a URL-like source name (simulates an external `<script src>`).
  host.queue_task(TaskSource::DOMManipulation, |host_state, event_loop| {
    host_state.exec_script_with_name_in_event_loop(
      event_loop,
      "https://example.com/scriptdir/script.js",
      r#"
      globalThis.__err = '';
      globalThis.__url = '';
      Promise.resolve()
        .then(() => import('./mod.js'))
        .then(m => { globalThis.__url = m.default; })
        .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
      "#,
    )?;
    Ok(())
  })?;

  host.run_until_idle(RunLimits {
    max_tasks: 20,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(
    get_global_string(&mut host, "__url").as_deref(),
    Some("https://example.com/scriptdir/mod.js")
  );
  assert_eq!(
    fetcher.request_urls(),
    vec!["https://example.com/scriptdir/mod.js".to_string()]
  );
  Ok(())
}

#[test]
fn window_host_cleared_base_url_falls_back_to_document_url_for_fetch() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/c",
    FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string())),
  );

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    // Use a URL with a filename to ensure path resolution uses the document URL when `base_url` is
    // cleared.
    "https://example.com/a/b/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    js_opts_for_test(),
  )?;

  // Clear the base URL override: `document.baseURI` should still be the document URL, and relative
  // fetch() URLs should resolve against it.
  host.host_mut().set_document_base_url(None);

  host.exec_script(
    r#"
    globalThis.__href = new URL('c', document.baseURI).href;
    globalThis.__err = '';
    globalThis.__text = '';
    fetch('c')
      .then(r => r.text())
      .then(t => { globalThis.__text = t; })
      .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 10,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(get_global_string(&mut host, "__href").as_deref(), Some("https://example.com/a/b/c"));
  assert_eq!(get_global_string(&mut host, "__text").as_deref(), Some("ok"));
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/c".to_string()]);
  Ok(())
}

#[test]
fn window_host_cleared_base_url_falls_back_to_document_url_for_dynamic_import() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/mod.js",
    FetchedResource::new(
      b"export default import.meta.url;".to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut options = js_opts_for_test();
  options.supports_module_scripts = true;

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    // Use a URL with a filename to ensure path resolution uses the document URL when `base_url` is
    // cleared.
    "https://example.com/a/b/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    options,
  )?;

  // Clear the base URL override: relative dynamic import specifiers should resolve against the
  // document URL (matching `document.baseURI` fallback semantics).
  host.host_mut().set_document_base_url(None);

  host.exec_script(
    r#"
    globalThis.__err = '';
    globalThis.__url = '';
    import('./mod.js')
      .then(m => { globalThis.__url = m.default; })
      .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 20,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(
    get_global_string(&mut host, "__url").as_deref(),
    Some("https://example.com/a/b/mod.js")
  );
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/mod.js".to_string()]);
  Ok(())
}

#[test]
fn window_host_cleared_base_url_falls_back_to_document_url_for_dynamic_import_in_microtask(
) -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/mod.js",
    FetchedResource::new(
      b"export default import.meta.url;".to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut options = js_opts_for_test();
  options.supports_module_scripts = true;

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    // Use a URL with a filename to ensure path resolution uses the document URL when `base_url` is
    // cleared.
    "https://example.com/a/b/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    options,
  )?;

  // Clear the base URL override: relative `import()` inside a microtask should still resolve
  // against the document URL (matching `document.baseURI` fallback semantics).
  host.host_mut().set_document_base_url(None);

  host.exec_script(
    r#"
    globalThis.__err = '';
    globalThis.__url = '';
    Promise.resolve()
      .then(() => import('./mod.js'))
      .then(m => { globalThis.__url = m.default; })
      .catch(e => { globalThis.__err = String(e && (e.stack || e.message) || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 20,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(
    get_global_string(&mut host, "__url").as_deref(),
    Some("https://example.com/a/b/mod.js")
  );
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/mod.js".to_string()]);
  Ok(())
}

#[test]
fn window_host_base_url_propagates_to_xhr_relative_urls() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/c",
    FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string())),
  );

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/original/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    js_opts_for_test(),
  )?;

  host
    .host_mut()
    .set_document_base_url(Some("https://example.com/a/b/".to_string()));

  host.exec_script(
    r#"
    globalThis.__err = '';
    globalThis.__text = '';
    try {
      const xhr = new XMLHttpRequest();
      xhr.open('GET', 'c', false);
      xhr.send();
      globalThis.__text = xhr.responseText;
    } catch (e) {
      globalThis.__err = String(e && (e.stack || e.message) || e);
    }
    "#,
  )?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(get_global_string(&mut host, "__text").as_deref(), Some("ok"));
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/c".to_string()]);
  Ok(())
}

#[test]
fn window_host_cleared_base_url_falls_back_to_document_url_for_xhr() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher = Arc::new(RecordingFetcher::default());
  fetcher.insert(
    "https://example.com/a/b/c",
    FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string())),
  );

  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/a/b/page.html",
    fetcher.clone() as Arc<dyn ResourceFetcher>,
    js_opts_for_test(),
  )?;

  host.host_mut().set_document_base_url(None);

  host.exec_script(
    r#"
    globalThis.__err = '';
    globalThis.__text = '';
    try {
      const xhr = new XMLHttpRequest();
      xhr.open('GET', 'c', false);
      xhr.send();
      globalThis.__text = xhr.responseText;
    } catch (e) {
      globalThis.__err = String(e && (e.stack || e.message) || e);
    }
    "#,
  )?;

  assert_eq!(get_global_string(&mut host, "__err").unwrap_or_default(), "");
  assert_eq!(get_global_string(&mut host, "__text").as_deref(), Some("ok"));
  assert_eq!(fetcher.request_urls(), vec!["https://example.com/a/b/c".to_string()]);
  Ok(())
}
