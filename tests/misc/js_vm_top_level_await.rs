use fastrender::dom2;
use fastrender::js::{EventLoop, JsExecutionOptions, VmJsModuleLoader, WindowHostState};
use fastrender::resource::{FetchedResource, FetchRequest, ResourceFetcher};
use fastrender::Result;
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::sync::Arc;
use vm_js::{Budget, PropertyKey, Value};

#[test]
fn vmjs_module_loader_top_level_await_microtask_resolves() -> Result<()> {
  #[derive(Debug)]
  struct MapFetcher {
    map: HashMap<String, FetchedResource>,
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .map
        .get(url)
        .cloned()
        .ok_or_else(|| fastrender::error::Error::Other(format!("no fixture for url {url}")))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.fetch(req.url)
    }
  }

  fn get_global_prop(host: &mut WindowHostState, name: &str) -> Value {
    let (_vm, realm_ref, heap) = host.window_mut().vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm_ref.global_object();
    scope.push_root(Value::Object(global)).expect("root global");
    let key_s = scope.alloc_string(name).expect("alloc name");
    scope.push_root(Value::String(key_s)).expect("root name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  let entry_url = "https://example.com/entry.js";
  let document_url = "https://example.com/index.html";

  let mut map: HashMap<String, FetchedResource> = HashMap::new();
  map.insert(
    entry_url.to_string(),
    FetchedResource::new(
      "globalThis.result = await Promise.resolve(123);"
        .as_bytes()
        .to_vec(),
      Some("application/javascript".to_string()),
    ),
  );
  let fetcher = Arc::new(MapFetcher { map });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut options = JsExecutionOptions::default();
  options.supports_module_scripts = true;
  let mut host = WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), options)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

  let mut loader = VmJsModuleLoader::new(fetcher, document_url);
  loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

  assert_eq!(get_global_prop(&mut host, "result"), Value::Number(123.0));
  Ok(())
}

#[test]
fn vmjs_module_loader_top_level_await_timer_resolves() -> Result<()> {
  #[derive(Debug)]
  struct MapFetcher {
    map: HashMap<String, FetchedResource>,
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .map
        .get(url)
        .cloned()
        .ok_or_else(|| fastrender::error::Error::Other(format!("no fixture for url {url}")))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.fetch(req.url)
    }
  }

  fn get_global_prop(host: &mut WindowHostState, name: &str) -> Value {
    let (_vm, realm_ref, heap) = host.window_mut().vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm_ref.global_object();
    scope.push_root(Value::Object(global)).expect("root global");
    let key_s = scope.alloc_string(name).expect("alloc name");
    scope.push_root(Value::String(key_s)).expect("root name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  let entry_url = "https://example.com/entry.js";
  let document_url = "https://example.com/index.html";

  let mut map: HashMap<String, FetchedResource> = HashMap::new();
  map.insert(
    entry_url.to_string(),
    FetchedResource::new(
      "await new Promise((resolve) => setTimeout(resolve, 0)); globalThis.done = true;"
        .as_bytes()
        .to_vec(),
      Some("application/javascript".to_string()),
    ),
  );
  let fetcher = Arc::new(MapFetcher { map });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut options = JsExecutionOptions::default();
  options.supports_module_scripts = true;
  let mut host = WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher.clone(), options)?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  host.window_mut().vm_mut().set_budget(Budget::unlimited(100));

  let mut loader = VmJsModuleLoader::new(fetcher, document_url);
  loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;
  assert_eq!(get_global_prop(&mut host, "done"), Value::Bool(true));
  Ok(())
}
