use fastrender::dom2::Document;
use fastrender::js::vm_dom::{install_dom_bindings, install_dom_bindings_with_limits};
use fastrender::js::CurrentScriptState;
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{
  Heap, HeapLimits, Job, JsRuntime, PropertyKey, PropertyKind, Realm, RealmId, Scope, Value, Vm,
  VmError, VmHostHooks, VmOptions,
};

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

  // Identity/shape getters.
  let key_is_connected = PropertyKey::from_string(scope.alloc_string("isConnected")?);
  let is_connected_get = get_accessor_getter(scope.heap(), el_obj, &key_is_connected)
    .expect("isConnected getter should exist");
  let key_node_name = PropertyKey::from_string(scope.alloc_string("nodeName")?);
  let node_name_get = get_accessor_getter(scope.heap(), el_obj, &key_node_name)
    .expect("nodeName getter should exist");
  let key_node_value = PropertyKey::from_string(scope.alloc_string("nodeValue")?);
  let node_value_get = get_accessor_getter(scope.heap(), el_obj, &key_node_value)
    .expect("nodeValue getter should exist");
  let node_value_set = get_accessor_setter(scope.heap(), el_obj, &key_node_value)
    .expect("nodeValue setter should exist");
  let key_tag_name = PropertyKey::from_string(scope.alloc_string("tagName")?);
  let tag_name_get = get_accessor_getter(scope.heap(), el_obj, &key_tag_name)
    .expect("tagName getter should exist");
  let key_id_prop = PropertyKey::from_string(scope.alloc_string("id")?);
  let id_get = get_accessor_getter(scope.heap(), el_obj, &key_id_prop).expect("id getter exists");
  let key_class_name = PropertyKey::from_string(scope.alloc_string("className")?);
  let class_name_get =
    get_accessor_getter(scope.heap(), el_obj, &key_class_name).expect("className getter exists");
  let class_name_set = get_accessor_setter(scope.heap(), el_obj, &key_class_name)
    .expect("className setter exists");
  let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
  let text_content_get =
    get_accessor_getter(scope.heap(), el_obj, &key_text_content).expect("textContent getter exists");
  let text_content_set =
    get_accessor_setter(scope.heap(), el_obj, &key_text_content).expect("textContent setter exists");

  // A freshly created element is not yet connected to the document tree.
  assert_eq!(
    vm.call_without_host(&mut scope, is_connected_get, el_val, &[])?,
    Value::Bool(false)
  );

  let node_name = vm.call_without_host(&mut scope, node_name_get, document_val, &[])?;
  let Value::String(node_name_str) = node_name else {
    panic!("expected nodeName string");
  };
  assert_eq!(scope.heap().get_string(node_name_str)?.to_utf8_lossy(), "#document");

  let node_name = vm.call_without_host(&mut scope, node_name_get, el_val, &[])?;
  let Value::String(node_name_str) = node_name else {
    panic!("expected nodeName string");
  };
  assert_eq!(scope.heap().get_string(node_name_str)?.to_utf8_lossy(), "DIV");

  let tag_name = vm.call_without_host(&mut scope, tag_name_get, el_val, &[])?;
  let Value::String(tag_name_str) = tag_name else {
    panic!("expected tagName string");
  };
  assert_eq!(scope.heap().get_string(tag_name_str)?.to_utf8_lossy(), "DIV");

  assert!(matches!(
    vm.call_without_host(&mut scope, node_value_get, document_val, &[])?,
    Value::Null
  ));
  assert!(matches!(
    vm.call_without_host(&mut scope, node_value_get, el_val, &[])?,
    Value::Null
  ));

  let id = vm.call_without_host(&mut scope, id_get, el_val, &[])?;
  let Value::String(id_str) = id else {
    panic!("expected id string");
  };
  assert!(scope.heap().get_string(id_str)?.to_utf8_lossy().is_empty());

  let class_name = vm.call_without_host(&mut scope, class_name_get, el_val, &[])?;
  let Value::String(class_name_str) = class_name else {
    panic!("expected className string");
  };
  assert!(scope
    .heap()
    .get_string(class_name_str)?
    .to_utf8_lossy()
    .is_empty());

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

  let id = vm.call_without_host(&mut scope, id_get, el_val, &[])?;
  let Value::String(id_str) = id else {
    panic!("expected id string");
  };
  assert_eq!(scope.heap().get_string(id_str)?.to_utf8_lossy(), "foo");

  // document.appendChild(el)
  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");
  let appended = vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;
  assert_eq!(appended, el_val, "appendChild should return the child");

  assert_eq!(
    vm.call_without_host(&mut scope, is_connected_get, el_val, &[])?,
    Value::Bool(true)
  );

  // document.hasChildNodes() should now return true.
  let doc_has_children =
    vm.call_without_host(&mut scope, has_child_nodes, document_val, &[])?;
  assert_eq!(doc_has_children, Value::Bool(true));

  // className setter updates the backing attribute.
  let arg_class = Value::String(scope.alloc_string("a b")?);
  vm.call_without_host(&mut scope, class_name_set, el_val, &[arg_class])?;
  let class_name = vm.call_without_host(&mut scope, class_name_get, el_val, &[])?;
  let Value::String(class_name_str) = class_name else {
    panic!("expected className string");
  };
  assert_eq!(scope.heap().get_string(class_name_str)?.to_utf8_lossy(), "a b");

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

  // nodeValue behavior for Text nodes.
  let arg_hello = Value::String(scope.alloc_string("hello")?);
  vm.call_without_host(&mut scope, text_content_set, child1, &[arg_hello])?;

  let text_node = vm.call_without_host(&mut scope, first_child_get, child1, &[])?;
  assert_eq!(
    vm.call_without_host(&mut scope, node_type_get, text_node, &[])?,
    Value::Number(3.0)
  );

  let text_node_name = vm.call_without_host(&mut scope, node_name_get, text_node, &[])?;
  let Value::String(text_node_name_str) = text_node_name else {
    panic!("expected nodeName string");
  };
  assert_eq!(
    scope.heap().get_string(text_node_name_str)?.to_utf8_lossy(),
    "#text"
  );

  let text_node_value = vm.call_without_host(&mut scope, node_value_get, text_node, &[])?;
  let Value::String(text_node_value_str) = text_node_value else {
    panic!("expected nodeValue string");
  };
  assert_eq!(
    scope
      .heap()
      .get_string(text_node_value_str)?
      .to_utf8_lossy(),
    "hello"
  );

  let arg_bye = Value::String(scope.alloc_string("bye")?);
  vm.call_without_host(&mut scope, node_value_set, text_node, &[arg_bye])?;

  let text_node_value = vm.call_without_host(&mut scope, node_value_get, text_node, &[])?;
  let Value::String(text_node_value_str) = text_node_value else {
    panic!("expected nodeValue string");
  };
  assert_eq!(
    scope
      .heap()
      .get_string(text_node_value_str)?
      .to_utf8_lossy(),
    "bye"
  );

  let child_text = vm.call_without_host(&mut scope, text_content_get, child1, &[])?;
  let Value::String(child_text_str) = child_text else {
    panic!("expected textContent string");
  };
  assert_eq!(
    scope.heap().get_string(child_text_str)?.to_utf8_lossy(),
    "bye"
  );

  // Inert template contents: `<template>` should not expose children via Node navigation.
  let tag_template = Value::String(scope.alloc_string("template")?);
  let template = vm.call_without_host(&mut scope, create_element, document_val, &[tag_template])?;
  vm.call_without_host(&mut scope, append_child, el_val, &[template])?;

  let arg_inert = Value::String(scope.alloc_string("INERT")?);
  vm.call_without_host(&mut scope, text_content_set, template, &[arg_inert])?;

  let template_has_children =
    vm.call_without_host(&mut scope, has_child_nodes, template, &[])?;
  assert_eq!(template_has_children, Value::Bool(false));
  assert_eq!(
    vm.call_without_host(&mut scope, first_child_get, template, &[])?,
    Value::Null
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
    Err(err) => match err.thrown_value() {
      Some(v) => v,
      None => return Err(err),
    },
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
  let msg: Result<String, VmError> = (|| {
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

    let thrown = match err.thrown_value() {
      Some(v) => v,
      None => return Err(err),
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
    Ok(scope.heap().get_string(message_str)?.to_utf8_lossy().to_string())
  })();

  // Ensure teardown runs even if assertions fail, otherwise `Realm` will panic in Drop while the
  // test is already unwinding.
  drop(scope);
  realm.teardown(&mut heap);

  let msg = msg?;
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
fn element_inner_html_and_outer_html_round_trip() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

  struct Recorded {
    inner_html_initial: String,
    inner_html_round_trip: String,
    outer_html_round_trip: String,
    span_collection_len_initial: f64,
    span_collection_len_after_insert: f64,
    span_collection_0_identity_preserved: bool,
    span_collection_len_after_script: f64,
    span_node_type: f64,
    tail_node_type: f64,
    span_text: String,
    tail_text: String,
    div_text: String,
    child_identity_preserved: bool,
    script_already_started: bool,
    old_wrapper_disconnected: bool,
    replaced_parent_is_body: bool,
    replaced_text: String,
    target_node_detached: bool,
  }

  let mut scope = heap.scope();
  let recorded: Result<Recorded, VmError> = (|| {
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .ok_or(VmError::InvariantViolation(
        "globalThis.document should be defined",
      ))?;
    let Value::Object(document_obj) = document_val else {
      return Err(VmError::InvariantViolation("document should be an object"));
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .ok_or(VmError::InvariantViolation(
        "document.createElement should exist",
      ))?;

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .ok_or(VmError::InvariantViolation("appendChild should exist"))?;

    // Document nodes can only have one element child; use a `<body>` element as the root parent so
    // `outerHTML` replacement does not attempt to modify a direct child of the `Document`.
    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
    let Value::Object(_body_obj) = body_val else {
      return Err(VmError::InvariantViolation("createElement(body) should return an object"));
    };
    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    let tag_div = Value::String(scope.alloc_string("div")?);
    let div_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let Value::Object(div_obj) = div_val else {
      return Err(VmError::InvariantViolation("createElement(div) should return an object"));
    };

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let set_attribute = get_data_property_value(scope.heap(), div_obj, &key_set_attribute)
      .ok_or(VmError::InvariantViolation("setAttribute should exist"))?;
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_target = Value::String(scope.alloc_string("target")?);
    vm.call_without_host(&mut scope, set_attribute, div_val, &[arg_id, arg_target])?;

    vm.call_without_host(&mut scope, append_child, body_val, &[div_val])?;

    // Create a live HTMLCollection before inserting any matching elements so we can ensure updates
    // occur when `innerHTML` mutates the DOM.
    let key_get_elements_by_tag_name =
      PropertyKey::from_string(scope.alloc_string("getElementsByTagName")?);
    let get_elements_by_tag_name =
      get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name).ok_or(
        VmError::InvariantViolation("document.getElementsByTagName should exist"),
      )?;
    let arg_span = Value::String(scope.alloc_string("span")?);
    let span_coll_val =
      vm.call_without_host(&mut scope, get_elements_by_tag_name, document_val, &[arg_span])?;
    let Value::Object(span_coll_obj) = span_coll_val else {
      return Err(VmError::InvariantViolation("expected an HTMLCollection object"));
    };
    let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
    let span_collection_len_initial = get_data_property_value(scope.heap(), span_coll_obj, &key_length)
      .ok_or(VmError::InvariantViolation(
        "HTMLCollection.length should exist",
      ))?;
    let Value::Number(span_collection_len_initial) = span_collection_len_initial else {
      return Err(VmError::InvariantViolation(
        "HTMLCollection.length should be a number",
      ));
    };

    let (body_id, target_id) = {
      let dom_ref = dom.borrow();
      let body_id = dom_ref
        .document_element()
        .ok_or(VmError::InvariantViolation("missing body element"))?;
      let target_id = dom_ref
        .get_element_by_id("target")
        .ok_or(VmError::InvariantViolation("missing #target element"))?;
      (body_id, target_id)
    };

    let key_inner_html = PropertyKey::from_string(scope.alloc_string("innerHTML")?);
    let inner_html_get = get_accessor_getter(scope.heap(), div_obj, &key_inner_html)
      .ok_or(VmError::InvariantViolation("innerHTML getter should exist"))?;
    let inner_html_set = get_accessor_setter(scope.heap(), div_obj, &key_inner_html)
      .ok_or(VmError::InvariantViolation("innerHTML setter should exist"))?;

    let inner_html_initial = vm.call_without_host(&mut scope, inner_html_get, div_val, &[])?;
    let Value::String(inner_html_initial) = inner_html_initial else {
      return Err(VmError::InvariantViolation("innerHTML getter should return a string"));
    };
    let inner_html_initial = scope.heap().get_string(inner_html_initial)?.to_utf8_lossy().to_string();

    let arg_html = Value::String(scope.alloc_string("<span id=child>hi</span>tail")?);
    vm.call_without_host(&mut scope, inner_html_set, div_val, &[arg_html])?;

    let span_collection_len_after_insert =
      get_data_property_value(scope.heap(), span_coll_obj, &key_length).ok_or(
        VmError::InvariantViolation("HTMLCollection.length should exist after innerHTML set"),
      )?;
    let Value::Number(span_collection_len_after_insert) = span_collection_len_after_insert else {
      return Err(VmError::InvariantViolation(
        "HTMLCollection.length should be a number after innerHTML set",
      ));
    };

    let inner_html_round_trip = vm.call_without_host(&mut scope, inner_html_get, div_val, &[])?;
    let Value::String(inner_html_round_trip) = inner_html_round_trip else {
      return Err(VmError::InvariantViolation("innerHTML getter should return a string"));
    };
    let inner_html_round_trip = scope
      .heap()
      .get_string(inner_html_round_trip)?
      .to_utf8_lossy()
      .to_string();

    // Validate Node navigation for the newly inserted children.
    let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
    let first_child_get = get_accessor_getter(scope.heap(), div_obj, &key_first_child)
      .ok_or(VmError::InvariantViolation("firstChild getter should exist"))?;
    let key_next_sibling = PropertyKey::from_string(scope.alloc_string("nextSibling")?);
    let next_sibling_get = get_accessor_getter(scope.heap(), div_obj, &key_next_sibling)
      .ok_or(VmError::InvariantViolation("nextSibling getter should exist"))?;
    let key_node_type = PropertyKey::from_string(scope.alloc_string("nodeType")?);
    let node_type_get = get_accessor_getter(scope.heap(), div_obj, &key_node_type)
      .ok_or(VmError::InvariantViolation("nodeType getter should exist"))?;
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_get = get_accessor_getter(scope.heap(), div_obj, &key_text_content)
      .ok_or(VmError::InvariantViolation("textContent getter should exist"))?;

    let span_val = vm.call_without_host(&mut scope, first_child_get, div_val, &[])?;
    let Value::Object(_span_obj) = span_val else {
      return Err(VmError::InvariantViolation("firstChild should return an object"));
    };

    let span_collection_0_identity_preserved = {
      let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
      let v0 = get_data_property_value(scope.heap(), span_coll_obj, &key_0)
        .ok_or(VmError::InvariantViolation("HTMLCollection[0] should exist"))?;
      v0 == span_val
    };

    let tail_val = vm.call_without_host(&mut scope, next_sibling_get, span_val, &[])?;
    let Value::Object(_tail_obj) = tail_val else {
      return Err(VmError::InvariantViolation("nextSibling should return an object"));
    };

    let span_node_type = vm.call_without_host(&mut scope, node_type_get, span_val, &[])?;
    let Value::Number(span_node_type) = span_node_type else {
      return Err(VmError::InvariantViolation("nodeType should return a number"));
    };
    let tail_node_type = vm.call_without_host(&mut scope, node_type_get, tail_val, &[])?;
    let Value::Number(tail_node_type) = tail_node_type else {
      return Err(VmError::InvariantViolation("nodeType should return a number"));
    };

    let span_text = vm.call_without_host(&mut scope, text_content_get, span_val, &[])?;
    let Value::String(span_text) = span_text else {
      return Err(VmError::InvariantViolation("textContent should return a string"));
    };
    let span_text = scope.heap().get_string(span_text)?.to_utf8_lossy().to_string();

    let tail_text = vm.call_without_host(&mut scope, text_content_get, tail_val, &[])?;
    let Value::String(tail_text) = tail_text else {
      return Err(VmError::InvariantViolation("textContent should return a string"));
    };
    let tail_text = scope.heap().get_string(tail_text)?.to_utf8_lossy().to_string();

    let div_text = vm.call_without_host(&mut scope, text_content_get, div_val, &[])?;
    let Value::String(div_text) = div_text else {
      return Err(VmError::InvariantViolation("textContent should return a string"));
    };
    let div_text = scope.heap().get_string(div_text)?.to_utf8_lossy().to_string();

    // document.getElementById should be able to find the inserted child element and return the same
    // wrapper object (identity cache).
    let key_get_element_by_id = PropertyKey::from_string(scope.alloc_string("getElementById")?);
    let get_element_by_id = get_data_property_value(scope.heap(), document_obj, &key_get_element_by_id)
      .ok_or(VmError::InvariantViolation("getElementById should exist"))?;
    let arg_child = Value::String(scope.alloc_string("child")?);
    let child_val = vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_child])?;
    let child_identity_preserved = child_val == span_val;

    // outerHTML getter serializes the element itself.
    let key_outer_html = PropertyKey::from_string(scope.alloc_string("outerHTML")?);
    let outer_html_get = get_accessor_getter(scope.heap(), div_obj, &key_outer_html)
      .ok_or(VmError::InvariantViolation("outerHTML getter should exist"))?;
    let outer_html_set = get_accessor_setter(scope.heap(), div_obj, &key_outer_html)
      .ok_or(VmError::InvariantViolation("outerHTML setter should exist"))?;

    let outer_html_round_trip = vm.call_without_host(&mut scope, outer_html_get, div_val, &[])?;
    let Value::String(outer_html_round_trip) = outer_html_round_trip else {
      return Err(VmError::InvariantViolation("outerHTML getter should return a string"));
    };
    let outer_html_round_trip = scope
      .heap()
      .get_string(outer_html_round_trip)?
      .to_utf8_lossy()
      .to_string();

    // Insert a script via innerHTML; the Rust-side DOM should mark it as already started.
    let arg_script = Value::String(scope.alloc_string("<script id=s>console.log(1)</script>")?);
    vm.call_without_host(&mut scope, inner_html_set, div_val, &[arg_script])?;

    let span_collection_len_after_script =
      get_data_property_value(scope.heap(), span_coll_obj, &key_length).ok_or(
        VmError::InvariantViolation("HTMLCollection.length should exist after script innerHTML"),
      )?;
    let Value::Number(span_collection_len_after_script) = span_collection_len_after_script else {
      return Err(VmError::InvariantViolation(
        "HTMLCollection.length should be a number after script innerHTML",
      ));
    };

    let script_already_started = {
      let dom_ref = dom.borrow();
      let script_id = dom_ref
        .get_element_by_id("s")
        .ok_or(VmError::InvariantViolation("expected script inserted via innerHTML"))?;
      dom_ref.node(script_id).script_already_started
    };

    // outerHTML setter replaces the element and should disconnect the old wrapper (parentNode=null).
    let arg_replacement = Value::String(scope.alloc_string("<p id=replaced>ok</p>")?);
    vm.call_without_host(&mut scope, outer_html_set, div_val, &[arg_replacement])?;

    let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
    let parent_node_get = get_accessor_getter(scope.heap(), div_obj, &key_parent_node)
      .ok_or(VmError::InvariantViolation("parentNode getter should exist"))?;
    let div_parent = vm.call_without_host(&mut scope, parent_node_get, div_val, &[])?;
    let old_wrapper_disconnected = matches!(div_parent, Value::Null);

    let arg_replaced = Value::String(scope.alloc_string("replaced")?);
    let replaced_val =
      vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_replaced])?;
    let Value::Object(_replaced_obj) = replaced_val else {
      return Err(VmError::InvariantViolation("expected replaced element wrapper"));
    };
    let replaced_parent = vm.call_without_host(&mut scope, parent_node_get, replaced_val, &[])?;
    let replaced_parent_is_body = replaced_parent == body_val;

    let replaced_text = vm.call_without_host(&mut scope, text_content_get, replaced_val, &[])?;
    let Value::String(replaced_text) = replaced_text else {
      return Err(VmError::InvariantViolation("textContent should return a string"));
    };
    let replaced_text = scope.heap().get_string(replaced_text)?.to_utf8_lossy().to_string();

    let target_node_detached = {
      let dom_ref = dom.borrow();
      let parent = dom_ref
        .parent(target_id)
        .map_err(|_| VmError::InvariantViolation("dom.parent failed for original target"))?;
      parent.is_none()
    };

    // Also validate that the new element is connected under the original `<body>` element.
    {
      let dom_ref = dom.borrow();
      let replaced_id = dom_ref
        .get_element_by_id("replaced")
        .ok_or(VmError::InvariantViolation("expected #replaced to exist in DOM"))?;
      let replaced_parent = dom_ref
        .parent(replaced_id)
        .map_err(|_| VmError::InvariantViolation("dom.parent failed for #replaced"))?;
      if replaced_parent != Some(body_id) {
        return Err(VmError::InvariantViolation(
          "#replaced should be a child of the body element",
        ));
      }
    }

    Ok(Recorded {
      inner_html_initial,
      inner_html_round_trip,
      outer_html_round_trip,
      span_collection_len_initial,
      span_collection_len_after_insert,
      span_collection_0_identity_preserved,
      span_collection_len_after_script,
      span_node_type,
      tail_node_type,
      span_text,
      tail_text,
      div_text,
      child_identity_preserved,
      script_already_started,
      old_wrapper_disconnected,
      replaced_parent_is_body,
      replaced_text,
      target_node_detached,
    })
  })();

  drop(scope);
  realm.teardown(&mut heap);

  let recorded = recorded?;

  assert_eq!(recorded.inner_html_initial, "");
  assert_eq!(
    recorded.inner_html_round_trip,
    "<span id=\"child\">hi</span>tail"
  );
  assert_eq!(recorded.span_collection_len_initial, 0.0);
  assert_eq!(recorded.span_collection_len_after_insert, 1.0);
  assert!(recorded.span_collection_0_identity_preserved);
  assert_eq!(recorded.span_collection_len_after_script, 0.0);
  assert_eq!(recorded.span_node_type, 1.0);
  assert_eq!(recorded.tail_node_type, 3.0);
  assert_eq!(recorded.span_text, "hi");
  assert_eq!(recorded.tail_text, "tail");
  assert_eq!(recorded.div_text, "hitail");
  assert!(recorded.child_identity_preserved);
  assert_eq!(
    recorded.outer_html_round_trip,
    "<div id=\"target\"><span id=\"child\">hi</span>tail</div>"
  );
  assert!(recorded.script_already_started);
  assert!(recorded.old_wrapper_disconnected);
  assert!(recorded.replaced_parent_is_body);
  assert_eq!(recorded.replaced_text, "ok");
  assert!(recorded.target_node_detached);

  Ok(())
}

#[test]
fn element_insert_adjacent_html_element_and_text() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

  struct Recorded {
    bad_position_error_name: String,
    insert_adjacent_element_returns_arg: bool,
    body_child_ids: Vec<Option<String>>,
    target_child_repr: Vec<String>,
    script_already_started: bool,
  }

  let mut scope = heap.scope();
  let recorded: Result<Recorded, VmError> = (|| {
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .ok_or(VmError::InvariantViolation(
        "globalThis.document should be defined",
      ))?;
    let Value::Object(document_obj) = document_val else {
      return Err(VmError::InvariantViolation("document should be an object"));
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element =
      get_data_property_value(scope.heap(), document_obj, &key_create_element).ok_or(
        VmError::InvariantViolation("document.createElement should exist"),
      )?;

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child =
      get_data_property_value(scope.heap(), document_obj, &key_append_child).ok_or(
        VmError::InvariantViolation("appendChild should exist"),
      )?;

    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    // <div id="target"></div>
    let tag_div = Value::String(scope.alloc_string("div")?);
    let target_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let Value::Object(target_obj) = target_val else {
      return Err(VmError::InvariantViolation("expected target element wrapper"));
    };
    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let set_attribute =
      get_data_property_value(scope.heap(), target_obj, &key_set_attribute).ok_or(
        VmError::InvariantViolation("setAttribute should exist"),
      )?;
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_target = Value::String(scope.alloc_string("target")?);
    vm.call_without_host(&mut scope, set_attribute, target_val, &[arg_id, arg_target])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[target_val])?;

    // Grab insertAdjacent* methods.
    let key_insert_adjacent_html =
      PropertyKey::from_string(scope.alloc_string("insertAdjacentHTML")?);
    let insert_adjacent_html =
      get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_html).ok_or(
        VmError::InvariantViolation("insertAdjacentHTML should exist"),
      )?;
    let key_insert_adjacent_element =
      PropertyKey::from_string(scope.alloc_string("insertAdjacentElement")?);
    let insert_adjacent_element =
      get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_element).ok_or(
        VmError::InvariantViolation("insertAdjacentElement should exist"),
      )?;
    let key_insert_adjacent_text =
      PropertyKey::from_string(scope.alloc_string("insertAdjacentText")?);
    let insert_adjacent_text =
      get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_text).ok_or(
        VmError::InvariantViolation("insertAdjacentText should exist"),
      )?;

    // Invalid position throws SyntaxError.
    let bad_position_error_name = {
      let bad = Value::String(scope.alloc_string("nope")?);
      let html = Value::String(scope.alloc_string("<b>bad</b>")?);
      let thrown =
        match vm.call_without_host(&mut scope, insert_adjacent_html, target_val, &[bad, html]) {
          Ok(_) => {
            return Err(VmError::InvariantViolation(
              "expected insertAdjacentHTML to throw",
            ));
          }
          Err(err) => match err.thrown_value() {
            Some(v) => v,
            None => return Err(err),
          },
        };
      let Value::Object(thrown_obj) = thrown else {
        return Err(VmError::InvariantViolation("thrown value should be an object"));
      };
      let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
      let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name)
        .ok_or(VmError::InvariantViolation("thrown error should have .name"))?;
      let Value::String(name_val) = name_val else {
        return Err(VmError::InvariantViolation(".name should be a string"));
      };
      scope.heap().get_string(name_val)?.to_utf8_lossy().to_string()
    };

    // beforebegin + afterend around the target.
    let pos_before = Value::String(scope.alloc_string("beforebegin")?);
    let html_before = Value::String(scope.alloc_string("<p id=before>one</p>")?);
    vm.call_without_host(&mut scope, insert_adjacent_html, target_val, &[pos_before, html_before])?;

    let pos_after = Value::String(scope.alloc_string("afterend")?);
    let html_after = Value::String(scope.alloc_string("<p id=after>two</p>")?);
    vm.call_without_host(&mut scope, insert_adjacent_html, target_val, &[pos_after, html_after])?;

    // afterbegin + beforeend inside the target.
    let pos_after_begin = Value::String(scope.alloc_string("afterbegin")?);
    let html_first = Value::String(scope.alloc_string("<span id=first>first</span>")?);
    vm.call_without_host(
      &mut scope,
      insert_adjacent_html,
      target_val,
      &[pos_after_begin, html_first],
    )?;

    let pos_before_end = Value::String(scope.alloc_string("beforeend")?);
    let html_last = Value::String(scope.alloc_string("<span id=last>last</span>")?);
    vm.call_without_host(
      &mut scope,
      insert_adjacent_html,
      target_val,
      &[pos_before_end, html_last],
    )?;

    // insertAdjacentElement(beforebegin, <section id=moved>).
    let tag_section = Value::String(scope.alloc_string("section")?);
    let moved_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_section])?;
    let Value::Object(moved_obj) = moved_val else {
      return Err(VmError::InvariantViolation("expected moved element wrapper"));
    };
    let set_attribute_moved =
      get_data_property_value(scope.heap(), moved_obj, &key_set_attribute).ok_or(
        VmError::InvariantViolation("setAttribute should exist on moved element"),
      )?;
    let arg_id2 = Value::String(scope.alloc_string("id")?);
    let arg_moved = Value::String(scope.alloc_string("moved")?);
    vm.call_without_host(&mut scope, set_attribute_moved, moved_val, &[arg_id2, arg_moved])?;

    let where_before_begin = Value::String(scope.alloc_string("beforebegin")?);
    let inserted =
      vm.call_without_host(&mut scope, insert_adjacent_element, target_val, &[where_before_begin, moved_val])?;
    let insert_adjacent_element_returns_arg = inserted == moved_val;

    // insertAdjacentText(beforeend, "tail") and then a script.
    let where_before_end = Value::String(scope.alloc_string("beforeend")?);
    let data_tail = Value::String(scope.alloc_string("tail")?);
    vm.call_without_host(
      &mut scope,
      insert_adjacent_text,
      target_val,
      &[where_before_end, data_tail],
    )?;

    let where_before_end2 = Value::String(scope.alloc_string("beforeend")?);
    let html_script =
      Value::String(scope.alloc_string("<script id=s>console.log(1)</script>")?);
    vm.call_without_host(
      &mut scope,
      insert_adjacent_html,
      target_val,
      &[where_before_end2, html_script],
    )?;

    // Inspect the Rust-side dom2 tree for structure and script flags.
    let (body_child_ids, target_child_repr, script_already_started) = {
      let dom_ref = dom.borrow();
      let body_id = dom_ref
        .document_element()
        .ok_or(VmError::InvariantViolation("missing <body> element"))?;

      let body_child_ids: Vec<Option<String>> = dom_ref
        .children(body_id)
        .map_err(|_| VmError::InvariantViolation("dom.children(body) failed"))?
        .iter()
        .map(|&child| {
          dom_ref
            .get_attribute(child, "id")
            .ok()
            .flatten()
            .map(str::to_string)
        })
        .collect();

      let target_id = dom_ref
        .get_element_by_id("target")
        .ok_or(VmError::InvariantViolation("missing #target element"))?;
      let target_children = dom_ref
        .children(target_id)
        .map_err(|_| VmError::InvariantViolation("dom.children(target) failed"))?;

      let target_child_repr: Vec<String> = target_children
        .iter()
        .map(|&child| {
          let node = dom_ref.node(child);
          match &node.kind {
            fastrender::dom2::NodeKind::Text { content } => format!("#text:{content}"),
            fastrender::dom2::NodeKind::Element { tag_name, .. } => {
              let tag = tag_name.to_ascii_lowercase();
              let id = dom_ref.get_attribute(child, "id").ok().flatten().unwrap_or("");
              if id.is_empty() {
                tag
              } else {
                format!("{tag}#{id}")
              }
            }
            fastrender::dom2::NodeKind::Slot { .. } => "slot".to_string(),
            other => format!("{other:?}"),
          }
        })
        .collect();

      let script_id = dom_ref
        .get_element_by_id("s")
        .ok_or(VmError::InvariantViolation("missing inserted script"))?;
      let script_already_started = dom_ref.node(script_id).script_already_started;

      (body_child_ids, target_child_repr, script_already_started)
    };

    Ok(Recorded {
      bad_position_error_name,
      insert_adjacent_element_returns_arg,
      body_child_ids,
      target_child_repr,
      script_already_started,
    })
  })();

  drop(scope);
  realm.teardown(&mut heap);

  let recorded = recorded?;
  assert_eq!(recorded.bad_position_error_name, "SyntaxError");
  assert!(recorded.insert_adjacent_element_returns_arg);

  let body_ids: Vec<Option<&str>> = recorded
    .body_child_ids
    .iter()
    .map(|v| v.as_deref())
    .collect();
  assert_eq!(
    body_ids,
    vec![Some("before"), Some("moved"), Some("target"), Some("after")]
  );

  // Expected child order:
  // <span id=first>, <span id=last>, "tail", <script id=s>
  assert_eq!(
    recorded.target_child_repr,
    vec![
      "span#first".to_string(),
      "span#last".to_string(),
      "#text:tail".to_string(),
      "script#s".to_string(),
    ]
  );
  assert!(recorded.script_already_started);
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

  // replace(token, newToken) reflects to the backing `class` attribute.
  let key_replace = PropertyKey::from_string(scope.alloc_string("replace")?);
  let replace = get_data_property_value(scope.heap(), list_obj, &key_replace).expect("replace exists");
  let arg_b3 = Value::String(scope.alloc_string("b")?);
  let arg_d = Value::String(scope.alloc_string("d")?);
  let replaced = vm.call_without_host(&mut scope, replace, list1, &[arg_b3, arg_d])?;
  assert_eq!(replaced, Value::Bool(true));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("d")
  );

  let arg_nope = Value::String(scope.alloc_string("nope")?);
  let arg_x = Value::String(scope.alloc_string("x")?);
  let replaced = vm.call_without_host(&mut scope, replace, list1, &[arg_nope, arg_x])?;
  assert_eq!(replaced, Value::Bool(false));
  assert_eq!(
    dom.borrow().get_attribute(e1_id, "class").unwrap(),
    Some("d")
  );

  // Invalid tokens (ASCII whitespace) throw SyntaxError.
  let bad = Value::String(scope.alloc_string("bad token")?);
  let thrown = match vm.call_without_host(&mut scope, add, list1, &[bad]) {
    Ok(_) => panic!("expected classList.add to throw for invalid token"),
    Err(err) => match err.thrown_value() {
      Some(v) => v,
      None => return Err(err),
    },
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
fn dataset_and_style_shims_reflect_to_attributes() -> Result<(), VmError> {
  const DOM_STRING_MAP_HOST_KIND: u64 = 4;

  #[derive(Clone)]
  struct DatasetHooks {
    dom: Rc<RefCell<Document>>,
  }

  impl VmHostHooks for DatasetHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
      // This test does not enqueue Promise jobs. If that changes, the hook should discard the job
      // via a real `VmJobContext` to avoid leaking persistent roots.
      panic!("unexpected Promise job in dataset/style shim test");
    }

    fn host_exotic_get(
      &mut self,
      scope: &mut Scope<'_>,
      obj: vm_js::GcObject,
      key: PropertyKey,
      receiver: Value,
    ) -> Result<Option<Value>, VmError> {
      let _ = receiver;

      let slots = scope.heap().object_host_slots(obj)?;
      let Some(slots) = slots else {
        return Ok(None);
      };
      if slots.b != DOM_STRING_MAP_HOST_KIND {
        return Ok(None);
      }

      let PropertyKey::String(prop_s) = key else {
        return Ok(None);
      };

      let node_index = match usize::try_from(slots.a) {
        Ok(v) => v,
        Err(_) => return Ok(None),
      };
      let node_id = match self.dom.borrow().node_id_from_index(node_index) {
        Ok(id) => id,
        Err(_) => return Ok(None),
      };

      let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();
      let dom = self.dom.borrow();
      let Some(value) = dom.dataset_get(node_id, &prop) else {
        return Ok(None);
      };
      Ok(Some(Value::String(scope.alloc_string(value)?)))
    }

    fn host_exotic_set(
      &mut self,
      scope: &mut Scope<'_>,
      obj: vm_js::GcObject,
      key: PropertyKey,
      value: Value,
      receiver: Value,
    ) -> Result<Option<bool>, VmError> {
      let _ = receiver;

      let slots = scope.heap().object_host_slots(obj)?;
      let Some(slots) = slots else {
        return Ok(None);
      };
      if slots.b != DOM_STRING_MAP_HOST_KIND {
        return Ok(None);
      }

      let PropertyKey::String(prop_s) = key else {
        return Ok(None);
      };

      let node_index = match usize::try_from(slots.a) {
        Ok(v) => v,
        Err(_) => return Ok(None),
      };
      let node_id = match self.dom.borrow().node_id_from_index(node_index) {
        Ok(id) => id,
        Err(_) => return Ok(None),
      };

      let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

      let value_s = scope.heap_mut().to_string(value)?;
      let value = scope.heap().get_string(value_s)?.to_utf8_lossy();

      self
        .dom
        .borrow_mut()
        .dataset_set(node_id, &prop, &value)
        .map_err(|_| VmError::TypeError("failed to set dataset property"))?;

      Ok(Some(true))
    }

    fn host_exotic_delete(
      &mut self,
      scope: &mut Scope<'_>,
      obj: vm_js::GcObject,
      key: PropertyKey,
    ) -> Result<Option<bool>, VmError> {
      let slots = scope.heap().object_host_slots(obj)?;
      let Some(slots) = slots else {
        return Ok(None);
      };
      if slots.b != DOM_STRING_MAP_HOST_KIND {
        return Ok(None);
      }

      let PropertyKey::String(prop_s) = key else {
        return Ok(None);
      };

      let node_index = match usize::try_from(slots.a) {
        Ok(v) => v,
        Err(_) => return Ok(None),
      };
      let node_id = match self.dom.borrow().node_id_from_index(node_index) {
        Ok(id) => id,
        Err(_) => return Ok(None),
      };

      let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

      self
        .dom
        .borrow_mut()
        .dataset_delete(node_id, &prop)
        .map_err(|_| VmError::TypeError("failed to delete dataset property"))?;

      Ok(Some(true))
    }
  }

  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(limits);
  let mut rt = JsRuntime::new(vm, heap)?;

  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  let realm_ptr = rt.realm() as *const Realm;
  // SAFETY: `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields. We do not move
  // `rt` while these borrows are live.
  let realm = unsafe { &*realm_ptr };
  install_dom_bindings(&mut rt.vm, &mut rt.heap, realm, dom.clone(), current_script)?;

  let mut hooks = DatasetHooks { dom: dom.clone() };
  let ok = rt.exec_script_with_hooks(
    &mut hooks,
    "(() => {\n\
      const el = document.createElement('div');\n\
      el.id = 't';\n\
      document.appendChild(el);\n\
      el.dataset.fooBar = 'baz';\n\
      const got = el.dataset.fooBar;\n\
      delete el.dataset.fooBar;\n\
      const missing = el.dataset.fooBar;\n\
      el.style.setProperty('backgroundColor', 'red');\n\
      const style = el.style.getPropertyValue('background-color');\n\
      return got === 'baz' && missing === undefined && style === 'red';\n\
    })()",
  )?;
  assert_eq!(ok, Value::Bool(true));

  let t = dom.borrow().get_element_by_id("t").expect("missing #t");
  assert_eq!(dom.borrow().get_attribute(t, "data-foo-bar").unwrap(), None);
  assert_eq!(
    dom.borrow().get_attribute(t, "style").unwrap(),
    Some("background-color: red;")
  );

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

#[test]
fn node_clone_node_deep_clones_detached_subtree() -> Result<(), VmError> {
  let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
  let mut heap = Heap::new(limits);
  let mut vm = Vm::new(VmOptions::default());

  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
  let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
  install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

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
  let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
    .expect("document.createElement should exist");

  let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
  let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
    .expect("appendChild exists");

  // document.createElement("div")
  let tag_div = Value::String(scope.alloc_string("div")?);
  let div_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
  let Value::Object(div_obj) = div_val else {
    panic!("expected div wrapper");
  };

  // div.id = "src" (via setter).
  let key_id = PropertyKey::from_string(scope.alloc_string("id")?);
  let id_set = get_accessor_setter(scope.heap(), div_obj, &key_id).expect("id setter exists");
  let arg_src = Value::String(scope.alloc_string("src")?);
  vm.call_without_host(&mut scope, id_set, div_val, &[arg_src])?;

  // Connect the original: document.appendChild(div)
  vm.call_without_host(&mut scope, append_child, document_val, &[div_val])?;

  // Add a child: <span>hello</span>
  let tag_span = Value::String(scope.alloc_string("span")?);
  let span_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
  vm.call_without_host(&mut scope, append_child, div_val, &[span_val])?;

  let Value::Object(span_obj) = span_val else {
    panic!("expected span wrapper");
  };
  let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
  let text_content_set =
    get_accessor_setter(scope.heap(), span_obj, &key_text_content).expect("textContent setter exists");
  let arg_hello = Value::String(scope.alloc_string("hello")?);
  vm.call_without_host(&mut scope, text_content_set, span_val, &[arg_hello])?;

  // div.cloneNode(true)
  let key_clone_node = PropertyKey::from_string(scope.alloc_string("cloneNode")?);
  let clone_node =
    get_data_property_value(scope.heap(), div_obj, &key_clone_node).expect("cloneNode exists");
  let clone_val = vm.call_without_host(&mut scope, clone_node, div_val, &[Value::Bool(true)])?;
  let Value::Object(clone_obj) = clone_val else {
    panic!("expected clone wrapper");
  };

  assert_ne!(clone_val, div_val, "cloneNode must allocate a new wrapper");

  // Detached clone: parentNode === null, isConnected === false.
  let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
  let parent_node_get = get_accessor_getter(scope.heap(), clone_obj, &key_parent_node)
    .expect("parentNode getter exists");
  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, clone_val, &[])?,
    Value::Null
  );

  let key_is_connected = PropertyKey::from_string(scope.alloc_string("isConnected")?);
  let is_connected_get = get_accessor_getter(scope.heap(), clone_obj, &key_is_connected)
    .expect("isConnected getter exists");
  assert_eq!(
    vm.call_without_host(&mut scope, is_connected_get, clone_val, &[])?,
    Value::Bool(false)
  );

  // Reflected attribute is cloned.
  let id_get = get_accessor_getter(scope.heap(), clone_obj, &key_id).expect("id getter exists");
  let id_val = vm.call_without_host(&mut scope, id_get, clone_val, &[])?;
  let Value::String(id_str) = id_val else {
    panic!("expected id string");
  };
  assert_eq!(scope.heap().get_string(id_str)?.to_utf8_lossy(), "src");

  // Deep clone should include children but preserve identity (cloneChild !== span).
  let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
  let first_child_get = get_accessor_getter(scope.heap(), clone_obj, &key_first_child)
    .expect("firstChild getter exists");
  let clone_span = vm.call_without_host(&mut scope, first_child_get, clone_val, &[])?;
  assert_ne!(clone_span, Value::Null);
  assert_ne!(clone_span, span_val);

  let Value::Object(clone_span_obj) = clone_span else {
    panic!("expected cloned span wrapper");
  };

  let key_node_name = PropertyKey::from_string(scope.alloc_string("nodeName")?);
  let node_name_get = get_accessor_getter(scope.heap(), clone_span_obj, &key_node_name)
    .expect("nodeName getter exists");
  let node_name = vm.call_without_host(&mut scope, node_name_get, clone_span, &[])?;
  let Value::String(node_name_str) = node_name else {
    panic!("expected nodeName string");
  };
  assert_eq!(scope.heap().get_string(node_name_str)?.to_utf8_lossy(), "SPAN");

  // Validate nested text node value.
  let clone_text = vm.call_without_host(&mut scope, first_child_get, clone_span, &[])?;
  let Value::Object(clone_text_obj) = clone_text else {
    panic!("expected cloned text wrapper");
  };
  let key_node_value = PropertyKey::from_string(scope.alloc_string("nodeValue")?);
  let node_value_get = get_accessor_getter(scope.heap(), clone_text_obj, &key_node_value)
    .expect("nodeValue getter exists");
  let v = vm.call_without_host(&mut scope, node_value_get, clone_text, &[])?;
  let Value::String(v_str) = v else {
    panic!("expected nodeValue string");
  };
  assert_eq!(scope.heap().get_string(v_str)?.to_utf8_lossy(), "hello");

  // Shallow clone: no children.
  let shallow_val = vm.call_without_host(&mut scope, clone_node, div_val, &[])?;
  let Value::Object(shallow_obj) = shallow_val else {
    panic!("expected shallow clone wrapper");
  };
  let shallow_first_child_get =
    get_accessor_getter(scope.heap(), shallow_obj, &key_first_child).expect("firstChild getter exists");
  let shallow_first = vm.call_without_host(&mut scope, shallow_first_child_get, shallow_val, &[])?;
  assert_eq!(shallow_first, Value::Null);

  // Document.cloneNode(false) returns a detached Document with no children.
  let document_clone = get_data_property_value(scope.heap(), document_obj, &key_clone_node)
    .expect("document.cloneNode exists");
  let doc_shallow = vm.call_without_host(&mut scope, document_clone, document_val, &[])?;
  let Value::Object(_doc_shallow_obj) = doc_shallow else {
    panic!("expected cloned Document wrapper");
  };
  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, doc_shallow, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, is_connected_get, doc_shallow, &[])?,
    Value::Bool(false)
  );
  let doc_shallow_first = vm.call_without_host(&mut scope, first_child_get, doc_shallow, &[])?;
  assert_eq!(doc_shallow_first, Value::Null);

  // Document.cloneNode(true) deep clones the document subtree.
  let doc_deep = vm.call_without_host(&mut scope, document_clone, document_val, &[Value::Bool(true)])?;
  let Value::Object(doc_deep_obj) = doc_deep else {
    panic!("expected cloned Document wrapper");
  };
  assert_ne!(doc_deep, document_val);
  assert_ne!(doc_deep_obj, document_obj);
  assert_eq!(
    vm.call_without_host(&mut scope, parent_node_get, doc_deep, &[])?,
    Value::Null
  );
  assert_eq!(
    vm.call_without_host(&mut scope, is_connected_get, doc_deep, &[])?,
    Value::Bool(false)
  );

  let doc_deep_div = vm.call_without_host(&mut scope, first_child_get, doc_deep, &[])?;
  assert_ne!(doc_deep_div, Value::Null);
  assert_ne!(doc_deep_div, div_val);
  let Value::Object(doc_deep_div_obj) = doc_deep_div else {
    panic!("expected deep-cloned div wrapper");
  };

  let doc_deep_id_get =
    get_accessor_getter(scope.heap(), doc_deep_div_obj, &key_id).expect("id getter exists");
  let doc_deep_id_val = vm.call_without_host(&mut scope, doc_deep_id_get, doc_deep_div, &[])?;
  let Value::String(doc_deep_id_str) = doc_deep_id_val else {
    panic!("expected id string");
  };
  assert_eq!(scope.heap().get_string(doc_deep_id_str)?.to_utf8_lossy(), "src");

  // Mutating the clone must not affect the original.
  let arg_cloned = Value::String(scope.alloc_string("cloned")?);
  vm.call_without_host(&mut scope, id_set, doc_deep_div, &[arg_cloned])?;
  let original_id_val = vm.call_without_host(&mut scope, id_get, div_val, &[])?;
  let Value::String(original_id_str) = original_id_val else {
    panic!("expected id string");
  };
  assert_eq!(scope.heap().get_string(original_id_str)?.to_utf8_lossy(), "src");

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
