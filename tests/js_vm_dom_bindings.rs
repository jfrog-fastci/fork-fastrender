use fastrender::dom2::Document;
use fastrender::js::{install_dom_bindings, CurrentScriptState};
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

fn get_data_property_value(
  heap: &Heap,
  obj: vm_js::GcObject,
  key: &PropertyKey,
) -> Option<Value> {
  heap
    .get_property(obj, key)
    .ok()
    .flatten()
    .and_then(|desc| match desc.kind {
      PropertyKind::Data { value, .. } => Some(value),
      PropertyKind::Accessor { .. } => None,
    })
}

fn get_accessor_getter(
  heap: &Heap,
  obj: vm_js::GcObject,
  key: &PropertyKey,
) -> Option<Value> {
  heap
    .get_property(obj, key)
    .ok()
    .flatten()
    .and_then(|desc| match desc.kind {
      PropertyKind::Accessor { get, .. } => Some(get),
      PropertyKind::Data { .. } => None,
    })
}

#[test]
fn dom_bindings_smoke() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));

  install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script.clone())?;

  let mut scope = heap.scope();

  // Fetch globalThis.document.
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  let document_val = scope
    .heap()
    .object_get_own_data_property_value(realm.global_object(), &key_document)?
    .expect("globalThis.document should be defined");
  let document_obj = match document_val {
    Value::Object(o) => o,
    _ => panic!("document should be an object"),
  };

  // document.createElement("div") -> Element wrapper.
  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let tag_div = Value::String(scope.alloc_string("div")?);
  let el_val = vm.call(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  // el.setAttribute("id", "foo")
  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_foo = Value::String(scope.alloc_string("foo")?);
  let r = vm.call(&mut scope, set_attribute, el_val, &[arg_id, arg_foo])?;
  assert!(matches!(r, Value::Undefined));

  // document.appendChild(el)
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  let appended = vm.call(&mut scope, append_child, document_val, &[el_val])?;
  assert_eq!(appended, el_val, "appendChild should return the child");

  // Validate DOM mutation.
  let root = dom.borrow().root();
  let found = dom.borrow().get_element_by_id("foo").expect("id should be set");
  assert_eq!(dom.borrow().parent(found).unwrap(), Some(root));
  assert!(
    dom.borrow()
      .children(root)
      .unwrap()
      .iter()
      .any(|&c| c == found)
  );

  // document.getElementById("foo") returns wrapper identity.
  let key_get_element_by_id = PropertyKey::from_string(scope.alloc_string("getElementById")?);
  let get_element_by_id =
    get_data_property_value(scope.heap(), document_obj, &key_get_element_by_id)
      .expect("getElementById exists");
  let arg_foo2 = Value::String(scope.alloc_string("foo")?);
  let got = vm.call(&mut scope, get_element_by_id, document_val, &[arg_foo2])?;
  assert_eq!(got, el_val, "wrapper identity should be preserved");

  let arg_nope = Value::String(scope.alloc_string("nope")?);
  let missing = vm.call(&mut scope, get_element_by_id, document_val, &[arg_nope])?;
  assert!(matches!(missing, Value::Null));

  // document.querySelector invalid selector throws a DOMException-like object with name == "SyntaxError".
  let key_query_selector = PropertyKey::from_string(scope.alloc_string("querySelector")?);
  let query_selector = get_data_property_value(scope.heap(), document_obj, &key_query_selector)
    .expect("querySelector exists");
  let arg_bad = Value::String(scope.alloc_string("???")?);
  let thrown = match vm.call(&mut scope, query_selector, document_val, &[arg_bad]) {
    Ok(_) => panic!("expected querySelector to throw"),
    Err(VmError::Throw(v)) => v,
    Err(e) => return Err(e),
  };
  let thrown_obj = match thrown {
    Value::Object(o) => o,
    _ => panic!("thrown value should be an object"),
  };
  let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
  let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name)
    .expect("thrown object should have .name");
  let name_str = match name_val {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => panic!(".name should be a string"),
  };
  assert_eq!(name_str, "SyntaxError");

  // document.currentScript getter returns null by default, then a wrapper when set.
  let key_current_script = PropertyKey::from_string(scope.alloc_string("currentScript")?);
  let current_script_get =
    get_accessor_getter(scope.heap(), document_obj, &key_current_script)
      .expect("currentScript getter should exist");
  let cs0 = vm.call(&mut scope, current_script_get, document_val, &[])?;
  assert!(matches!(cs0, Value::Null));

  // Create a <script> node and set CurrentScriptState.
  let script_id = dom.borrow_mut().create_element("script", "");
  // Document nodes only allow a single element child; append under the existing <div id="foo">.
  dom.borrow_mut().append_child(found, script_id).unwrap();
  current_script.borrow_mut().current_script = Some(script_id);

  let cs1 = vm.call(&mut scope, current_script_get, document_val, &[])?;
  assert!(matches!(cs1, Value::Object(_)));

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
