use vm_js::{Heap, HeapLimits, Job, Realm, RealmId, Value, Vm, VmError, VmHostHooks, VmOptions};

use webidl::WebIdlLimits;
use webidl_vm_js::bindings_runtime::BindingsRuntime;

#[derive(Debug, Default)]
struct NoopHooks;

impl VmHostHooks for NoopHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}

#[test]
fn bindings_scope_to_string_enforces_max_string_code_units() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let mut rt = BindingsRuntime::new(&mut vm, &mut heap);

  let mut limits = WebIdlLimits::default();
  limits.max_string_code_units = 1;
  rt.set_limits(limits);

  let s = rt.scope.alloc_string("ab")?;
  rt.scope.push_root(Value::String(s))?;

  let mut dummy_vm_host = ();
  let mut hooks = NoopHooks::default();

  let err = rt
    .scope
    .to_string(&mut *rt.vm, &mut dummy_vm_host, &mut hooks, Value::String(s))
    .expect_err("expected string length to exceed max_string_code_units");

  let thrown = match err {
    VmError::Throw(v) => v,
    VmError::ThrowWithStack { value, .. } => value,
    other => panic!("expected thrown RangeError object, got {other:?}"),
  };

  let Value::Object(err_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  // Root the thrown value across string allocations for property key creation.
  rt.scope.push_root(thrown)?;

  assert_eq!(
    rt.scope.object_get_prototype(err_obj)?,
    Some(realm.intrinsics().range_error_prototype())
  );

  let msg_key_s = rt.scope.alloc_string("message")?;
  rt.scope.push_root(Value::String(msg_key_s))?;
  let msg_key = vm_js::PropertyKey::from_string(msg_key_s);

  let message = rt.scope.heap().get(err_obj, &msg_key)?;
  let Value::String(message_s) = message else {
    panic!("expected error.message to be a string, got {message:?}");
  };
  assert_eq!(
    rt.scope.heap().get_string(message_s)?.to_utf8_lossy(),
    "string exceeds maximum length"
  );

  drop(rt);
  realm.teardown(&mut heap);
  Ok(())
}
