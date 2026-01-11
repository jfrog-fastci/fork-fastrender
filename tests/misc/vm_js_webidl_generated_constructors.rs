use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use fastrender::js::webidl::{InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits};
use fastrender::js::webidl::WebIdlBindingsRuntime;
use vm_js::{Heap, HeapLimits, JsRuntime as VmJsScriptRuntime, Value, Vm, VmError, VmOptions};

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
    let ctor = rt.get(self, global, ctor_key)?;
    let proto_key = rt.property_key("prototype")?;
    rt.get(self, ctor, proto_key)
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
      _ => Err(rt.throw_type_error("unimplemented host operation")),
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
  let typeof_sp = runtime.exec_script_with_host(&mut host, r#"typeof URLSearchParams === "function""#)?;
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
