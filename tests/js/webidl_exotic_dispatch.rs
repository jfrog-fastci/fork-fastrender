use fastrender::js::window_realm::{WindowRealm, WindowRealmHost};
use fastrender::js::window_timers::VmJsEventLoopHooks;
use vm_js::{Heap, HeapLimits, HostSlots, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use webidl_vm_js::WebIdlBindingsHost;

#[test]
fn vmjs_host_exotic_get_delegates_to_webidl_bindings_host() -> Result<(), VmError> {
  struct DummyWindowRealmHost;

  impl WindowRealmHost for DummyWindowRealmHost {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> fastrender::Result<(&mut dyn VmHost, &mut WindowRealm)> {
      unreachable!("DummyWindowRealmHost is only used as a type parameter in this test")
    }
  }

  const SENTINEL_SLOTS: HostSlots = HostSlots { a: 1, b: 0xEC71C };
  const SENTINEL_VALUE: Value = Value::Number(123.0);

  struct ExoticGetHost {
    calls: usize,
  }

  impl WebIdlBindingsHost for ExoticGetHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    fn call_constructor(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _interface: &'static str,
      _overload: usize,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    fn exotic_get(
      &mut self,
      scope: &mut Scope<'_>,
      obj: vm_js::GcObject,
      key: vm_js::PropertyKey,
      receiver: Value,
    ) -> Result<Option<Value>, VmError> {
      let _ = (key, receiver);
      if scope.heap().object_host_slots(obj)? == Some(SENTINEL_SLOTS) {
        self.calls += 1;
        return Ok(Some(SENTINEL_VALUE));
      }
      Ok(None)
    }
  }

  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();

  // Ensure the object stays live across key allocations and hook dispatch.
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_host_slots(obj, SENTINEL_SLOTS)?;

  let key_s = scope.alloc_string("sentinel")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);

  // No WebIDL host installed: should behave like an unhandled exotic get (i.e. no result).
  {
    let mut vm_host = ();
    let mut hooks = VmJsEventLoopHooks::<DummyWindowRealmHost>::new(&mut vm_host);
    let out = hooks.host_exotic_get(&mut scope, obj, key, Value::Object(obj))?;
    assert_eq!(out, None);
  }

  // With a WebIDL host installed: delegate to `WebIdlBindingsHost::exotic_get`.
  {
    let mut vm_host = ();
    let mut hooks = VmJsEventLoopHooks::<DummyWindowRealmHost>::new(&mut vm_host);
    let mut webidl_host = ExoticGetHost { calls: 0 };
    hooks.set_webidl_bindings_host(&mut webidl_host);

    let out = hooks.host_exotic_get(&mut scope, obj, key, Value::Object(obj))?;
    assert_eq!(out, Some(SENTINEL_VALUE));
    assert_eq!(webidl_host.calls, 1);
  }

  Ok(())
}
