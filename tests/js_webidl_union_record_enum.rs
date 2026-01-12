use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use fastrender::js::webidl::WebIdlBindingsRuntime;
use fastrender::js::webidl::{
  InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits,
};
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
struct RecordingHost {
  add_event_listener_options: Vec<BindingValue<Value>>,
  url_search_params_inits: Vec<BindingValue<Value>>,
}

impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, RecordingHost>> for RecordingHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsWebIdlBindingsCx<'a, RecordingHost>,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      // Constructors initialize the pre-allocated wrapper object; our tests do not need any per-instance
      // state so we can treat them as no-ops.
      ("EventTarget", "constructor") => Ok(BindingValue::Undefined),
      ("URLSearchParams", "constructor") => {
        let init = args.into_iter().next().unwrap_or(BindingValue::Undefined);
        self.url_search_params_inits.push(init);
        Ok(BindingValue::Undefined)
      }
      ("EventTarget", "addEventListener") => {
        let options = args.into_iter().nth(2).unwrap_or(BindingValue::Undefined);
        self.add_event_listener_options.push(options);
        Ok(BindingValue::Undefined)
      }
      _ => Err(rt.throw_type_error("unimplemented host operation")),
    }
  }
}

fn unwrap_union<'a>(value: &'a BindingValue<Value>) -> (&'a str, &'a BindingValue<Value>) {
  let BindingValue::Union { member_type, value } = value else {
    panic!("expected union value, got: {value:?}");
  };
  (member_type.as_str(), value.as_ref())
}

#[test]
fn vm_js_union_record_conversions_reach_host_through_window_bindings() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut runtime = VmJsScriptRuntime::new(vm, heap)?;

  let state = Box::new(VmJsWebIdlBindingsState::<RecordingHost>::new(
    runtime.realm().global_object(),
    WebIdlLimits::default(),
    Box::new(NoHooks),
  ));

  let mut host = RecordingHost::default();

  // Install generated bindings into the realm.
  {
    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
    install_window_bindings(&mut cx, &mut host)?;
  }

  let ok = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        const t = new EventTarget();
        t.addEventListener("x", () => {}, true);
        t.addEventListener("x", () => {}, { capture: true });

        new URLSearchParams("a=b");
        new URLSearchParams([["a","b"]]);
        new URLSearchParams({ a: "b" });
        return true;
      })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));

  // EventTarget.addEventListener(..., options)
  assert_eq!(host.add_event_listener_options.len(), 2);

  let (member_type, inner) = unwrap_union(&host.add_event_listener_options[0]);
  assert_eq!(member_type, "boolean");
  match inner {
    BindingValue::Bool(true) => {}
    other => panic!("expected boolean true, got {other:?}"),
  }

  let (member_type, inner) = unwrap_union(&host.add_event_listener_options[1]);
  assert_eq!(member_type, "AddEventListenerOptions");
  let BindingValue::Dictionary(map) = inner else {
    panic!("expected dictionary for AddEventListenerOptions, got {inner:?}");
  };
  match map.get("capture") {
    Some(BindingValue::Bool(true)) => {}
    other => panic!("expected capture=true, got {other:?}"),
  }
  match map.get("once") {
    Some(BindingValue::Bool(false)) => {}
    other => panic!("expected once=false, got {other:?}"),
  }

  // URLSearchParams constructor init union conversion.
  assert_eq!(host.url_search_params_inits.len(), 3);

  let (member_type, inner) = unwrap_union(&host.url_search_params_inits[0]);
  assert_eq!(member_type, "USVString");
  match inner {
    BindingValue::String(s) if s == "a=b" => {}
    other => panic!("expected string \"a=b\", got {other:?}"),
  }

  let (member_type, inner) = unwrap_union(&host.url_search_params_inits[1]);
  assert_eq!(member_type, "sequence<sequence<USVString>>");
  let BindingValue::Sequence(outer) = inner else {
    panic!("expected outer sequence, got {inner:?}");
  };
  assert_eq!(outer.len(), 1);
  let BindingValue::Sequence(inner) = &outer[0] else {
    panic!("expected inner sequence, got {:?}", outer[0]);
  };
  assert_eq!(inner.len(), 2);
  match (&inner[0], &inner[1]) {
    (BindingValue::String(a), BindingValue::String(b)) if a == "a" && b == "b" => {}
    other => panic!("expected [[\"a\",\"b\"]], got {other:?}"),
  }

  let (member_type, inner) = unwrap_union(&host.url_search_params_inits[2]);
  assert_eq!(member_type, "record<USVString, USVString>");
  let BindingValue::Record(entries) = inner else {
    panic!("expected record, got {inner:?}");
  };
  assert_eq!(entries.len(), 1);
  match &entries[0] {
    (k, BindingValue::String(v)) if k == "a" && v == "b" => {}
    other => panic!("expected record {{a:\"b\"}}, got {other:?}"),
  }

  Ok(())
}

fn test_enum_fn<'a>(
  rt: &mut VmJsWebIdlBindingsCx<'a, ()>,
  host: &mut (),
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let _ = fastrender::js::webidl::conversions::to_enum(rt, host, value, "TestEnum", &["a", "b"])?;
  Ok(Value::Bool(true))
}

#[test]
fn vm_js_enum_conversion_throws_type_error_on_invalid_value() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut runtime = VmJsScriptRuntime::new(vm, heap)?;

  let state = Box::new(VmJsWebIdlBindingsState::<()>::new(
    runtime.realm().global_object(),
    WebIdlLimits::default(),
    Box::new(NoHooks),
  ));

  let mut host = ();

  // Install a single native function that uses the shared enum conversion helper.
  {
    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
    let global = cx.global_object()?;
    let func = cx.create_function("testEnum", 1, test_enum_fn)?;
    cx.define_method(global, "testEnum", func)?;
  }

  let ok = runtime.exec_script_with_host(
    &mut host,
    r#"
      (function () {
        try {
          testEnum("c");
          return false;
        } catch (e) {
          return e instanceof TypeError &&
            e.message === "Value is not a valid member of the `TestEnum` enum";
        }
      })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}
