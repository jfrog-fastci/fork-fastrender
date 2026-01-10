use std::collections::BTreeMap;

use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlBindingsRuntime, WebIdlJsRuntime as _};

#[derive(Default)]
struct EventTargetHost {
  last_args: Option<Vec<BindingValue<Value>>>,
}

impl EventTargetHost {
  fn prototype_for(&mut self, rt: &mut VmJsRuntime, name: &str) -> Result<Value, VmError> {
    let global = <VmJsRuntime as WebIdlBindingsRuntime<Self>>::global_object(rt)?;
    let ctor_key: PropertyKey = rt.property_key_from_str(name)?;
    let ctor = rt.get(global, ctor_key)?;
    let proto_key: PropertyKey = rt.property_key_from_str("prototype")?;
    rt.get(ctor, proto_key)
  }
}

impl WebHostBindings<VmJsRuntime> for EventTargetHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsRuntime,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("EventTarget", "constructor") => {
        let obj = rt.alloc_object_value()?;
        let proto = self.prototype_for(rt, "EventTarget")?;
        rt.set_prototype(obj, Some(proto))?;
        Ok(BindingValue::Object(obj))
      }
      ("EventTarget", "addEventListener") => {
        self.last_args = Some(args);
        Ok(BindingValue::Undefined)
      }
      _ => Err(rt.throw_type_error("unimplemented host operation")),
    }
  }
}

fn get(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let key: PropertyKey = rt.property_key_from_str(name)?;
  rt.get(obj, key)
}

fn get_method(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let func = get(rt, obj, name)?;
  if !rt.is_callable(func) {
    return Err(rt.throw_type_error(&format!("{name} is not callable")));
  }
  Ok(func)
}

fn assert_options_dict(
  dict: &BTreeMap<String, BindingValue<Value>>,
  capture: bool,
  once: bool,
) {
  match dict.get("capture") {
    Some(BindingValue::Bool(v)) => assert_eq!(*v, capture, "capture mismatch"),
    Some(other) => panic!("expected capture bool, got {:?}", other),
    None => panic!("missing capture"),
  }
  match dict.get("once") {
    Some(BindingValue::Bool(v)) => assert_eq!(*v, once, "once mismatch"),
    Some(other) => panic!("expected once bool, got {:?}", other),
    None => panic!("missing once"),
  }
  assert!(!dict.contains_key("passive"), "passive should be omitted when unset");
  assert!(!dict.contains_key("signal"), "signal should be omitted when unset");
}

#[test]
fn generated_webidl_bindings_event_target_add_event_listener_options_defaults() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = EventTargetHost::default();

  install_window_bindings(&mut rt, &mut host)?;

  let global =
    <VmJsRuntime as WebIdlBindingsRuntime<EventTargetHost>>::global_object(&mut rt)?;
  let ctor = get_method(&mut rt, global, "EventTarget")?;
  // `EventTarget` is a WebIDL interface object: calling it without `new` is illegal.
  // `webidl_js_runtime::VmJsRuntime` does not model `[[Construct]]`, so create a wrapper object
  // manually.
  let proto = get(&mut rt, ctor, "prototype")?;
  let target = rt.alloc_object_value()?;
  rt.set_prototype(target, Some(proto))?;
  let target_root = rt.heap_mut().add_root(target)?;

  let add_event_listener = get_method(&mut rt, proto, "addEventListener")?;

  let ty = rt.alloc_string_value("x")?;
  let listener = rt.alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))?;

  // addEventListener("x", fn, {capture:true})
  let options = rt.alloc_object_value()?;
  let options_root = rt.heap_mut().add_root(options)?;
  let capture_key: PropertyKey = rt.property_key_from_str("capture")?;
  rt.define_data_property(options, capture_key, Value::Bool(true), true)?;

  rt.with_host_context(&mut host, |rt| {
    rt.call(add_event_listener, target, &[ty, listener, options])
  })?;

  let args = host.last_args.take().expect("host call recorded");
  assert_eq!(args.len(), 3);
  match &args[0] {
    BindingValue::String(s) => assert_eq!(s, "x", "type argument mismatch"),
    other => panic!("expected first argument to be a string, got {:?}", other),
  }
  let BindingValue::Dictionary(map) = &args[2] else {
    panic!("expected options dictionary, got {:?}", args[2]);
  };
  assert_options_dict(map, true, false);

  // addEventListener("x", fn) -> defaults should be materialized (capture=false, once=false).
  rt.with_host_context(&mut host, |rt| {
    rt.call(add_event_listener, target, &[ty, listener])
  })?;
  let args = host.last_args.take().expect("host call recorded (default options)");
  let BindingValue::Dictionary(map) = &args[2] else {
    panic!("expected default options dictionary, got {:?}", args[2]);
  };
  assert_options_dict(map, false, false);

  rt.heap_mut().remove_root(options_root);
  rt.heap_mut().remove_root(target_root);

  Ok(())
}
