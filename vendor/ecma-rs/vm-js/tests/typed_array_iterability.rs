use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

fn detach_array_buffer(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = arg0 else {
    return Err(VmError::TypeError(
      "detachArrayBuffer expects an ArrayBuffer",
    ));
  };
  scope.heap_mut().detach_array_buffer(obj)?;
  Ok(Value::Undefined)
}

#[test]
fn typed_array_values_iterability_for_of_spread_and_array_from() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ta = new Uint8Array([10, 20]);

        var sum = 0;
        for (var v of ta) sum += v;

        var spread = [...ta];
        var from = Array.from(ta);

        sum === 30 &&
          spread.length === 2 && spread[0] === 10 && spread[1] === 20 &&
          from.length === 2 && from[0] === 10 && from[1] === 20
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn typed_array_symbol_iterator_is_values() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ta = new Uint8Array([1, 2]);
        ta[Symbol.iterator] === ta.values
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn typed_array_keys_and_entries() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ta = new Uint8Array([10, 20]);
        var k = [...ta.keys()];
        var e = [...ta.entries()];

        k.length === 2 && k[0] === 0 && k[1] === 1 &&
          e.length === 2 &&
          e[0][0] === 0 && e[0][1] === 10 &&
          e[1][0] === 1 && e[1][1] === 20
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn typed_array_iteration_methods_throw_on_detached_buffer() {
  let mut rt = new_runtime();
  rt.register_global_native_function("detachArrayBuffer", detach_array_buffer, 1)
    .expect("register detachArrayBuffer");

  let value = rt
    .exec_script(
      r#"
        var ab = new ArrayBuffer(2);
        var ta = new Uint8Array(ab);
        detachArrayBuffer(ab);

        var okValues = false;
        try { ta.values(); } catch(e) { okValues = e && e.name === 'TypeError'; }

        var okIter = false;
        try { ta[Symbol.iterator](); } catch(e) { okIter = e && e.name === 'TypeError'; }

        var okKeys = false;
        try { ta.keys(); } catch(e) { okKeys = e && e.name === 'TypeError'; }

        var okEntries = false;
        try { ta.entries(); } catch(e) { okEntries = e && e.name === 'TypeError'; }

        okValues && okIter && okKeys && okEntries
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
