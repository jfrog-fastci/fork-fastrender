use std::cell::Cell;
use vm_js::{
  GcObject, Heap, HeapLimits, PropertyKey as VmPropertyKey, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};
use webidl::{
  InterfaceId, JsPropertyKind, JsRuntime, PropertyKey, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};
use webidl_vm_js::VmJsWebIdlCx;

struct NoHooks;

impl WebIdlHooks<Value> for NoHooks {
  fn is_platform_object(&self, _value: Value) -> bool {
    false
  }

  fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
    false
  }
}

fn alloc_key(cx: &mut VmJsWebIdlCx<'_>, s: &str) -> Result<vm_js::GcString, VmError> {
  let units: Vec<u16> = s.encode_utf16().collect();
  cx.alloc_string_from_code_units(&units)
}

thread_local! {
  static OWN_KEYS_CALLS: Cell<u32> = const { Cell::new(0) };
  static GET_CALLS: Cell<u32> = const { Cell::new(0) };
  static GOPD_CALLS: Cell<u32> = const { Cell::new(0) };
  static EXPECTED_RECEIVER: Cell<Option<GcObject>> = const { Cell::new(None) };
  static EXPECTED_KEY: Cell<Option<vm_js::GcString>> = const { Cell::new(None) };
}

fn own_keys_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  OWN_KEYS_CALLS.with(|c| c.set(c.get() + 1));

  // Return ["a"].
  let arr = scope.alloc_array(1)?;
  scope.push_root(Value::Object(arr))?;

  let key_a = scope.alloc_string("a")?;
  scope.push_root(Value::String(key_a))?;

  let idx0 = scope.alloc_string("0")?;
  scope.push_root(Value::String(idx0))?;

  scope.create_data_property_or_throw(arr, VmPropertyKey::from_string(idx0), Value::String(key_a))?;

  // Stress rooting: force a GC while the trap result is only reachable from local roots.
  scope.heap_mut().collect_garbage();

  Ok(Value::Object(arr))
}

fn get_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  GET_CALLS.with(|c| c.set(c.get() + 1));

  assert_eq!(args.len(), 3, "Proxy get trap must receive (target, key, receiver)");

  // `Get(O, P)` uses `receiver = O`, so the trap should observe the Proxy object as the receiver.
  let expected_receiver = EXPECTED_RECEIVER
    .with(|c| c.get())
    .expect("EXPECTED_RECEIVER should be set");
  assert_eq!(args[2], Value::Object(expected_receiver));

  let expected_key = EXPECTED_KEY.with(|c| c.get()).expect("EXPECTED_KEY should be set");
  assert_eq!(args[1], Value::String(expected_key));

  // Stress rooting: ensure `expected_key` is still a valid string across GC.
  scope.heap_mut().collect_garbage();
  assert_eq!(
    scope.heap().get_string(expected_key)?.to_utf8_lossy(),
    "a".to_string()
  );

  Ok(Value::Number(42.0))
}

fn get_own_property_descriptor_trap(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  GOPD_CALLS.with(|c| c.set(c.get() + 1));

  assert_eq!(
    args.len(),
    2,
    "Proxy getOwnPropertyDescriptor trap must receive (target, key)"
  );

  let expected_key = EXPECTED_KEY.with(|c| c.get()).expect("EXPECTED_KEY should be set");
  assert_eq!(args[1], Value::String(expected_key));

  // Return a data descriptor matching the target's property attributes.
  let desc = scope.alloc_object()?;
  scope.push_root(Value::Object(desc))?;

  let enumerable_key = VmPropertyKey::from_string(scope.alloc_string("enumerable")?);
  scope.create_data_property_or_throw(desc, enumerable_key, Value::Bool(true))?;
  let configurable_key = VmPropertyKey::from_string(scope.alloc_string("configurable")?);
  scope.create_data_property_or_throw(desc, configurable_key, Value::Bool(true))?;
  let writable_key = VmPropertyKey::from_string(scope.alloc_string("writable")?);
  scope.create_data_property_or_throw(desc, writable_key, Value::Bool(true))?;
  let value_key = VmPropertyKey::from_string(scope.alloc_string("value")?);
  scope.create_data_property_or_throw(desc, value_key, Value::Number(10.0))?;

  Ok(Value::Object(desc))
}

#[test]
fn proxy_property_ops_respect_traps() -> Result<(), VmError> {
  OWN_KEYS_CALLS.with(|c| c.set(0));
  GET_CALLS.with(|c| c.set(0));
  GOPD_CALLS.with(|c| c.set(0));

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
  let mut vm = Vm::new(VmOptions::default());
  let hooks = NoHooks;
  let limits = WebIdlLimits::default();
  let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

  let own_keys_id = cx.vm.register_native_call(own_keys_trap)?;
  let get_id = cx.vm.register_native_call(get_trap)?;
  let gopd_id = cx.vm.register_native_call(get_own_property_descriptor_trap)?;

  // target: { a: 10 }
  let target = cx.alloc_object()?;
  let key_a = alloc_key(&mut cx, "a")?;
  cx.create_data_property_or_throw(target, PropertyKey::String(key_a), Value::Number(10.0))?;

  // handler: { ownKeys, getOwnPropertyDescriptor, get }
  let handler = cx.scope.alloc_object()?;
  cx.scope.push_root(Value::Object(handler))?;

  let own_keys_name = cx.scope.alloc_string("ownKeys")?;
  let own_keys_fn = cx
    .scope
    .alloc_native_function(own_keys_id, None, own_keys_name, 1)?;
  cx.scope.push_root(Value::Object(own_keys_fn))?;
  let own_keys_prop = VmPropertyKey::from_string(cx.scope.alloc_string("ownKeys")?);
  cx.scope.create_data_property_or_throw(
    handler,
    own_keys_prop,
    Value::Object(own_keys_fn),
  )?;

  let gopd_name = cx.scope.alloc_string("getOwnPropertyDescriptor")?;
  let gopd_fn = cx
    .scope
    .alloc_native_function(gopd_id, None, gopd_name, 2)?;
  cx.scope.push_root(Value::Object(gopd_fn))?;
  let gopd_prop = VmPropertyKey::from_string(cx.scope.alloc_string("getOwnPropertyDescriptor")?);
  cx.scope.create_data_property_or_throw(
    handler,
    gopd_prop,
    Value::Object(gopd_fn),
  )?;

  let get_name = cx.scope.alloc_string("get")?;
  let get_fn = cx
    .scope
    .alloc_native_function(get_id, None, get_name, 3)?;
  cx.scope.push_root(Value::Object(get_fn))?;
  let get_prop = VmPropertyKey::from_string(cx.scope.alloc_string("get")?);
  cx.scope.create_data_property_or_throw(
    handler,
    get_prop,
    Value::Object(get_fn),
  )?;

  let proxy = cx.scope.alloc_proxy(Some(target), Some(handler))?;
  cx.scope.push_root(Value::Object(proxy))?;

  EXPECTED_RECEIVER.with(|c| c.set(Some(proxy)));
  EXPECTED_KEY.with(|c| c.set(Some(key_a)));

  // [[OwnPropertyKeys]] / ownKeys trap.
  let keys = cx.own_property_keys(proxy)?;
  let rendered: Vec<String> = keys
    .into_iter()
    .map(|k| match k {
      PropertyKey::String(s) => cx.scope.heap().get_string(s).unwrap().to_utf8_lossy(),
      PropertyKey::Symbol(sym) => format!("sym{}", cx.scope.heap().get_symbol_id(sym).unwrap()),
    })
    .collect();
  assert_eq!(rendered, vec!["a".to_string()]);
  assert_eq!(OWN_KEYS_CALLS.with(|c| c.get()), 1);

  // [[GetOwnProperty]] / getOwnPropertyDescriptor trap.
  let desc = cx
    .get_own_property(proxy, PropertyKey::String(key_a))?
    .expect("expected descriptor for proxy.a");
  assert!(desc.enumerable);
  match desc.kind {
    JsPropertyKind::Data { value } => assert_eq!(value, Value::Number(10.0)),
    _ => return Err(VmError::TypeError("expected data descriptor")),
  }
  assert_eq!(GOPD_CALLS.with(|c| c.get()), 1);

  // [[Get]] / get trap.
  let got = cx.get(proxy, PropertyKey::String(key_a))?;
  assert_eq!(got, Value::Number(42.0));
  assert_eq!(GET_CALLS.with(|c| c.get()), 1);

  Ok(())
}
