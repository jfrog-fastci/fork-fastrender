use fastrender::dom2::Document;
use fastrender::js::{install_dom_bindings, CurrentScriptState};
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions};

fn get_data_property_value(heap: &Heap, obj: vm_js::GcObject, key: &PropertyKey) -> Option<Value> {
  heap
    .get_property(obj, key)
    .ok()
    .flatten()
    .and_then(|desc| match desc.kind {
      PropertyKind::Data { value, .. } => Some(value),
      PropertyKind::Accessor { .. } => None,
    })
}

fn get_accessor_getter(heap: &Heap, obj: vm_js::GcObject, key: &PropertyKey) -> Option<Value> {
  heap
    .get_property(obj, key)
    .ok()
    .flatten()
    .and_then(|desc| match desc.kind {
      PropertyKind::Accessor { get, .. } => Some(get),
      PropertyKind::Data { .. } => None,
    })
}

fn as_utf8_lossy(heap: &Heap, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string, got {v:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn installs_dom_bindings_and_exposes_constructors_and_basic_methods() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));

  install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

  let mut scope = heap.scope();
  let global = realm.global_object();

  // Global constructors exist (non-constructable but spec-shaped).
  let key_document_ctor = PropertyKey::from_string(scope.alloc_string("Document")?);
  let ctor_document = scope
    .heap()
    .object_get_own_data_property_value(global, &key_document_ctor)?
    .expect("globalThis.Document should exist");
  assert!(
    scope.heap().is_callable(ctor_document).unwrap_or(false),
    "Document should be callable"
  );
  let Value::Object(ctor_document_obj) = ctor_document else {
    panic!("Document should be an object");
  };
  let key_prototype = PropertyKey::from_string(scope.alloc_string("prototype")?);
  let proto_document = scope
    .heap()
    .object_get_own_data_property_value(ctor_document_obj, &key_prototype)?
    .expect("Document.prototype should exist");
  let Value::Object(proto_document_obj) = proto_document else {
    panic!("Document.prototype should be an object");
  };

  let key_element_ctor = PropertyKey::from_string(scope.alloc_string("Element")?);
  let ctor_element = scope
    .heap()
    .object_get_own_data_property_value(global, &key_element_ctor)?
    .expect("globalThis.Element should exist");
  assert!(
    scope.heap().is_callable(ctor_element).unwrap_or(false),
    "Element should be callable"
  );
  let Value::Object(ctor_element_obj) = ctor_element else {
    panic!("Element should be an object");
  };
  let proto_element = scope
    .heap()
    .object_get_own_data_property_value(ctor_element_obj, &key_prototype)?
    .expect("Element.prototype should exist");
  let Value::Object(proto_element_obj) = proto_element else {
    panic!("Element.prototype should be an object");
  };

  // globalThis.document exists and has the expected prototype.
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  let document_val = scope
    .heap()
    .object_get_own_data_property_value(global, &key_document)?
    .expect("globalThis.document should be defined");
  let document_obj = match document_val {
    Value::Object(o) => o,
    _ => panic!("document should be an object"),
  };
  assert_eq!(scope.object_get_prototype(document_obj)?, Some(proto_document_obj));

  // document.createElement("div") returns an Element wrapper with the correct prototype.
  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let tag_div = Value::String(scope.alloc_string("div")?);
  let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let Value::Object(el_obj) = el_val else {
    panic!("createElement should return an object");
  };
  assert_eq!(scope.object_get_prototype(el_obj)?, Some(proto_element_obj));

  let key_tag_name = PropertyKey::from_string(scope.alloc_string("tagName")?);
  let tag_name_get =
    get_accessor_getter(scope.heap(), el_obj, &key_tag_name).expect("tagName getter exists");
  let tag_name = vm.call_without_host(&mut scope, tag_name_get, el_val, &[])?;
  assert_eq!(as_utf8_lossy(scope.heap(), tag_name), "DIV");

  // document.querySelector("body") returns null on an empty document; invalid selectors throw.
  let key_query_selector = PropertyKey::from_string(scope.alloc_string("querySelector")?);
  let query_selector =
    get_data_property_value(scope.heap(), document_obj, &key_query_selector).expect("querySelector exists");

  let arg_body = Value::String(scope.alloc_string("body")?);
  let out = vm.call_without_host(&mut scope, query_selector, document_val, &[arg_body])?;
  assert!(matches!(out, Value::Null));

  let arg_bad = Value::String(scope.alloc_string("[")?);
  let thrown = match vm.call_without_host(&mut scope, query_selector, document_val, &[arg_bad]) {
    Ok(_) => panic!("expected querySelector to throw"),
    Err(err) => err.thrown_value().expect("expected thrown value"),
  };
  let Value::Object(thrown_obj) = thrown else {
    panic!("thrown value should be an object");
  };
  let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
  let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name)
    .expect("thrown object should have .name");
  assert_eq!(as_utf8_lossy(scope.heap(), name_val), "SyntaxError");

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
