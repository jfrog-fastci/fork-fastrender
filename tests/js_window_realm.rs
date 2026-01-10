use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
use fastrender::js::runtime::with_event_loop;
use fastrender::js::{
  EventLoop, RunLimits, RunUntilIdleOutcome, ScriptBlockExecutor, ScriptOrchestrator, ScriptType, TaskSource,
  VirtualClock, WindowFetchEnv, WindowHost, WindowHostState, WindowRealm, WindowRealmConfig, WindowRealmHost,
};
use fastrender::resource::{
  FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher,
};
use fastrender::resource::web_fetch::WebFetchLimits;
use fastrender::render_control;
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vm_js::{Heap, PropertyKey, Scope, Value, Vm, VmError};

fn install_vm_js_microtask_checkpoint_hook<Host: WindowRealmHost>(event_loop: &mut EventLoop<Host>) {
  fn drain<Host: WindowRealmHost>(host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
    with_event_loop(event_loop, || {
      let realm = host.window_realm();
      realm.reset_interrupt();
      let (vm, heap) = realm.vm_and_heap_mut();
      vm.perform_microtask_checkpoint(heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      Ok(())
    })
  }

  event_loop.set_microtask_checkpoint_hook(Some(drain::<Host>));
}

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn format_vm_error(heap: &mut Heap, err: VmError) -> String {
  if let Some(value) = err.thrown_value() {
    if let Value::String(s) = value {
      if let Ok(js) = heap.get_string(s) {
        return js.to_utf8_lossy();
      }
    }

    if let Value::Object(obj) = value {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(obj)).ok();

      let mut get_prop_str = |name: &str| -> Option<String> {
        let key_s = scope.alloc_string(name).ok()?;
        scope.push_root(Value::String(key_s)).ok()?;
        let key = PropertyKey::from_string(key_s);
        let value = scope
          .heap()
          .object_get_own_data_property_value(obj, &key)
          .ok()?
          .unwrap_or(Value::Undefined);
        match value {
          Value::String(s) => Some(scope.heap().get_string(s).ok()?.to_utf8_lossy()),
          _ => None,
        }
      };

      let name = get_prop_str("name");
      let message = get_prop_str("message");
      match (name, message) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name,
        (_, Some(message)) => message,
        _ => "uncaught exception".to_string(),
      }
    } else {
      "uncaught exception".to_string()
    }
  } else {
    match err {
      VmError::Syntax(diags) => format!("syntax error: {diags:?}"),
      other => other.to_string(),
    }
  }
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
fn strict_script_top_level_this_is_window() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
"use strict";
globalThis.__strict_this_ok = (this === window) && (this === globalThis);
"#,
  )?;

  let strict_this_ok = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__strict_this_ok")
  };
  assert_eq!(strict_this_ok, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_jobs_and_queue_microtask_preserve_fifo_order() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__log = "";
Promise.resolve().then(() => { globalThis.__log += "p1,"; });
queueMicrotask(() => { globalThis.__log += "qm,"; });
Promise.resolve().then(() => { globalThis.__log += "p2,"; });
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "p1,qm,p2,");
  Ok(())
}

#[test]
fn named_scripts_route_promise_jobs_through_event_loop_microtasks() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHostState::new(dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_with_name_in_event_loop(
    &mut event_loop,
    "<test named script>",
    r#"
globalThis.__log = "";
Promise.resolve().then(() => { globalThis.__log += "p1,"; });
queueMicrotask(() => { globalThis.__log += "qm,"; });
Promise.resolve().then(() => { globalThis.__log += "p2,"; });
"#,
  )?;

  let before = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let after = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "p1,qm,p2,");
  Ok(())
}

#[test]
fn promise_jobs_abort_when_render_deadline_is_expired() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__ran = false;
Promise.resolve().then(() => { globalThis.__ran = true; });
"#,
  )?;

  // Install an already-expired render deadline so the VM callback budget has no time remaining.
  // Promise jobs are host-owned microtasks; they must not leak roots or run once the deadline is
  // exceeded.
  let deadline =
    render_control::RenderDeadline::new(Some(std::time::Duration::from_millis(0)), None);
  let _guard = render_control::DeadlineGuard::install(Some(&deadline));

  let _err = host
    .perform_microtask_checkpoint()
    .expect_err("expected microtask checkpoint to fail under expired deadline");

  let ran = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__ran")
  };
  assert_eq!(ran, Value::Bool(false));
  Ok(())
}

#[test]
fn promise_any_resolves_first_fulfilled_value() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__result = "";
Promise.any(["a", "b"]).then(
  function (v) { globalThis.__result = v; },
  function () { globalThis.__result = "rejected"; }
);
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__result");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__result");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "a");
  Ok(())
}

#[test]
fn promise_any_rejects_with_aggregate_error_when_all_reject() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__err_name = "";
globalThis.__err0 = "";
Promise.any([Promise.reject("x"), Promise.reject("y")]).then(
  function () { globalThis.__err_name = "resolved"; },
  function (e) {
    globalThis.__err_name = e && e.name;
    globalThis.__err0 = e && e.errors && e.errors[0];
  }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let (name, err0) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let name = get_data_prop(&mut scope, global, "__err_name");
    let err0 = get_data_prop(&mut scope, global, "__err0");
    (get_string(scope.heap(), name), get_string(scope.heap(), err0))
  };

  assert_eq!(name, "AggregateError");
  assert_eq!(err0, "x");
  Ok(())
}

#[test]
fn promise_all_settled_reports_fulfilled_and_rejected_entries() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__status0 = "";
globalThis.__value0 = "";
globalThis.__status1 = "";
globalThis.__reason1 = "";
Promise.allSettled([Promise.resolve("a"), Promise.reject("b")]).then(function (res) {
  globalThis.__status0 = res[0].status;
  globalThis.__value0 = res[0].value;
  globalThis.__status1 = res[1].status;
  globalThis.__reason1 = res[1].reason;
});
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let (status0, value0, status1, reason1) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let status0 = get_data_prop(&mut scope, global, "__status0");
    let value0 = get_data_prop(&mut scope, global, "__value0");
    let status1 = get_data_prop(&mut scope, global, "__status1");
    let reason1 = get_data_prop(&mut scope, global, "__reason1");
    (
      get_string(scope.heap(), status0),
      get_string(scope.heap(), value0),
      get_string(scope.heap(), status1),
      get_string(scope.heap(), reason1),
    )
  };

  assert_eq!(status0, "fulfilled");
  assert_eq!(value0, "a");
  assert_eq!(status1, "rejected");
  assert_eq!(reason1, "b");
  Ok(())
}

#[test]
fn promise_all_resolves_values_in_input_order() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.all([Promise.resolve("a"), "b"]).then(
  function (res) { globalThis.__out = res[0] + "," + res[1]; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "a,b");
  Ok(())
}

#[test]
fn promise_all_rejects_with_first_rejection_reason() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.all([Promise.reject("x"), Promise.resolve("y")]).then(
  function () { globalThis.__out = "resolved"; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "rejected:x");
  Ok(())
}

#[test]
fn promise_race_resolves_first_settled_value() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.race([Promise.resolve("a"), Promise.resolve("b")]).then(
  function (v) { globalThis.__out = "resolved:" + v; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "resolved:a");
  Ok(())
}

#[test]
fn promise_race_rejects_first_rejection_reason() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.race([Promise.reject("x"), Promise.resolve("y")]).then(
  function (v) { globalThis.__out = "resolved:" + v; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "rejected:x");
  Ok(())
}

#[test]
fn location_url_components_are_exposed_to_js_execution() -> Result<()> {
  let url = "https://example.com:8080/path/to/page?query=1#hash";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      "location.protocol + '|' + location.host + '|' + location.hostname + '|' + location.port + '|' + location.pathname + '|' + location.search + '|' + location.hash + '|' + location.origin",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), value),
    "https:|example.com:8080|example.com|8080|/path/to/page|?query=1|#hash|https://example.com:8080"
  );
  Ok(())
}

#[test]
fn document_head_and_body_reflect_dom_ids() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head id=h></head><body id=b></body></html>",
  )?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r#"
  globalThis.__head_id = document.head.id;
  globalThis.__body_id = document.body.id;
  globalThis.__head_same = document.head === document.head;
  globalThis.__body_same = document.body === document.body;
  document.body.id = "new";
  globalThis.__body_id_after = document.body.id;
  "#,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

  let (head_id, body_id, body_id_after, head_same, body_same) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let head_value = get_data_prop(&mut scope, global, "__head_id");
    let body_value = get_data_prop(&mut scope, global, "__body_id");
    let body_after_value = get_data_prop(&mut scope, global, "__body_id_after");
    let head_same = get_data_prop(&mut scope, global, "__head_same");
    let body_same = get_data_prop(&mut scope, global, "__body_same");
    (
      get_string(scope.heap(), head_value),
      get_string(scope.heap(), body_value),
      get_string(scope.heap(), body_after_value),
      head_same,
      body_same,
    )
  };

  assert_eq!(head_id, "h");
  assert_eq!(body_id, "b");
  assert_eq!(body_id_after, "new");
  assert_eq!(head_same, Value::Bool(true));
  assert_eq!(body_same, Value::Bool(true));

  let body_node = host
    .dom()
    .body()
    .expect("expected body element to exist for HTML document");
  assert_eq!(host.dom().element_id(body_node), "new");

  Ok(())
}

#[test]
fn document_get_element_by_id_returns_stable_wrapper() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div id=x></div></body></html>",
  )?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r#"
  globalThis.__missing = document.getElementById("missing") === null;
  globalThis.__empty = document.getElementById("") === null;
  const el = document.getElementById("x");
  globalThis.__same = el === document.getElementById("x");
  globalThis.__id_before = el.id;
  el.id = "y";
  globalThis.__old_missing = document.getElementById("x") === null;
  const el2 = document.getElementById("y");
  globalThis.__same_after = el === el2;
  globalThis.__id_after = el2.id;
  "#,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

  let (missing, empty, same, old_missing, same_after, id_before, id_after) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let missing = get_data_prop(&mut scope, global, "__missing");
    let empty = get_data_prop(&mut scope, global, "__empty");
    let same = get_data_prop(&mut scope, global, "__same");
    let old_missing = get_data_prop(&mut scope, global, "__old_missing");
    let same_after = get_data_prop(&mut scope, global, "__same_after");
    let id_before_v = get_data_prop(&mut scope, global, "__id_before");
    let id_after_v = get_data_prop(&mut scope, global, "__id_after");
    (
      missing,
      empty,
      same,
      old_missing,
      same_after,
      get_string(scope.heap(), id_before_v),
      get_string(scope.heap(), id_after_v),
    )
  };

  assert_eq!(missing, Value::Bool(true));
  assert_eq!(empty, Value::Bool(true));
  assert_eq!(same, Value::Bool(true));
  assert_eq!(old_missing, Value::Bool(true));
  assert_eq!(same_after, Value::Bool(true));
  assert_eq!(id_before, "x");
  assert_eq!(id_after, "y");

  assert!(host.dom().get_element_by_id("x").is_none());
  let node = host
    .dom()
    .get_element_by_id("y")
    .expect("expected DOM to reflect updated id");
  assert_eq!(host.dom().element_id(node), "y");

  Ok(())
}

#[test]
fn document_query_selector_returns_stable_wrapper() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div class=x id=a></div></body></html>",
  )?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r###"
  const el = document.querySelector(".x");
  globalThis.__qs_found = (el !== null);
  globalThis.__qs_same = (el === document.querySelector(".x"));
  globalThis.__qs_id = el && el.getAttribute("id");
  try {
    document.querySelector("##");
    globalThis.__qs_bad = "no";
  } catch (e) {
    globalThis.__qs_bad = e.name;
  }
  "###,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

  let (qs_found, qs_same, qs_id, qs_bad) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let found = get_data_prop(&mut scope, global, "__qs_found");
    let same = get_data_prop(&mut scope, global, "__qs_same");
    let id = get_data_prop(&mut scope, global, "__qs_id");
    let bad = get_data_prop(&mut scope, global, "__qs_bad");
    (
      found,
      same,
      get_string(scope.heap(), id),
      get_string(scope.heap(), bad),
    )
  };

  assert_eq!(qs_found, Value::Bool(true));
  assert_eq!(qs_same, Value::Bool(true));
  assert_eq!(qs_id, "a");
  assert_eq!(qs_bad, "SyntaxError");

  Ok(())
}

#[test]
fn element_query_selector_all_and_matches_closest_work() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body>\
     <div id=a class=wrap><span id=a_inner class='inner other'></span></div>\
     <div id=b class=wrap><span id=b_inner class=inner></span></div>\
     </body></html>",
  )?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r###"
  const a = document.getElementById("a");
  const inner = a.querySelector(".inner");
  globalThis.__el_qs_id = inner && inner.id;

  const doc_all = document.querySelectorAll(".inner");
  globalThis.__doc_all_len = doc_all.length;
  globalThis.__doc_all_0 = doc_all[0] && doc_all[0].id;
  globalThis.__doc_all_1 = doc_all[1] && doc_all[1].id;

  const a_all = a.querySelectorAll(".inner");
  globalThis.__a_all_len = a_all.length;
  globalThis.__a_all_0 = a_all[0] && a_all[0].id;

  globalThis.__scope_same = (a.querySelector(":scope") === a);
  globalThis.__a_matches = a.matches("div.wrap");
  globalThis.__inner_matches = inner.matches("div span.inner");
  globalThis.__closest_ok = (inner.closest("#a") === a);

  try {
    a.querySelectorAll("##");
    globalThis.__bad_qsa = "no";
  } catch (e) {
    globalThis.__bad_qsa = e.name;
  }
  try {
    inner.matches("##");
    globalThis.__bad_matches = "no";
  } catch (e) {
    globalThis.__bad_matches = e.name;
  }
  try {
    inner.closest("##");
    globalThis.__bad_closest = "no";
  } catch (e) {
    globalThis.__bad_closest = e.name;
  }
  "###,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

  let (
    el_qs_id,
    doc_all_len,
    doc_all_0,
    doc_all_1,
    a_all_len,
    a_all_0,
    scope_same,
    a_matches,
    inner_matches,
    closest_ok,
    bad_qsa,
    bad_matches,
    bad_closest,
  ) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let el_qs_id_v = get_data_prop(&mut scope, global, "__el_qs_id");
    let doc_all_len = get_data_prop(&mut scope, global, "__doc_all_len");
    let doc_all_0_v = get_data_prop(&mut scope, global, "__doc_all_0");
    let doc_all_1_v = get_data_prop(&mut scope, global, "__doc_all_1");
    let a_all_len = get_data_prop(&mut scope, global, "__a_all_len");
    let a_all_0_v = get_data_prop(&mut scope, global, "__a_all_0");
    let scope_same = get_data_prop(&mut scope, global, "__scope_same");
    let a_matches = get_data_prop(&mut scope, global, "__a_matches");
    let inner_matches = get_data_prop(&mut scope, global, "__inner_matches");
    let closest_ok = get_data_prop(&mut scope, global, "__closest_ok");
    let bad_qsa_v = get_data_prop(&mut scope, global, "__bad_qsa");
    let bad_matches_v = get_data_prop(&mut scope, global, "__bad_matches");
    let bad_closest_v = get_data_prop(&mut scope, global, "__bad_closest");

    let heap = scope.heap();
    (
      get_string(heap, el_qs_id_v),
      doc_all_len,
      get_string(heap, doc_all_0_v),
      get_string(heap, doc_all_1_v),
      a_all_len,
      get_string(heap, a_all_0_v),
      scope_same,
      a_matches,
      inner_matches,
      closest_ok,
      get_string(heap, bad_qsa_v),
      get_string(heap, bad_matches_v),
      get_string(heap, bad_closest_v),
    )
  };

  assert_eq!(el_qs_id, "a_inner");
  assert_eq!(doc_all_len, Value::Number(2.0));
  assert_eq!(doc_all_0, "a_inner");
  assert_eq!(doc_all_1, "b_inner");
  assert_eq!(a_all_len, Value::Number(1.0));
  assert_eq!(a_all_0, "a_inner");
  assert_eq!(scope_same, Value::Bool(true));
  assert_eq!(a_matches, Value::Bool(true));
  assert_eq!(inner_matches, Value::Bool(true));
  assert_eq!(closest_ok, Value::Bool(true));
  assert_eq!(bad_qsa, "SyntaxError");
  assert_eq!(bad_matches, "SyntaxError");
  assert_eq!(bad_closest, "SyntaxError");

  Ok(())
}

#[test]
fn document_create_element_and_append_child_update_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r#"
  const el = document.createElement("div");
  el.setAttribute("id", "x");
  el.setAttribute("data-test", "1");
  globalThis.__data_test = el.getAttribute("data-test");
  globalThis.__missing_attr = (el.getAttribute("missing") === null);
  const ret = document.body.appendChild(el);
  globalThis.__append_same = (ret === el);
  globalThis.__found_same = (document.getElementById("x") === el);
  "#,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

  let (append_same, found_same, data_test, missing_attr) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__append_same"),
      get_data_prop(&mut scope, global, "__found_same"),
      get_data_prop(&mut scope, global, "__data_test"),
      get_data_prop(&mut scope, global, "__missing_attr"),
    )
  };

  assert_eq!(append_same, Value::Bool(true));
  assert_eq!(found_same, Value::Bool(true));
  assert_eq!(get_string(host.window().heap(), data_test), "1");
  assert_eq!(missing_attr, Value::Bool(true));

  let node = host
    .dom()
    .get_element_by_id("x")
    .expect("expected appended element to be reachable via get_element_by_id");
  let body = host
    .dom()
    .body()
    .expect("expected HTML document to have a body element");
  assert_eq!(
    host
      .dom()
      .parent(node)
      .expect("expected dom2::Document::parent to succeed"),
    Some(body)
  );
  assert_eq!(
    host
      .dom()
      .get_attribute(node, "data-test")
      .expect("expected get_attribute to succeed"),
    Some("1")
  );

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

#[derive(Debug, Clone)]
struct StubResponse {
  bytes: Vec<u8>,
  status: u16,
}

#[derive(Debug)]
struct InMemoryFetcher {
  routes: HashMap<String, StubResponse>,
  last_request_headers: Mutex<Vec<(String, String)>>,
  last_request_body: Mutex<Option<Vec<u8>>>,
  last_request_credentials_mode: Mutex<Option<FetchCredentialsMode>>,
}

impl InMemoryFetcher {
  fn new() -> Self {
    Self {
      routes: HashMap::new(),
      last_request_headers: Mutex::new(Vec::new()),
      last_request_body: Mutex::new(None),
      last_request_credentials_mode: Mutex::new(None),
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

  fn last_request_body(&self) -> Option<Vec<u8>> {
    self
      .last_request_body
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn last_request_credentials_mode(&self) -> Option<FetchCredentialsMode> {
    *self
      .last_request_credentials_mode
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }
}

impl Default for InMemoryFetcher {
  fn default() -> Self {
    Self::new()
  }
}

impl ResourceFetcher for InMemoryFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let fetch = FetchRequest::new(url, FetchDestination::Fetch);
    self.fetch_http_request(HttpRequest::new(fetch, "GET"))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch_http_request(HttpRequest::new(req, "GET"))
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    {
      let mut lock = self
        .last_request_headers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = req.headers.to_vec();
    }
    {
      let mut lock = self
        .last_request_body
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = req.body.map(|body| body.to_vec());
    }
    {
      let mut lock = self
        .last_request_credentials_mode
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = Some(req.fetch.credentials_mode);
    }

    let stub = self.lookup(req.fetch.url)?;
    let mut resource = FetchedResource::new(stub.bytes, None);
    resource.status = Some(stub.status);
    // Echo request headers back as response headers so JS can observe them via `Response.headers`
    // if desired.
    resource.response_headers = Some(req.headers.to_vec());
    Ok(resource)
  }
}

fn read_log_object(heap: &mut Heap, global: vm_js::GcObject) -> Result<Vec<String>> {
  let mut scope = heap.scope();
  scope
    .push_root(Value::Object(global))
    .map_err(|e| Error::Other(e.to_string()))?;

  let log_obj = match get_data_prop(&mut scope, global, "__log") {
    Value::Object(obj) => obj,
    _ => return Err(Error::Other("__log missing".to_string())),
  };
  scope
    .push_root(Value::Object(log_obj))
    .map_err(|e| Error::Other(e.to_string()))?;

  let len = match get_data_prop(&mut scope, global, "__log_len") {
    Value::Number(n) => n as u32,
    _ => return Err(Error::Other("__log_len missing".to_string())),
  };

  let mut out = Vec::with_capacity(len as usize);
  for idx in 0..len {
    let key_s = scope
      .alloc_string(&idx.to_string())
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::String(key_s))
      .map_err(|e| Error::Other(e.to_string()))?;
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(log_obj, &key)
      .map_err(|e| Error::Other(e.to_string()))?
      .unwrap_or(Value::Undefined);
    out.push(get_string(scope.heap(), value));
  }
  Ok(out)
}

struct FetchOnlyHost {
  window: WindowRealm,
  _fetch_bindings: fastrender::js::WindowFetchBindings,
}

impl WindowRealmHost for FetchOnlyHost {
  fn window_realm(&mut self) -> &mut WindowRealm {
    &mut self.window
  }
}

#[test]
fn window_fetch_text_orders_microtasks_before_networking() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/x", b"hello", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 globalThis.__log = {};
 globalThis.__log_len = 0;
  queueMicrotask(() => {
    globalThis.__log[globalThis.__log_len] = "micro";
    globalThis.__log_len = globalThis.__log_len + 1;
  });
   fetch("https://example.com/x")
    .then(r => r.text())
    .then(t => {
      globalThis.__log[globalThis.__log_len] = t;
      globalThis.__log_len = globalThis.__log_len + 1;
    });
  globalThis.__log[globalThis.__log_len] = "sync";
  globalThis.__log_len = globalThis.__log_len + 1;
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    read_log_object(heap, global)?
  };

  assert_eq!(log, vec!["sync", "micro", "hello"]);
  Ok(())
}

#[test]
fn window_fetch_forwards_request_headers() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 fetch("https://example.com/headers", { headers: { "x-test": "1" } })
   .then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "1"),
    "expected ResourceFetcher::fetch_http_request to receive x-test: 1"
  );
  Ok(())
}

#[test]
fn window_fetch_accepts_request_object_input() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 fetch(req).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "1"),
    "expected Request object input to forward x-test: 1"
  );
  Ok(())
}

#[test]
fn window_request_constructor_clones_request_input() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req1 = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 const req2 = new Request(req1);
 req2.headers.set("x-test", "2");
 fetch(req2).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "2"),
    "expected Request cloned from Request(input) to forward updated x-test: 2"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_body() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 fetch("https://example.com/submit", { method: "POST", body: "payload" }).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_body(),
    Some(b"payload".to_vec()),
    "expected fetch init body to reach the ResourceFetcher"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_body_from_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req = new Request("https://example.com/submit", { method: "POST", body: "payload" });
 fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_body(),
    Some(b"payload".to_vec()),
    "expected Request body to reach the ResourceFetcher when passed to fetch()"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_credentials_mode() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  fetch("https://example.com/creds", { credentials: "include" }).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_credentials_mode(),
    Some(FetchCredentialsMode::Include),
    "expected fetch init credentials to reach the ResourceFetcher"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_credentials_mode_from_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  const req = new Request("https://example.com/creds", { credentials: "omit" });
  fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_credentials_mode(),
    Some(FetchCredentialsMode::Omit),
    "expected Request constructor credentials to reach the ResourceFetcher when passed to fetch()"
  );
  Ok(())
}

#[test]
fn window_fetch_response_json_parses_body() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/json", br#"{"ok": true}"#, 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  fetch("https://example.com/json")
    .then(r => r.json())
    .then(v => globalThis.__json_ok = v.ok);
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let json_ok = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    get_data_prop(&mut scope, global, "__json_ok")
  };
  assert_eq!(json_ok, Value::Bool(true));
  Ok(())
}

#[test]
fn window_fetch_response_array_buffer_returns_bytes() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/bytes", b"hello", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__bytes = null;
  globalThis.__bytes_err = null;
  fetch("https://example.com/bytes")
    .then(function (r) { return r.arrayBuffer(); })
    .then(function (b) { globalThis.__bytes = b; })
    .catch(function (e) { globalThis.__bytes_err = e && e.name; });
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (bytes, err) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    (
      get_data_prop(&mut scope, global, "__bytes"),
      get_data_prop(&mut scope, global, "__bytes_err"),
    )
  };

  assert_eq!(err, Value::Null);
  assert_eq!(get_string(host.window_mut().heap(), bytes), "hello");
  Ok(())
}

#[test]
fn window_fetch_response_array_buffer_rejects_second_consumption() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/once-bytes", b"hello", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__ab_first = "";
  globalThis.__ab_second_err = "";
  fetch("https://example.com/once-bytes")
    .then(function (r) {
      return r.arrayBuffer().then(function (b) {
        globalThis.__ab_first = b;
        return r.arrayBuffer().then(
          function () { globalThis.__ab_second_err = "no error"; },
          function (e) { globalThis.__ab_second_err = e && e.name; }
        );
      });
    });
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (first, second_err) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let first = get_data_prop(&mut scope, global, "__ab_first");
    let second_err = get_data_prop(&mut scope, global, "__ab_second_err");
    (get_string(scope.heap(), first), get_string(scope.heap(), second_err))
  };

  assert_eq!(first, "hello");
  assert_eq!(second_err, "TypeError");
  Ok(())
}

#[test]
fn window_fetch_rejects_on_cors_failure() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://other.example/res", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://client.example/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__cors = "";
  fetch("https://other.example/res")
    .then(function () { globalThis.__cors = "resolved"; })
    .catch(function (e) { globalThis.__cors = e && e.name; });
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let cors = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__cors");
    get_string(scope.heap(), value)
  };
  assert_eq!(cors, "TypeError");
  Ok(())
}

#[test]
fn window_fetch_rejects_when_response_body_exceeds_limit() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://client.example/large", b"abcd", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<FetchOnlyHost>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  let document_url = "https://client.example/";
  let mut window =
    WindowRealm::new(WindowRealmConfig::new(document_url)).map_err(|e| Error::Other(e.to_string()))?;
  let limits = WebFetchLimits {
    max_response_body_bytes: 3,
    ..WebFetchLimits::default()
  };
  let fetch_bindings = {
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    fastrender::js::install_window_fetch_bindings_with_guard::<FetchOnlyHost>(
      vm,
      realm,
      heap,
      WindowFetchEnv::for_document(fetcher, Some(document_url.to_string())).with_limits(limits),
    )
    .map_err(|e| Error::Other(e.to_string()))?
  };
  let mut host = FetchOnlyHost {
    window,
    _fetch_bindings: fetch_bindings,
  };

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_realm();
      let res = realm.exec_script(
        r#"
  globalThis.__size_err_name = "";
  globalThis.__size_err_msg = "";
  fetch("https://client.example/large")
    .then(function () { globalThis.__size_err_name = "resolved"; })
    .catch(function (e) {
      globalThis.__size_err_name = e && e.name;
      globalThis.__size_err_msg = e && e.message;
    });
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (name, msg) = {
    let realm = host.window_realm();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let name = get_data_prop(&mut scope, global, "__size_err_name");
    let msg = get_data_prop(&mut scope, global, "__size_err_msg");
    (get_string(scope.heap(), name), get_string(scope.heap(), msg))
  };
  assert_eq!(name, "TypeError");
  assert!(
    msg.contains("response body exceeds configured limits"),
    "unexpected error message: {msg}"
  );
  Ok(())
}

#[test]
fn window_fetch_response_text_rejects_second_consumption() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/once-text", b"hello", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__text1 = "";
  globalThis.__text2_err = "";
  globalThis.__text_body_used = false;
  fetch("https://example.com/once-text")
    .then(function (r) {
      return r.text().then(function (t) {
        globalThis.__text1 = t;
        globalThis.__text_body_used = r.bodyUsed;
        return r.text().then(
          function () { globalThis.__text2_err = "no error"; },
          function (e) { globalThis.__text2_err = e && e.name; }
        );
      });
    });
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (text1, text2_err, body_used) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let text1 = get_data_prop(&mut scope, global, "__text1");
    let text2_err = get_data_prop(&mut scope, global, "__text2_err");
    let body_used = get_data_prop(&mut scope, global, "__text_body_used");
    (
      get_string(scope.heap(), text1),
      get_string(scope.heap(), text2_err),
      body_used,
    )
  };

  assert_eq!(text1, "hello");
  assert_eq!(text2_err, "TypeError");
  assert_eq!(body_used, Value::Bool(true));
  Ok(())
}

#[test]
fn window_fetch_accepts_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers2", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  let req = new Request("https://example.com/headers2", { headers: { "x-test": "2" } });
  fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "2"),
    "expected fetch(Request) to forward headers to ResourceFetcher::fetch_http_request"
  );
  Ok(())
}

#[test]
fn window_response_clone_duplicates_body() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__clone_text = "";
  let r = new Response("hello");
  let c = r.clone();
  r.text().then(function (t1) {
    return c.text().then(function (t2) {
      globalThis.__clone_text = t1 + "," + t2;
    });
  });
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let clone_text = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__clone_text");
    get_string(scope.heap(), value)
  };
  assert_eq!(clone_text, "hello,hello");
  Ok(())
}

#[test]
fn window_response_clone_throws_when_body_used() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  globalThis.__clone_error = "";
  let r = new Response("hello");
  r.text().then(() => {
    try {
      r.clone();
      globalThis.__clone_error = "no error";
    } catch (e) {
      globalThis.__clone_error = e.name;
    }
  });
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let clone_error = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__clone_error");
    get_string(scope.heap(), value)
  };
  assert_eq!(clone_error, "TypeError");
  Ok(())
}
