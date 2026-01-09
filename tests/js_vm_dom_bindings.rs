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

fn get_accessor_setter(
  heap: &Heap,
  obj: vm_js::GcObject,
  key: &PropertyKey,
) -> Option<Value> {
  heap
    .get_property(obj, key)
    .ok()
    .flatten()
    .and_then(|desc| match desc.kind {
      PropertyKind::Accessor { set, .. } => Some(set),
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

#[test]
fn node_text_content_getter_and_setter() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

  let mut scope = heap.scope();
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  let document_val = scope
    .heap()
    .object_get_own_data_property_value(realm.global_object(), &key_document)?
    .expect("globalThis.document should exist");
  let document_obj = match document_val {
    Value::Object(o) => o,
    _ => panic!("document should be an object"),
  };

  // Create an element wrapper and attach it to the document so `getElementById` can find it.
  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");
  let tag_div = Value::String(scope.alloc_string("div")?);
  let el_val = vm.call(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_root = Value::String(scope.alloc_string("root")?);
  vm.call(&mut scope, set_attribute, el_val, &[arg_id, arg_root])?;

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  vm.call(&mut scope, append_child, document_val, &[el_val])?;

  let root_id = dom
    .borrow()
    .get_element_by_id("root")
    .expect("missing #root node id");

  // Build a nested subtree:
  // <div id=root>
  //   "Hello "
  //   <span>"World"</span>
  //   <template>"INERT"</template>  (should be skipped)
  //   "!"
  // </div>
  {
    let mut d = dom.borrow_mut();
    let text_hello = d.create_text("Hello ");
    d.append_child(root_id, text_hello).unwrap();

    let span = d.create_element("span", "");
    d.append_child(root_id, span).unwrap();
    let text_world = d.create_text("World");
    d.append_child(span, text_world).unwrap();

    let template = d.create_element("template", "");
    d.append_child(root_id, template).unwrap();
    let inert = d.create_text("INERT");
    d.append_child(template, inert).unwrap();

    let text_bang = d.create_text("!");
    d.append_child(root_id, text_bang).unwrap();
  }

  let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
  let text_content_get =
    get_accessor_getter(scope.heap(), el_obj, &key_text_content).expect("textContent getter exists");

  let got = vm.call(&mut scope, text_content_get, el_val, &[])?;
  let got_s = match got {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => panic!("textContent getter should return string"),
  };
  assert_eq!(got_s, "Hello World!");

  let text_content_set =
    get_accessor_setter(scope.heap(), el_obj, &key_text_content).expect("textContent setter exists");
  let arg_replaced = Value::String(scope.alloc_string("replaced")?);
  let r = vm.call(&mut scope, text_content_set, el_val, &[arg_replaced])?;
  assert!(matches!(r, Value::Undefined));

  let children = dom.borrow().children(root_id).unwrap().to_vec();
  assert_eq!(children.len(), 1);
  let only_child = children[0];
  match &dom.borrow().node(only_child).kind {
    fastrender::dom2::NodeKind::Text { content } => assert_eq!(content, "replaced"),
    other => panic!("expected a single Text node child, got {other:?}"),
  }

  // Setting to empty clears children.
  let arg_empty = Value::String(scope.alloc_string("")?);
  vm.call(&mut scope, text_content_set, el_val, &[arg_empty])?;
  assert!(dom.borrow().children(root_id).unwrap().is_empty());

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn element_class_list_dom_token_list() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

  let mut scope = heap.scope();
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  let document_val = scope
    .heap()
    .object_get_own_data_property_value(realm.global_object(), &key_document)?
    .expect("globalThis.document should exist");
  let document_obj = match document_val {
    Value::Object(o) => o,
    _ => panic!("document should be an object"),
  };

  // Create an element, attach it, and set class="a".
  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");
  let tag_div = Value::String(scope.alloc_string("div")?);
  let el_val = vm.call(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_e1 = Value::String(scope.alloc_string("e1")?);
  vm.call(&mut scope, set_attribute, el_val, &[arg_id, arg_e1])?;

  let arg_class = Value::String(scope.alloc_string("class")?);
  let arg_a = Value::String(scope.alloc_string("a")?);
  vm.call(&mut scope, set_attribute, el_val, &[arg_class, arg_a])?;

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  vm.call(&mut scope, append_child, document_val, &[el_val])?;

  let e1_id = dom.borrow().get_element_by_id("e1").expect("missing #e1");
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("a")
  );

  // classList getter returns a DOMTokenList wrapper with identity.
  let key_class_list = PropertyKey::from_string(scope.alloc_string("classList")?);
  let class_list_get =
    get_accessor_getter(scope.heap(), el_obj, &key_class_list).expect("classList getter exists");
  let list1 = vm.call(&mut scope, class_list_get, el_val, &[])?;
  let list2 = vm.call(&mut scope, class_list_get, el_val, &[])?;
  assert_eq!(list1, list2, "classList should preserve wrapper identity");

  let list_obj = match list1 {
    Value::Object(o) => o,
    _ => panic!("classList should return an object"),
  };

  let key_contains = PropertyKey::from_string(scope.alloc_string("contains")?);
  let contains =
    get_data_property_value(scope.heap(), list_obj, &key_contains).expect("contains exists");

  let arg_a2 = Value::String(scope.alloc_string("a")?);
  let arg_b = Value::String(scope.alloc_string("b")?);
  let has_a = vm.call(&mut scope, contains, list1, &[arg_a2])?;
  let has_b = vm.call(&mut scope, contains, list1, &[arg_b])?;
  assert_eq!(has_a, Value::Bool(true));
  assert_eq!(has_b, Value::Bool(false));

  let key_add = PropertyKey::from_string(scope.alloc_string("add")?);
  let add = get_data_property_value(scope.heap(), list_obj, &key_add).expect("add exists");
  let arg_b2 = Value::String(scope.alloc_string("b")?);
  assert!(matches!(
    vm.call(&mut scope, add, list1, &[arg_b2])?,
    Value::Undefined
  ));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("a b")
  );

  let key_remove = PropertyKey::from_string(scope.alloc_string("remove")?);
  let remove =
    get_data_property_value(scope.heap(), list_obj, &key_remove).expect("remove exists");
  let arg_a3 = Value::String(scope.alloc_string("a")?);
  vm.call(&mut scope, remove, list1, &[arg_a3])?;
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b")
  );

  let key_toggle = PropertyKey::from_string(scope.alloc_string("toggle")?);
  let toggle =
    get_data_property_value(scope.heap(), list_obj, &key_toggle).expect("toggle exists");

  let arg_c = Value::String(scope.alloc_string("c")?);
  let added = vm.call(&mut scope, toggle, list1, &[arg_c])?;
  assert_eq!(added, Value::Bool(true));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b c")
  );

  let arg_c2 = Value::String(scope.alloc_string("c")?);
  let removed = vm.call(&mut scope, toggle, list1, &[arg_c2])?;
  assert_eq!(removed, Value::Bool(false));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b")
  );

  // Invalid tokens (ASCII whitespace) throw SyntaxError.
  let bad = Value::String(scope.alloc_string("bad token")?);
  let thrown = match vm.call(&mut scope, add, list1, &[bad]) {
    Ok(_) => panic!("expected classList.add to throw for invalid token"),
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

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
