use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use fastrender::js::webidl::legacy::VmJsRuntime;
use vm_js::{PropertyKey, PropertyKind, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, WebIdlJsRuntime as _};

#[derive(Default)]
struct DummyHost;

impl WebHostBindings<VmJsRuntime> for DummyHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsRuntime,
    _receiver: Option<Value>,
    _interface: &'static str,
    _operation: &'static str,
    _overload: usize,
    _args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    Err(rt.throw_type_error("unexpected host call while inspecting bindings descriptors"))
  }
}

fn string_value_to_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string value, got {v:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn webidl_generated_bindings_install_property_descriptors_and_function_metadata() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = DummyHost::default();

  install_window_bindings(&mut rt, &mut host)?;

  let global =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<DummyHost>>::global_object(&mut rt)?;
  let Value::Object(global_obj) = global else {
    return Err(rt.throw_type_error("expected global_object to be an object"));
  };

  // global.URLSearchParams
  let ctor_key: PropertyKey = rt.property_key_from_str("URLSearchParams")?;
  let ctor_desc = rt
    .heap()
    .object_get_own_property(global_obj, &ctor_key)?
    .expect("missing global.URLSearchParams");
  assert!(
    !ctor_desc.enumerable,
    "expected global.URLSearchParams to be non-enumerable"
  );
  assert!(
    ctor_desc.configurable,
    "expected global.URLSearchParams to be configurable"
  );
  let (ctor_obj, ctor_value) = match ctor_desc.kind {
    PropertyKind::Data {
      value: Value::Object(obj),
      writable: true,
    } => (obj, Value::Object(obj)),
    other => panic!("unexpected global.URLSearchParams descriptor: {other:?}"),
  };

  // URLSearchParams.prototype
  let proto_key: PropertyKey = rt.property_key_from_str("prototype")?;
  let proto_desc = rt
    .heap()
    .object_get_own_property(ctor_obj, &proto_key)?
    .expect("missing URLSearchParams.prototype");
  assert!(
    !proto_desc.enumerable,
    "expected URLSearchParams.prototype to be non-enumerable"
  );
  assert!(
    !proto_desc.configurable,
    "expected URLSearchParams.prototype to be non-configurable"
  );
  let proto_obj = match proto_desc.kind {
    PropertyKind::Data {
      value: Value::Object(obj),
      writable: false,
    } => obj,
    other => panic!("unexpected URLSearchParams.prototype descriptor: {other:?}"),
  };

  // URLSearchParams.prototype.constructor
  let ctor_prop_key: PropertyKey = rt.property_key_from_str("constructor")?;
  let ctor_prop_desc = rt
    .heap()
    .object_get_own_property(proto_obj, &ctor_prop_key)?
    .expect("missing URLSearchParams.prototype.constructor");
  assert!(
    !ctor_prop_desc.enumerable,
    "expected URLSearchParams.prototype.constructor to be non-enumerable"
  );
  assert!(
    !ctor_prop_desc.configurable,
    "expected URLSearchParams.prototype.constructor to be non-configurable"
  );
  match ctor_prop_desc.kind {
    PropertyKind::Data {
      value,
      writable: false,
    } => assert_eq!(
      value, ctor_value,
      "expected URLSearchParams.prototype.constructor to point at the URLSearchParams constructor"
    ),
    other => panic!("unexpected URLSearchParams.prototype.constructor descriptor: {other:?}"),
  };

  // URLSearchParams.prototype.append
  let append_key: PropertyKey = rt.property_key_from_str("append")?;
  let append_desc = rt
    .heap()
    .object_get_own_property(proto_obj, &append_key)?
    .expect("missing URLSearchParams.prototype.append");
  assert!(
    !append_desc.enumerable,
    "expected URLSearchParams.prototype.append to be non-enumerable"
  );
  assert!(
    append_desc.configurable,
    "expected URLSearchParams.prototype.append to be configurable"
  );
  let append_func_obj = match append_desc.kind {
    PropertyKind::Data {
      value: Value::Object(obj),
      writable: true,
    } => obj,
    other => panic!("unexpected URLSearchParams.prototype.append descriptor: {other:?}"),
  };

  // Function metadata: length + name.
  let length_key: PropertyKey = rt.property_key_from_str("length")?;
  let length = rt.get(Value::Object(append_func_obj), length_key)?;
  assert_eq!(length, Value::Number(2.0), "append.length");

  let name_key: PropertyKey = rt.property_key_from_str("name")?;
  let name = rt.get(Value::Object(append_func_obj), name_key)?;
  assert_eq!(
    string_value_to_utf8_lossy(&rt, name),
    "append",
    "append.name"
  );

  Ok(())
}
