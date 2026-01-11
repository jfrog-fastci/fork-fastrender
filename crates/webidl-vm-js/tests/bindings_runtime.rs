use std::any::Any;
use std::collections::BTreeMap;
 
use vm_js::{
  GcObject, Heap, HeapLimits, Job, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
 
use webidl_vm_js::bindings_runtime::{
  BindingValue, BindingsHost, BindingsRuntime, DataPropertyAttributes, WebHostBindingsVm,
};
use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost, WebIdlBindingsHostSlot};
use webidl_vm_js::bindings_runtime::{to_int32_f64, to_uint32_f64};
 
struct HooksWithBindingsHost {
  slot: WebIdlBindingsHostSlot,
}
 
impl HooksWithBindingsHost {
  fn new(host: &mut dyn WebIdlBindingsHost) -> Self {
    Self {
      slot: WebIdlBindingsHostSlot::new(host),
    }
  }
}
  
impl VmHostHooks for HooksWithBindingsHost {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(&mut self.slot)
  }
}

#[derive(Default)]
struct TestHost {
  last_receiver: Option<Value>,
  last_interface: Option<&'static str>,
  last_member: Option<&'static str>,
  last_overload: Option<usize>,
  last_args: Vec<BindingValue>,
}

impl WebHostBindingsVm for TestHost {
  fn call_operation(
    &mut self,
    receiver: Option<Value>,
    interface: &'static str,
    member: &'static str,
    overload: usize,
    args: Vec<BindingValue>,
  ) -> Result<BindingValue, VmError> {
    self.last_receiver = receiver;
    self.last_interface = Some(interface);
    self.last_member = Some(member);
    self.last_overload = Some(overload);
    self.last_args = args;

    // Return a sequence and a dictionary nested inside it to exercise conversion.
    let mut dict = BTreeMap::new();
    dict.insert("k".to_string(), BindingValue::Number(2.0));
    Ok(BindingValue::Sequence(vec![
      BindingValue::Number(1.0),
      BindingValue::Dictionary(dict),
      BindingValue::RustString("s".to_string()),
    ]))
  }

  fn call_constructor(
    &mut self,
    _interface: &'static str,
    _overload: usize,
    _args: Vec<BindingValue>,
  ) -> Result<BindingValue, VmError> {
    Err(VmError::Unimplemented("constructor dispatch not used in this test"))
  }
}

fn generated_like_call_handler(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_from_hooks(hooks)?;
  host.call_operation(vm, scope, Some(this), "TestInterface", "testOperation", 0, args)
}

#[test]
fn can_install_and_call_generated_like_operation() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let global = realm.global_object();

  // Install `global.testOperation = <native fn>`.
  let test_fn: GcObject = {
    let mut rt = BindingsRuntime::new(&mut vm, &mut heap);
    let fn_obj = rt.alloc_native_function(generated_like_call_handler, None, "testOperation", 2)?;
    rt.define_data_property_str(
      global,
      "testOperation",
      Value::Object(fn_obj),
      DataPropertyAttributes::new(true, false, true),
    )?;
    fn_obj
  };

  // Call it.
  let mut host_impl = TestHost::default();
  let mut bindings_host = BindingsHost::new(&mut host_impl);
  let mut hooks = HooksWithBindingsHost::new(&mut bindings_host);

  let mut scope = heap.scope();
  scope.push_root(Value::Object(global))?;
  scope.push_root(Value::Object(test_fn))?;

  // Ensure the binding is actually installed on the realm global.
  let key_s = scope.alloc_string("testOperation")?;
  scope.push_root(Value::String(key_s))?;
  let key = vm_js::PropertyKey::from_string(key_s);
  let callee = vm.get(&mut scope, global, key)?;
  assert_eq!(callee, Value::Object(test_fn));
  scope.push_root(callee)?;

  let this_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(this_obj))?;
  let this = Value::Object(this_obj);

  let arg0 = Value::Number(123.0);
  let arg1_s = scope.alloc_string("arg")?;
  scope.push_root(Value::String(arg1_s))?;
  let arg1 = Value::String(arg1_s);

  let out = vm.call_with_host(&mut scope, &mut hooks, callee, this, &[arg0, arg1])?;
 
  // Drop the host wrapper before inspecting `host_impl`.
  drop(bindings_host);

  assert_eq!(host_impl.last_receiver, Some(this));
  assert_eq!(host_impl.last_interface, Some("TestInterface"));
  assert_eq!(host_impl.last_member, Some("testOperation"));
  assert_eq!(host_impl.last_overload, Some(0));
  assert_eq!(
    host_impl.last_args,
    vec![BindingValue::Number(123.0), BindingValue::String(arg1_s)]
  );

  // Verify the returned BindingValue converted back to JS correctly.
  scope.push_root(out)?;
  let intr = realm.intrinsics();

  let Value::Object(arr) = out else {
    return Err(VmError::TypeError("expected array object return value"));
  };
  assert_eq!(scope.object_get_prototype(arr)?, Some(intr.array_prototype()));

  // length === 3
  let len_key = vm_js::PropertyKey::from_string(scope.alloc_string("length")?);
  scope.push_root(Value::String(match len_key {
    vm_js::PropertyKey::String(s) => s,
    _ => unreachable!(),
  }))?;
  let len = scope
    .heap()
    .object_get_own_data_property_value(arr, &len_key)?
    .unwrap_or(Value::Undefined);
  assert_eq!(len, Value::Number(3.0));

  // arr[0] === 1
  let key0 = vm_js::PropertyKey::from_string(scope.alloc_string("0")?);
  let v0 = scope
    .heap()
    .object_get_own_data_property_value(arr, &key0)?
    .unwrap_or(Value::Undefined);
  assert_eq!(v0, Value::Number(1.0));

  // arr[1] is a dictionary object { k: 2 }
  let key1 = vm_js::PropertyKey::from_string(scope.alloc_string("1")?);
  let v1 = scope
    .heap()
    .object_get_own_data_property_value(arr, &key1)?
    .unwrap_or(Value::Undefined);
  let Value::Object(dict_obj) = v1 else {
    return Err(VmError::TypeError("expected object for arr[1]"));
  };
  assert_eq!(
    scope.object_get_prototype(dict_obj)?,
    Some(intr.object_prototype())
  );
  let k_key = vm_js::PropertyKey::from_string(scope.alloc_string("k")?);
  let k_val = scope
    .heap()
    .object_get_own_data_property_value(dict_obj, &k_key)?
    .unwrap_or(Value::Undefined);
  assert_eq!(k_val, Value::Number(2.0));

  // arr[2] === "s"
  let key2 = vm_js::PropertyKey::from_string(scope.alloc_string("2")?);
  let v2 = scope
    .heap()
    .object_get_own_data_property_value(arr, &key2)?
    .unwrap_or(Value::Undefined);
  let Value::String(s_handle) = v2 else {
    return Err(VmError::TypeError("expected string for arr[2]"));
  };
  assert_eq!(scope.heap().get_string(s_handle)?.to_utf8_lossy(), "s");

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn to_int32_and_uint32_helpers_match_webidl_convert_to_int() {
  let attrs = webidl::IntegerConversionAttrs::default();

  let samples: &[f64] = &[
    0.0,
    -0.0,
    1.0,
    -1.0,
    2.0,
    -2.0,
    2147483647.0,
    2147483648.0,
    4294967295.0,
    4294967296.0,
    9007199254740991.0,  // 2^53 - 1 (max safe integer)
    -9007199254740991.0, // -(2^53 - 1)
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
  ];

  for &n in samples {
    let expected_i32 = webidl::convert_to_int(n, 32, true, attrs).unwrap_or(0) as i32;
    assert_eq!(
      to_int32_f64(n),
      expected_i32,
      "to_int32_f64({n:?}) must match webidl::convert_to_int"
    );

    let expected_u32 = webidl::convert_to_int(n, 32, false, attrs).unwrap_or(0) as u32;
    assert_eq!(
      to_uint32_f64(n),
      expected_u32,
      "to_uint32_f64({n:?}) must match webidl::convert_to_int"
    );
  }

  // Deterministically sample a set of f64 bit patterns (skipping NaN payloads) to prevent drift.
  let mut x: u64 = 0x1234_5678_9abc_def0;
  for _ in 0..10_000 {
    // xorshift64*
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x = x.wrapping_mul(0x2545_f491_4f6c_dd1d);

    let n = f64::from_bits(x);
    if n.is_nan() {
      continue;
    }

    let expected_i32 = webidl::convert_to_int(n, 32, true, attrs).unwrap_or(0) as i32;
    assert_eq!(to_int32_f64(n), expected_i32, "random bits {x:#x}");

    let expected_u32 = webidl::convert_to_int(n, 32, false, attrs).unwrap_or(0) as u32;
    assert_eq!(to_uint32_f64(n), expected_u32, "random bits {x:#x}");
  }
}
