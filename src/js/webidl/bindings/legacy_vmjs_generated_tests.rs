use super::{install_window_bindings, BindingValue, WebHostBindings};
use crate::js::webidl::legacy::VmJsRuntime;
use crate::js::webidl::{
  InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlBindingsRuntime, WebIdlHooks,
  WebIdlLimits,
};
use vm_js::{
  Heap, HeapLimits, JsRuntime as VmJsScriptRuntime, PropertyKey, PropertyKind, Value, Vm, VmError,
  VmOptions,
};
use webidl_js_runtime::JsRuntime as _;

#[derive(Default)]
struct NoHooks;

impl WebIdlHooks<Value> for NoHooks {
  fn is_platform_object(&self, _value: Value) -> bool {
    false
  }

  fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
    false
  }
}

#[derive(Default)]
struct ConstructorHost;

impl ConstructorHost {
  fn prototype_for<'a>(
    &mut self,
    rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
    name: &str,
  ) -> Result<Value, VmError> {
    let global = rt.global_object()?;
    let ctor_key = rt.property_key(name)?;
    let ctor = WebIdlBindingsRuntime::get(rt, self, global, ctor_key)?;
    let proto_key = rt.property_key("prototype")?;
    WebIdlBindingsRuntime::get(rt, self, ctor, proto_key)
  }
}

impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, ConstructorHost>> for ConstructorHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsWebIdlBindingsCx<'a, ConstructorHost>,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    _args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("URL", "constructor") => {
        let proto = self.prototype_for(rt, "URL")?;
        let obj = rt.create_object()?;
        rt.set_prototype(obj, Some(proto))?;
        Ok(BindingValue::Object(obj))
      }
      ("URLSearchParams", "constructor") => {
        let proto = self.prototype_for(rt, "URLSearchParams")?;
        let obj = rt.create_object()?;
        rt.set_prototype(obj, Some(proto))?;
        Ok(BindingValue::Object(obj))
      }
      _ => Err(WebIdlBindingsRuntime::throw_type_error(
        rt,
        "unimplemented host operation",
      )),
    }
  }
}

#[test]
fn vm_js_webidl_generated_constructors_require_new() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut runtime = VmJsScriptRuntime::new(vm, heap)?;

  let state = Box::new(VmJsWebIdlBindingsState::<ConstructorHost>::new(
    runtime.realm().global_object(),
    WebIdlLimits::default(),
    Box::new(NoHooks),
  ));

  let mut host = ConstructorHost::default();

  {
    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
    install_window_bindings(&mut cx, &mut host)?;
  }

  // URL
  let typeof_url = runtime.exec_script_with_host(&mut host, r#"typeof URL === "function""#)?;
  assert_eq!(typeof_url, Value::Bool(true));

  let url_len = runtime.exec_script_with_host(&mut host, r#"URL.length === 1"#)?;
  assert_eq!(url_len, Value::Bool(true));

  let url_proto = runtime.exec_script_with_host(
    &mut host,
    r#"URL.prototype !== null && typeof URL.prototype === "object""#,
  )?;
  assert_eq!(url_proto, Value::Bool(true));

  let url_call_throws = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        try {
          URL("https://example.com");
          return false;
        } catch (e) {
          return e instanceof TypeError && e.message === "Illegal constructor";
        }
      })()
    "#,
  )?;
  assert_eq!(url_call_throws, Value::Bool(true));

  let url_new_works = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        const u = new URL("https://example.com");
        return typeof u === "object" && u !== null && u instanceof URL;
      })()
    "#,
  )?;
  assert_eq!(url_new_works, Value::Bool(true));

  // URLSearchParams
  let typeof_sp =
    runtime.exec_script_with_host(&mut host, r#"typeof URLSearchParams === "function""#)?;
  assert_eq!(typeof_sp, Value::Bool(true));

  let sp_len = runtime.exec_script_with_host(&mut host, r#"URLSearchParams.length === 0"#)?;
  assert_eq!(sp_len, Value::Bool(true));

  let sp_proto = runtime.exec_script_with_host(
    &mut host,
    r#"URLSearchParams.prototype !== null && typeof URLSearchParams.prototype === "object""#,
  )?;
  assert_eq!(sp_proto, Value::Bool(true));

  let sp_call_throws = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        try {
          URLSearchParams("a=b");
          return false;
        } catch (e) {
          return e instanceof TypeError && e.message === "Illegal constructor";
        }
      })()
    "#,
  )?;
  assert_eq!(sp_call_throws, Value::Bool(true));

  let sp_new_works = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        const p = new URLSearchParams("a=b");
        return typeof p === "object" && p !== null && p instanceof URLSearchParams;
      })()
    "#,
  )?;
  assert_eq!(sp_new_works, Value::Bool(true));

  Ok(())
}

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
    Err(<VmJsRuntime as WebIdlBindingsRuntime<DummyHost>>::throw_type_error(
      rt,
      "unexpected host call while inspecting bindings descriptors",
    ))
  }
}

fn string_value_to_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string value, got {v:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn webidl_generated_bindings_install_property_descriptors_and_function_metadata(
) -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = DummyHost::default();

  install_window_bindings(&mut rt, &mut host)?;

  let global =
    <VmJsRuntime as WebIdlBindingsRuntime<DummyHost>>::global_object(&mut rt)?;
  let Value::Object(global_obj) = global else {
    return Err(<VmJsRuntime as WebIdlBindingsRuntime<DummyHost>>::throw_type_error(
      &mut rt,
      "expected global_object to be an object",
    ));
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
  let length_desc = rt
    .heap()
    .object_get_own_property(append_func_obj, &length_key)?
    .expect("missing append.length");
  assert!(
    !length_desc.enumerable,
    "expected append.length to be non-enumerable"
  );
  let length = webidl_js_runtime::JsRuntime::get(
    &mut rt,
    Value::Object(append_func_obj),
    length_key,
  )?;
  assert_eq!(length, Value::Number(2.0), "append.length");

  let name_key: PropertyKey = rt.property_key_from_str("name")?;
  let name_desc = rt
    .heap()
    .object_get_own_property(append_func_obj, &name_key)?
    .expect("missing append.name");
  assert!(
    !name_desc.enumerable,
    "expected append.name to be non-enumerable"
  );
  let name = webidl_js_runtime::JsRuntime::get(&mut rt, Value::Object(append_func_obj), name_key)?;
  assert_eq!(
    string_value_to_utf8_lossy(&rt, name),
    "append",
    "append.name"
  );

  Ok(())
}
