use fastrender::dom::parse_html;
use fastrender::dom2::Document as Dom2Document;
use fastrender::js::dom_bindings_context::DomBindingsContext;
use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmOptions,
};

fn make_runtime_with_dom(html: &str) -> Result<JsRuntime, vm_js::VmError> {
  let renderer_dom = parse_html(html).expect("parse_html");
  let dom = Dom2Document::from_renderer_dom(&renderer_dom);

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut cx = DomBindingsContext::new(dom);
  {
    let global_object = rt.realm().global_object();
    let (vm, heap) = (&mut rt.vm, &mut rt.heap);

    let mut scope = heap.scope();
    cx.init(vm, &mut scope)?;
    let document_wrapper = cx.get_or_create_node_wrapper(&mut scope, cx.dom().root())?;
    scope.push_root(Value::Object(document_wrapper))?;

    // Expose `document` as a global binding by defining it as a property on the global object. The
    // AST interpreter's environment resolves unknown identifiers via global object lookup.
    let key = PropertyKey::from_string(scope.alloc_string("document")?);
    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(document_wrapper),
        writable: true,
      },
    };
    scope.define_property(global_object, key, desc)?;
  }

  rt.vm.set_user_data(cx);
  Ok(rt)
}

#[test]
fn dom_node_wrapper_identity_is_stable() -> Result<(), vm_js::VmError> {
  let mut rt = make_runtime_with_dom(r#"<!doctype html><div></div>"#)?;
  let value = rt.exec_script(r#"document.querySelector("div") === document.querySelector("div")"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn dom_wrapper_node_id_slot_drives_methods() -> Result<(), vm_js::VmError> {
  let mut rt = make_runtime_with_dom(r#"<!doctype html><div id="target"></div>"#)?;
  let value = rt.exec_script(r#"document.querySelector('#target').getAttribute('id')"#)?;

  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap.get_string(s)?.to_utf8_lossy();
  assert_eq!(actual, "target");
  Ok(())
}

#[test]
fn dom_wrapper_cache_allows_gc_recycling() -> Result<(), vm_js::VmError> {
  let mut rt = make_runtime_with_dom(r#"<!doctype html><div></div>"#)?;

  let value = rt.exec_script(r#"var a = document.querySelector("div"); a"#)?;
  let Value::Object(obj_a) = value else {
    panic!("expected object, got {value:?}");
  };

  rt.exec_script("a = null")?;
  rt.heap.collect_garbage();
  assert!(
    !rt.heap.is_valid_object(obj_a),
    "expected wrapper to be collectable after dropping JS refs"
  );

  let value = rt.exec_script(r#"document.querySelector("div")"#)?;
  let Value::Object(obj_b) = value else {
    panic!("expected object, got {value:?}");
  };
  assert!(rt.heap.is_valid_object(obj_b));
  assert_ne!(
    obj_a, obj_b,
    "expected a new wrapper to be created after GC"
  );

  Ok(())
}
