use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use fastrender::js::webidl::legacy::VmJsRuntime;
use vm_js::{Value, VmError};
use webidl_js_runtime::JsRuntime as _;

#[derive(Default)]
struct AlertHost {
  last_overload: Option<usize>,
  last_args: Vec<BindingValue<Value>>,
}

impl WebHostBindings<VmJsRuntime> for AlertHost {
  fn call_operation(
    &mut self,
    _rt: &mut VmJsRuntime,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    assert!(
      receiver.is_none(),
      "Window.alert is exposed on the global object and should not receive a JS receiver"
    );
    assert_eq!(interface, "Window");
    assert_eq!(operation, "alert");
    self.last_overload = Some(overload);
    self.last_args = args;
    Ok(BindingValue::Undefined)
  }
}

fn legacy_get(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let key = rt.property_key_from_str(name)?;
  rt.get(obj, key)
}

fn legacy_get_method(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let v = legacy_get(rt, obj, name)?;
  assert!(rt.is_callable(v), "expected {name} to be callable");
  Ok(v)
}

#[test]
fn legacy_window_alert_dispatches_overloads() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = AlertHost::default();
  install_window_bindings(&mut rt, &mut host)?;
  let global =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<AlertHost>>::global_object(
      &mut rt,
    )?;
  let alert = legacy_get_method(&mut rt, global, "alert")?;

  rt.with_host_context(&mut host, |rt| rt.call(alert, global, &[]))?;
  assert_eq!(host.last_overload, Some(0));
  assert!(host.last_args.is_empty());

  let message = rt.alloc_string_value("hello")?;
  rt.with_host_context(&mut host, |rt| rt.call(alert, global, &[message]))?;
  assert_eq!(host.last_overload, Some(1));
  match host.last_args.as_slice() {
    [BindingValue::String(s)] => assert_eq!(s, "hello"),
    other => panic!("expected one string arg, got {other:?}"),
  }

  // Extra arguments are ignored; WebIDL overload resolution should still pick the one-arg overload.
  let msg = rt.alloc_string_value("x")?;
  let extra = rt.alloc_string_value("y")?;
  rt.with_host_context(&mut host, |rt| rt.call(alert, global, &[msg, extra]))?;
  assert_eq!(host.last_overload, Some(1));
  match host.last_args.as_slice() {
    [BindingValue::String(s)] => assert_eq!(s, "x"),
    other => panic!("expected one string arg, got {other:?}"),
  }

  Ok(())
}
