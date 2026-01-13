use std::any::Any;

use vm_js::{GcObject, Heap, HeapLimits, Job, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions};

use webidl::WebIdlLimits;
use webidl_vm_js::bindings_runtime::BindingsRuntime;
use webidl_vm_js::VmJsHostHooksPayload;

struct HooksWithPayload {
  payload: VmJsHostHooksPayload,
}

impl VmHostHooks for HooksWithPayload {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(&mut self.payload)
  }
}

fn native_get_max_string_code_units(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Mirrors vm-js generated WebIDL bindings behaviour: construct a fresh `BindingsRuntime` inside
  // the native call handler and read its limits without calling `set_limits`.
  let rt = BindingsRuntime::from_scope(vm, scope.reborrow());
  Ok(Value::Number(rt.limits().max_string_code_units as f64))
}

#[test]
fn bindings_runtime_reads_webidl_limits_from_vmjs_host_hooks_payload() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  // Allocate a native function object.
  let func = {
    let call_id = vm.register_native_call(native_get_max_string_code_units)?;
    let mut scope = heap.scope();
    let name = scope.alloc_string("getLimit")?;
    scope.push_root(Value::String(name))?;
    scope.alloc_native_function(call_id, None, name, 0)?
  };
  let _func_root = heap.add_root(Value::Object(func))?;

  // Install a host hooks payload with a very small WebIDL max string limit.
  let mut payload = VmJsHostHooksPayload::default();
  let mut limits = WebIdlLimits::default();
  limits.max_string_code_units = 1;
  payload.set_webidl_limits(limits);

  let mut hooks = HooksWithPayload { payload };

  // Call the native function under a hooks override so `BindingsRuntime::from_scope` can recover
  // the configured limits from `vm.active_host_hooks_ptr()`.
  let out = {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(func))?;
    vm.call_with_host(&mut scope, &mut hooks, Value::Object(func), Value::Undefined, &[])?
  };
  assert_eq!(out, Value::Number(1.0));

  realm.teardown(&mut heap);
  Ok(())
}

