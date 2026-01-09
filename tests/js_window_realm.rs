use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
use fastrender::js::{
  ScriptBlockExecutor, ScriptOrchestrator, ScriptType, WindowHostState, WindowRealm, WindowRealmConfig,
};
use fastrender::{Error, Result};
use vm_js::{Heap, PropertyKey, Scope, Value, Vm, VmError};

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn get_data_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
  let key_s = scope.alloc_string(name).unwrap();
  let key = PropertyKey::from_string(key_s);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
    .unwrap()
    .unwrap()
}

fn find_script_elements(dom: &Dom2Document) -> Vec<NodeId> {
  dom
    .subtree_preorder(dom.root())
    .filter(|&id| matches!(&dom.node(id).kind, NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script")))
    .collect()
}

fn get_current_script(vm: &mut Vm, heap: &mut Heap, document_obj: vm_js::GcObject) -> Result<Value> {
  let mut scope = heap.scope();
  let key_s = scope.alloc_string("currentScript").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  vm.get(&mut scope, document_obj, key)
    .map_err(|e| Error::Other(e.to_string()))
}

fn get_wrapper_node_id(
  vm: &mut Vm,
  heap: &mut Heap,
  wrapper: vm_js::GcObject,
) -> Result<usize> {
  let mut scope = heap.scope();
  let key_s = scope
    .alloc_string("__fastrender_node_id")
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  let value = vm
    .get(&mut scope, wrapper, key)
    .map_err(|e| Error::Other(e.to_string()))?;
  let Value::Number(n) = value else {
    return Err(Error::Other("expected __fastrender_node_id to be a number".to_string()));
  };
  Ok(n as usize)
}

#[test]
fn window_self_and_document_url_are_exposed() -> Result<()> {
  let url = "https://example.com/";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let global = realm.global_object();
  let (_vm, heap) = realm.vm_and_heap_mut();
  let mut scope = heap.scope();

  let window = get_data_prop(&mut scope, global, "window");
  let self_ = get_data_prop(&mut scope, global, "self");
  assert_eq!(window, Value::Object(global));
  assert_eq!(self_, Value::Object(global));

  let document = get_data_prop(&mut scope, global, "document");
  let Value::Object(document_obj) = document else {
    panic!("expected document to be an object");
  };

  let doc_url = get_data_prop(&mut scope, document_obj, "URL");
  assert_eq!(get_string(scope.heap(), doc_url), url);
  Ok(())
}

#[test]
fn document_current_script_tracks_sequential_classic_scripts() -> Result<()> {
  #[derive(Default)]
  struct RecordingExecutor {
    observed: Vec<usize>,
  }

  impl ScriptBlockExecutor<WindowHostState> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut WindowHostState,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      let realm = host.window_mut();
      let global = realm.global_object();
      let (vm, heap) = realm.vm_and_heap_mut();
      let document_obj = {
        let mut scope = heap.scope();
        let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
          return Err(Error::Other("document is not an object".to_string()));
        };
        doc
      };

      let value = get_current_script(vm, heap, document_obj)?;
      let Value::Object(wrapper) = value else {
        return Err(Error::Other("expected document.currentScript to be an object".to_string()));
      };
      let node_id = get_wrapper_node_id(vm, heap, wrapper)?;
      self.observed.push(node_id);
      Ok(())
    }
  }

  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script></script><script></script>")?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let scripts = find_script_elements(host.dom());
  assert_eq!(scripts.len(), 2);

  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = RecordingExecutor::default();

  // Outside execution, currentScript should be null.
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(
    &mut host,
    scripts[0],
    ScriptType::Classic,
    &mut executor,
  )?;
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(
    &mut host,
    scripts[1],
    ScriptType::Classic,
    &mut executor,
  )?;
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  assert_eq!(
    executor.observed,
    vec![scripts[0].index(), scripts[1].index()]
  );
  Ok(())
}

#[test]
fn location_href_setter_errors_deterministically() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let global = realm.global_object();
  let (vm, heap) = realm.vm_and_heap_mut();
  let mut scope = heap.scope();

  let location = get_data_prop(&mut scope, global, "location");
  let Value::Object(location_obj) = location else {
    panic!("expected location to be an object");
  };

  let href_key_s = scope.alloc_string("href").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(href_key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let href_key = PropertyKey::from_string(href_key_s);

  let new_url_s = scope
    .alloc_string("https://example.com/next")
    .map_err(|e| Error::Other(e.to_string()))?;
  let new_value = Value::String(new_url_s);

  let err = scope
    .ordinary_set(vm, location_obj, href_key, new_value, Value::Object(location_obj))
    .expect_err("expected location.href setter to fail");
  assert!(
    matches!(err, VmError::TypeError(msg) if msg == "Navigation via location.href is not implemented yet"),
    "unexpected error: {err:?}"
  );
  Ok(())
}

#[test]
fn js_execution_can_observe_window_globals() -> Result<()> {
  let url = "https://example.com/path";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("window === globalThis && self === window")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(value, Value::Bool(true));

  let value = realm
    .exec_script("document.URL")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), url);

  let value = realm
    .exec_script("location.href")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), url);
  Ok(())
}

#[test]
fn document_current_script_is_visible_to_js_execution() -> Result<()> {
  #[derive(Default)]
  struct JsExecutor {
    observed: Vec<usize>,
    wrapper_identity_ok: Vec<bool>,
  }

  impl ScriptBlockExecutor<WindowHostState> for JsExecutor {
    fn execute_script(
      &mut self,
      host: &mut WindowHostState,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      let realm = host.window_mut();

      let stable = realm
        .exec_script("document.currentScript === document.currentScript")
        .map_err(|e| Error::Other(e.to_string()))?;
      let Value::Bool(stable) = stable else {
        return Err(Error::Other(
          "expected document.currentScript identity check to return a bool".to_string(),
        ));
      };
      self.wrapper_identity_ok.push(stable);

      let node_id = realm
        .exec_script("document.currentScript.__fastrender_node_id")
        .map_err(|e| Error::Other(e.to_string()))?;
      let Value::Number(n) = node_id else {
        return Err(Error::Other(
          "expected document.currentScript.__fastrender_node_id to be a number".to_string(),
        ));
      };
      let as_usize = n as usize;
      if (as_usize as f64) != n {
        return Err(Error::Other(format!(
          "expected document.currentScript.__fastrender_node_id to be an integer, got {n:?}"
        )));
      }
      self.observed.push(as_usize);
      Ok(())
    }
  }

  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script></script><script></script>")?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let scripts = find_script_elements(host.dom());
  assert_eq!(scripts.len(), 2);

  // Outside execution, currentScript is null.
  {
    let realm = host.window_mut();
    let value = realm
      .exec_script("document.currentScript")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(value, Value::Null);
  }

  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = JsExecutor::default();

  orchestrator.execute_script_element(
    &mut host,
    scripts[0],
    ScriptType::Classic,
    &mut executor,
  )?;
  orchestrator.execute_script_element(
    &mut host,
    scripts[1],
    ScriptType::Classic,
    &mut executor,
  )?;

  assert_eq!(executor.wrapper_identity_ok, vec![true, true]);
  assert_eq!(executor.observed, vec![scripts[0].index(), scripts[1].index()]);
  Ok(())
}
