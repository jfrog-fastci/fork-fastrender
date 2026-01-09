use fastrender::dom2::Document;
use fastrender::js::{install_dom_bindings, install_dom_bindings_with_limits, CurrentScriptState};
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

  // document.hasChildNodes() should be false on a new empty document.
  let key_has_child_nodes = PropertyKey::from_string(scope.alloc_string("hasChildNodes")?);
  let has_child_nodes = get_data_property_value(scope.heap(), document_obj, &key_has_child_nodes)
    .expect("document.hasChildNodes should exist");
  let has_children =
    vm.call_without_host(&mut scope, has_child_nodes, document_val, &[])?;
  assert_eq!(has_children, Value::Bool(false));

  // document.createElement("div") -> Element wrapper.
  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let tag_div = Value::String(scope.alloc_string("div")?);
  let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  // Element wrappers should also expose Node.hasChildNodes.
  let el_has_children = vm.call_without_host(&mut scope, has_child_nodes, el_val, &[])?;
  assert_eq!(el_has_children, Value::Bool(false));

  // el.setAttribute("id", "foo")
  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_foo = Value::String(scope.alloc_string("foo")?);
  let r = vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_foo])?;
  assert!(matches!(r, Value::Undefined));

  // document.appendChild(el)
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  let appended = vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;
  assert_eq!(appended, el_val, "appendChild should return the child");

  // document.hasChildNodes() should now return true.
  let doc_has_children =
    vm.call_without_host(&mut scope, has_child_nodes, document_val, &[])?;
  assert_eq!(doc_has_children, Value::Bool(true));

  // Basic Node navigation getters.
  let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
  let parent_node_get = get_accessor_getter(scope.heap(), el_obj, &key_parent_node)
    .expect("parentNode getter should exist");
  let key_parent_element = PropertyKey::from_string(scope.alloc_string("parentElement")?);
  let parent_element_get = get_accessor_getter(scope.heap(), el_obj, &key_parent_element)
    .expect("parentElement getter should exist");
  let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
  let first_child_get = get_accessor_getter(scope.heap(), el_obj, &key_first_child)
    .expect("firstChild getter should exist");
  let key_last_child = PropertyKey::from_string(scope.alloc_string("lastChild")?);
  let last_child_get = get_accessor_getter(scope.heap(), el_obj, &key_last_child)
    .expect("lastChild getter should exist");
  let key_previous_sibling = PropertyKey::from_string(scope.alloc_string("previousSibling")?);
  let previous_sibling_get = get_accessor_getter(scope.heap(), el_obj, &key_previous_sibling)
    .expect("previousSibling getter should exist");
  let key_next_sibling = PropertyKey::from_string(scope.alloc_string("nextSibling")?);
  let next_sibling_get = get_accessor_getter(scope.heap(), el_obj, &key_next_sibling)
    .expect("nextSibling getter should exist");
  let key_node_type = PropertyKey::from_string(scope.alloc_string("nodeType")?);
  let node_type_get = get_accessor_getter(scope.heap(), el_obj, &key_node_type)
    .expect("nodeType getter should exist");

  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, document_val, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, el_val, &[])?,
    document_val
  );
  assert_eq!(
    vm.call_without_host(&mut scope, parent_element_get, el_val, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, first_child_get, document_val, &[])?,
    el_val
  );
  assert_eq!(
    vm.call_without_host(&mut scope, last_child_get, document_val, &[])?,
    el_val
  );
  assert_eq!(
    vm.call_without_host(&mut scope, previous_sibling_get, el_val, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, next_sibling_get, el_val, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, node_type_get, document_val, &[])?,
    Value::Number(9.0)
  );
  assert_eq!(
    vm.call_without_host(&mut scope, node_type_get, el_val, &[])?,
    Value::Number(1.0)
  );

  // Add two child nodes under `<div id="foo">` so we can validate sibling relationships.
  let tag_span = Value::String(scope.alloc_string("span")?);
  let child1 = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
  let Value::Object(child1_obj) = child1 else {
    panic!("createElement should return an object");
  };
  let child2 = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
  let Value::Object(child2_obj) = child2 else {
    panic!("createElement should return an object");
  };
  vm.call_without_host(&mut scope, append_child, el_val, &[child1])?;
  vm.call_without_host(&mut scope, append_child, el_val, &[child2])?;

  assert_eq!(
    vm.call_without_host(&mut scope, first_child_get, el_val, &[])?,
    child1
  );
  assert_eq!(
    vm.call_without_host(&mut scope, last_child_get, el_val, &[])?,
    child2
  );
  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, child1, &[])?,
    el_val
  );
  assert_eq!(
    vm.call_without_host(&mut scope, parent_element_get, child1, &[])?,
    el_val
  );
  assert_eq!(
    vm.call_without_host(&mut scope, node_type_get, child1, &[])?,
    Value::Number(1.0)
  );

  let next_sibling_get = get_accessor_getter(scope.heap(), child1_obj, &key_next_sibling)
    .expect("nextSibling getter should exist");
  let previous_sibling_get = get_accessor_getter(scope.heap(), child2_obj, &key_previous_sibling)
    .expect("previousSibling getter should exist");
  assert_eq!(
    vm.call_without_host(&mut scope, next_sibling_get, child1, &[])?,
    child2
  );
  assert_eq!(
    vm.call_without_host(&mut scope, previous_sibling_get, child2, &[])?,
    child1
  );

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
  let got = vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_foo2])?;
  assert_eq!(got, el_val, "wrapper identity should be preserved");

  let arg_nope = Value::String(scope.alloc_string("nope")?);
  let missing = vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_nope])?;
  assert!(matches!(missing, Value::Null));

  // document.querySelector invalid selector throws a DOMException-like object with name == "SyntaxError".
  let key_query_selector = PropertyKey::from_string(scope.alloc_string("querySelector")?);
  let query_selector = get_data_property_value(scope.heap(), document_obj, &key_query_selector)
    .expect("querySelector exists");
  let arg_bad = Value::String(scope.alloc_string("???")?);
  let thrown = match vm.call_without_host(&mut scope, query_selector, document_val, &[arg_bad]) {
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
  let cs0 = vm.call_without_host(&mut scope, current_script_get, document_val, &[])?;
  assert!(matches!(cs0, Value::Null));

  // Create a <script> node and set CurrentScriptState.
  let script_id = dom.borrow_mut().create_element("script", "");
  // Document nodes only allow a single element child; append under the existing <div id="foo">.
  dom.borrow_mut().append_child(found, script_id).unwrap();
  current_script.borrow_mut().current_script = Some(script_id);

  // The <div id="foo"> wrapper should observe the new child.
  let el_has_children =
    vm.call_without_host(&mut scope, has_child_nodes, el_val, &[])?;
  assert_eq!(el_has_children, Value::Bool(true));

  let cs1 = vm.call_without_host(&mut scope, current_script_get, document_val, &[])?;
  assert!(matches!(cs1, Value::Object(_)));

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn dom_bindings_rejects_strings_over_max_string_bytes() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));

  // Use a tiny conversion limit so multi-byte strings can exceed it even though the UTF-16 input
  // is short.
  install_dom_bindings_with_limits(
    &mut vm,
    &mut heap,
    &realm,
    dom.clone(),
    current_script.clone(),
    5,
  )?;

  let mut scope = heap.scope();
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  let document_val = scope
    .heap()
    .object_get_own_data_property_value(realm.global_object(), &key_document)?
    .expect("globalThis.document should be defined");

  let document_obj = match document_val {
    Value::Object(o) => o,
    _ => panic!("document should be an object"),
  };

  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element =
    get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

  // "ééé" is 3 UTF-16 code units but 6 UTF-8 bytes.
  let tag = Value::String(scope.alloc_string("ééé")?);
  let err = vm
    .call_without_host(&mut scope, create_element, document_val, &[tag])
    .expect_err("expected createElement to throw");

  let VmError::Throw(thrown) = err else {
    panic!("expected a thrown TypeError, got {err:?}");
  };

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let message = match thrown {
    Value::Object(obj) => get_data_property_value(scope.heap(), obj, &message_key)
      .expect("thrown error should have message"),
    other => panic!("expected error object, got {other:?}"),
  };
  let Value::String(message_str) = message else {
    panic!("expected message string, got {message:?}");
  };
  let msg = scope.heap().get_string(message_str)?.to_utf8_lossy();

  // Ensure teardown runs even if assertions fail, otherwise `Realm` will panic in Drop while the
  // test is already unwinding.
  drop(scope);
  realm.teardown(&mut heap);

  assert!(msg.contains("max_string_bytes"), "unexpected error message: {msg}");
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
  let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_root = Value::String(scope.alloc_string("root")?);
  vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_root])?;

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;

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

  // DOM: `Document.textContent` is `null`.
  let doc_text = vm.call_without_host(&mut scope, text_content_get, document_val, &[])?;
  assert!(matches!(doc_text, Value::Null));

  let got = vm.call_without_host(&mut scope, text_content_get, el_val, &[])?;
  let got_s = match got {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    _ => panic!("textContent getter should return string"),
  };
  assert_eq!(got_s, "Hello World!");

  let text_content_set =
    get_accessor_setter(scope.heap(), el_obj, &key_text_content).expect("textContent setter exists");

  // DOM: setting `Document.textContent` is a no-op.
  let arg_ignored = Value::String(scope.alloc_string("ignored")?);
  vm.call_without_host(&mut scope, text_content_set, document_val, &[arg_ignored])?;
  assert!(dom.borrow().get_element_by_id("root").is_some());

  let arg_replaced = Value::String(scope.alloc_string("replaced")?);
  let r = vm.call_without_host(&mut scope, text_content_set, el_val, &[arg_replaced])?;
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
  vm.call_without_host(&mut scope, text_content_set, el_val, &[arg_empty])?;
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
  let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let el_obj = match el_val {
    Value::Object(o) => o,
    _ => panic!("createElement should return an object"),
  };

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let set_attribute =
    get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_e1 = Value::String(scope.alloc_string("e1")?);
  vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_e1])?;

  let arg_class = Value::String(scope.alloc_string("class")?);
  let arg_a = Value::String(scope.alloc_string("a")?);
  vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_class, arg_a])?;

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;

  let e1_id = dom.borrow().get_element_by_id("e1").expect("missing #e1");
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("a")
  );

  // classList getter returns a DOMTokenList wrapper with identity.
  let key_class_list = PropertyKey::from_string(scope.alloc_string("classList")?);
  let class_list_get =
    get_accessor_getter(scope.heap(), el_obj, &key_class_list).expect("classList getter exists");
  let list1 = vm.call_without_host(&mut scope, class_list_get, el_val, &[])?;
  let list2 = vm.call_without_host(&mut scope, class_list_get, el_val, &[])?;
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
  let has_a = vm.call_without_host(&mut scope, contains, list1, &[arg_a2])?;
  let has_b = vm.call_without_host(&mut scope, contains, list1, &[arg_b])?;
  assert_eq!(has_a, Value::Bool(true));
  assert_eq!(has_b, Value::Bool(false));

  let key_add = PropertyKey::from_string(scope.alloc_string("add")?);
  let add = get_data_property_value(scope.heap(), list_obj, &key_add).expect("add exists");
  let arg_b2 = Value::String(scope.alloc_string("b")?);
  assert!(matches!(
    vm.call_without_host(&mut scope, add, list1, &[arg_b2])?,
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
  vm.call_without_host(&mut scope, remove, list1, &[arg_a3])?;
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b")
  );

  let key_toggle = PropertyKey::from_string(scope.alloc_string("toggle")?);
  let toggle =
    get_data_property_value(scope.heap(), list_obj, &key_toggle).expect("toggle exists");

  let arg_c = Value::String(scope.alloc_string("c")?);
  let added = vm.call_without_host(&mut scope, toggle, list1, &[arg_c])?;
  assert_eq!(added, Value::Bool(true));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b c")
  );

  let arg_c2 = Value::String(scope.alloc_string("c")?);
  let removed = vm.call_without_host(&mut scope, toggle, list1, &[arg_c2])?;
  assert_eq!(removed, Value::Bool(false));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("b")
  );

  // Invalid tokens (ASCII whitespace) throw SyntaxError.
  let bad = Value::String(scope.alloc_string("bad token")?);
  let thrown = match vm.call_without_host(&mut scope, add, list1, &[bad]) {
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

#[test]
fn get_elements_by_tag_name_is_live_and_skips_inert_template_contents() -> Result<(), VmError> {
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

  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");

  // Build a minimal tree:
  // document
  //   <body>
  //     <div id=a></div>
  //     <div id=b></div>
  //     <template>
  //       <div id=inside></div>  (inert, should be skipped)
  //     </template>
  let tag_body = Value::String(scope.alloc_string("body")?);
  let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;

  vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

  let tag_div = Value::String(scope.alloc_string("div")?);
  let d1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let d1_obj = match d1_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr = get_data_property_value(scope.heap(), d1_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id = Value::String(scope.alloc_string("id")?);
  let arg_a = Value::String(scope.alloc_string("a")?);
  vm.call_without_host(&mut scope, set_attr, d1_val, &[arg_id, arg_a])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d1_val])?;

  let tag_div2 = Value::String(scope.alloc_string("div")?);
  let d2_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div2])?;
  let d2_obj = match d2_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr2 = get_data_property_value(scope.heap(), d2_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_id2 = Value::String(scope.alloc_string("id")?);
  let arg_b = Value::String(scope.alloc_string("b")?);
  vm.call_without_host(&mut scope, set_attr2, d2_val, &[arg_id2, arg_b])?;

  // Call getElementsByTagName before inserting d2 to exercise liveness.
  let key_get_elements_by_tag_name = PropertyKey::from_string(scope.alloc_string("getElementsByTagName")?);
  let get_elements_by_tag_name =
    get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name)
      .expect("getElementsByTagName should exist");

  let arg_div = Value::String(scope.alloc_string("div")?);
  let coll_val = vm.call_without_host(&mut scope, get_elements_by_tag_name, document_val, &[arg_div])?;
  let coll_obj = match coll_val {
    Value::Object(o) => o,
    _ => panic!("expected an object collection"),
  };

  let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
  let len1 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len1, Value::Number(1.0));

  let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
  let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
  assert_eq!(v0, d1_val);

  let key_item = PropertyKey::from_string(scope.alloc_string("item")?);
  let item = get_data_property_value(scope.heap(), coll_obj, &key_item).expect("item exists");
  let item0 = vm.call_without_host(&mut scope, item, coll_val, &[Value::Number(0.0)])?;
  assert_eq!(item0, d1_val);
  let item_neg = vm.call_without_host(&mut scope, item, coll_val, &[Value::Number(-1.0)])?;
  assert!(matches!(item_neg, Value::Null));

  // Append d2 and ensure the same collection object updates.
  vm.call_without_host(&mut scope, append_child, body_val, &[d2_val])?;
  let len2 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len2, Value::Number(2.0));
  let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
  let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
  assert_eq!(v1, d2_val);

  // Append a <template><div></div></template> and ensure inert contents are skipped.
  let tag_template = Value::String(scope.alloc_string("template")?);
  let tmpl_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_template])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[tmpl_val])?;

  let inside_val = {
    let tag_div3 = Value::String(scope.alloc_string("div")?);
    let inside = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div3])?;
    let inside_obj = match inside {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr3 = get_data_property_value(scope.heap(), inside_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id3 = Value::String(scope.alloc_string("id")?);
    let arg_inside = Value::String(scope.alloc_string("inside")?);
    vm.call_without_host(&mut scope, set_attr3, inside, &[arg_id3, arg_inside])?;
    inside
  };
  vm.call_without_host(&mut scope, append_child, tmpl_val, &[inside_val])?;

  let len3 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len3, Value::Number(2.0));

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn get_elements_by_class_name_tokenizes_and_is_live() -> Result<(), VmError> {
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

  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");

  let tag_body = Value::String(scope.alloc_string("body")?);
  let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
  vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

  let tag_div = Value::String(scope.alloc_string("div")?);
  let d1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let d1_obj = match d1_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr = get_data_property_value(scope.heap(), d1_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_class = Value::String(scope.alloc_string("class")?);
  let arg_foo_bar = Value::String(scope.alloc_string("foo bar")?);
  vm.call_without_host(&mut scope, set_attr, d1_val, &[arg_class, arg_foo_bar])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d1_val])?;

  let tag_div2 = Value::String(scope.alloc_string("div")?);
  let d2_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div2])?;
  let d2_obj = match d2_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr2 = get_data_property_value(scope.heap(), d2_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_class2 = Value::String(scope.alloc_string("class")?);
  let arg_foo = Value::String(scope.alloc_string("foo")?);
  vm.call_without_host(&mut scope, set_attr2, d2_val, &[arg_class2, arg_foo])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d2_val])?;

  let key_get_elements_by_class_name = PropertyKey::from_string(scope.alloc_string("getElementsByClassName")?);
  let get_elements_by_class_name =
    get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_class_name)
      .expect("getElementsByClassName should exist");

  let arg_query = Value::String(scope.alloc_string("foo  bar")?);
  let coll_val =
    vm.call_without_host(&mut scope, get_elements_by_class_name, document_val, &[arg_query])?;
  let coll_obj = match coll_val {
    Value::Object(o) => o,
    _ => panic!("expected collection object"),
  };

  let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
  let len1 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len1, Value::Number(1.0));

  let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
  let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
  assert_eq!(v0, d1_val);

  // Add a third element with both classes; the collection should update.
  let tag_div3 = Value::String(scope.alloc_string("div")?);
  let d3_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div3])?;
  let d3_obj = match d3_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr3 = get_data_property_value(scope.heap(), d3_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_class3 = Value::String(scope.alloc_string("class")?);
  let arg_bar_tab_foo = Value::String(scope.alloc_string("bar\tfoo baz")?);
  vm.call_without_host(&mut scope, set_attr3, d3_val, &[arg_class3, arg_bar_tab_foo])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d3_val])?;

  let len2 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len2, Value::Number(2.0));
  let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
  let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
  assert_eq!(v1, d3_val);

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn get_elements_by_name_matches_name_attribute() -> Result<(), VmError> {
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

  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");

  let tag_body = Value::String(scope.alloc_string("body")?);
  let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
  vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

  let tag_input = Value::String(scope.alloc_string("input")?);
  let i1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_input])?;
  let i1_obj = match i1_val {
    Value::Object(o) => o,
    _ => panic!("expected input wrapper"),
  };
  let set_attr = get_data_property_value(scope.heap(), i1_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_name = Value::String(scope.alloc_string("name")?);
  let arg_n = Value::String(scope.alloc_string("n")?);
  vm.call_without_host(&mut scope, set_attr, i1_val, &[arg_name, arg_n])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[i1_val])?;

  let tag_div = Value::String(scope.alloc_string("div")?);
  let d_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let d_obj = match d_val {
    Value::Object(o) => o,
    _ => panic!("expected div wrapper"),
  };
  let set_attr2 = get_data_property_value(scope.heap(), d_obj, &key_set_attribute).expect("setAttribute exists");
  let arg_name2 = Value::String(scope.alloc_string("name")?);
  let arg_n2 = Value::String(scope.alloc_string("n")?);
  vm.call_without_host(&mut scope, set_attr2, d_val, &[arg_name2, arg_n2])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d_val])?;

  let key_get_elements_by_name = PropertyKey::from_string(scope.alloc_string("getElementsByName")?);
  let get_elements_by_name = get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_name)
    .expect("getElementsByName should exist");

  let arg_q = Value::String(scope.alloc_string("n")?);
  let coll_val = vm.call_without_host(&mut scope, get_elements_by_name, document_val, &[arg_q])?;
  let coll_obj = match coll_val {
    Value::Object(o) => o,
    _ => panic!("expected collection object"),
  };

  let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
  let len = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len, Value::Number(2.0));

  let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
  let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
  let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
  let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
  assert_eq!(v0, i1_val);
  assert_eq!(v1, d_val);

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn get_elements_by_tag_name_ns_supports_html_namespace_and_wildcards() -> Result<(), VmError> {
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

  let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");

  let tag_body = Value::String(scope.alloc_string("body")?);
  let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
  vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

  let tag_div = Value::String(scope.alloc_string("div")?);
  let d_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  vm.call_without_host(&mut scope, append_child, body_val, &[d_val])?;

  let key_get_elements_by_tag_name_ns =
    PropertyKey::from_string(scope.alloc_string("getElementsByTagNameNS")?);
  let get_elements_by_tag_name_ns =
    get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name_ns)
      .expect("getElementsByTagNameNS should exist");

  let arg_ns = Value::String(scope.alloc_string("http://www.w3.org/1999/xhtml")?);
  let arg_div = Value::String(scope.alloc_string("DIV")?);
  let coll_val = vm.call_without_host(
    &mut scope,
    get_elements_by_tag_name_ns,
    document_val,
    &[arg_ns, arg_div],
  )?;
  let coll_obj = match coll_val {
    Value::Object(o) => o,
    _ => panic!("expected collection object"),
  };

  let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
  let len = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
  assert_eq!(len, Value::Number(1.0));

  let arg_ns2 = Value::String(scope.alloc_string("*")?);
  let arg_div2 = Value::String(scope.alloc_string("div")?);
  let coll2_val = vm.call_without_host(
    &mut scope,
    get_elements_by_tag_name_ns,
    document_val,
    &[arg_ns2, arg_div2],
  )?;
  let coll2_obj = match coll2_val {
    Value::Object(o) => o,
    _ => panic!("expected collection object"),
  };
  let len2 = get_data_property_value(scope.heap(), coll2_obj, &key_length).expect("length exists");
  assert_eq!(len2, Value::Number(1.0));

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
