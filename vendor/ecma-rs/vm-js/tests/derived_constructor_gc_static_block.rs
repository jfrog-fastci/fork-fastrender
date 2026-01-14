use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn host_gc(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  scope.heap_mut().collect_garbage();
  Ok(Value::Undefined)
}

#[test]
fn derived_constructor_state_is_rooted_across_nested_class_static_block_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.register_global_native_function("__gc", host_gc, 0)?;

  // This exercises an important GC invariant:
  // - Derived constructors allocate a `DerivedConstructorState` cell to represent the uninitialized
  //   `this` binding before `super()` returns.
  // - Nested class static initialization may run while a derived constructor is still pre-`super()`.
  // - The derived-state cell must remain rooted across any allocations/GC triggered by static
  //   initialization.
  //
  // We trigger a GC from inside a nested `static {}` block and then access `this` (which should
  // throw a ReferenceError, not crash or report an internal InvalidHandle).
  let value = rt.exec_script(
    r#"
      let ran = false;
      let ok = false;

      class Base {}
      class Derived extends Base {
        constructor() {
          class Inner {
            static {
              ran = true;
              __gc();
            }
          }

          try { this; } catch (e) { ok = e instanceof ReferenceError; }
          super();
        }
      }

      new Derived();
      ran && ok;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

