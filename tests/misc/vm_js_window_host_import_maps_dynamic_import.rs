use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{JsExecutionOptions, RunLimits, WindowHost};
use fastrender::resource::{FetchRequest, FetchedResource, HttpFetcher, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use vm_js::{PropertyKey, Value};

#[derive(Default)]
struct DataOnlyFetcher;

impl ResourceFetcher for DataOnlyFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if url.trim_start().to_ascii_lowercase().starts_with("data:") {
      return HttpFetcher::new().fetch(url);
    }
    Err(Error::Other(format!(
      "DataOnlyFetcher only supports data: URLs (offline test); got {url:?}"
    )))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch(req.url)
  }
}

fn get_global_prop(host: &mut WindowHost, name: &str) -> Value {
  let window = host.host_mut().window_mut();
  let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope
    .push_root(Value::Object(global))
    .expect("push root global");
  let key_s = scope.alloc_string(name).expect("alloc prop name");
  scope
    .push_root(Value::String(key_s))
    .expect("push root prop name");
  let key = PropertyKey::from_string(key_s);
  scope
    .heap()
    .object_get_own_data_property_value(global, &key)
    .expect("get global prop")
    .unwrap_or(Value::Undefined)
}

fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
  let value = get_global_prop(host, name);
  let window = host.host_mut().window_mut();
  match value {
    Value::String(s) => Some(
      window
        .heap()
        .get_string(s)
        .expect("get string")
        .to_utf8_lossy(),
    ),
    _ => None,
  }
}

#[test]
fn window_host_dynamic_import_resolves_bare_specifiers_via_import_maps() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(DataOnlyFetcher::default());

  let mut options = JsExecutionOptions::default();
  options.supports_module_scripts = true;
  // Keep test deterministic under parallel `cargo test`: avoid wall-clock deadline false positives.
  options.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.invalid/base/",
    fetcher,
    options,
  )?;

  let module_source = r#"
    export const answer = 42;
    export function add(a, b) { return a + b; }
  "#;
  let encoded = BASE64_STANDARD.encode(module_source.as_bytes());
  let data_url = format!("data:application/javascript;base64,{encoded}");

  let import_map = format!(r#"{{"imports":{{"foo":"{data_url}"}}}}"#);
  let base_url = Url::parse("https://example.invalid/base/")
    .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;
  host
    .host_mut()
    .register_import_map_string(&import_map, &base_url)
    .map_err(|err| Error::Other(err.to_string()))?;

  host.exec_script(
    r#"
    globalThis.__result = null;
    globalThis.__err = "";

    import("foo")
      .then(m => { globalThis.__result = m.answer; })
      .catch(e => { globalThis.__err = String(e && e.message || e); });
    "#,
  )?;

  host.run_until_idle(RunLimits {
    max_tasks: 20,
    max_microtasks: 100,
    max_wall_time: Some(Duration::from_secs(5)),
  })?;

  assert_eq!(get_global_prop_utf8(&mut host, "__err").unwrap_or_default(), "");
  assert!(matches!(
    get_global_prop(&mut host, "__result"),
    Value::Number(n) if n == 42.0
  ));
  Ok(())
}

