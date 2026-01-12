use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn proxy_wrapped_iterators_throw_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function throwsTypeError(f) {
        try { f(); return false; } catch (e) { return e instanceof TypeError; }
      }

      var ok = true;
      ok = ok && throwsTypeError(() => new Proxy([1].values(), {}).next());
      ok = ok && throwsTypeError(() => new Proxy(new Map([[1, 2]]).entries(), {}).next());
      ok = ok && throwsTypeError(() => new Proxy(new Set([1]).values(), {}).next());
      ok = ok && throwsTypeError(() => new Proxy("ab"[Symbol.iterator](), {}).next());
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn iterator_internal_slots_are_not_inherited() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function throwsTypeError(f) {
        try { f(); return false; } catch (e) { return e instanceof TypeError; }
      }

      var ok = true;

      var a = [1].values();
      ok = ok && throwsTypeError(() => Object.create(a).next());

      var m = new Map([[1, 2]]).entries();
      ok = ok && throwsTypeError(() => Object.create(m).next());

      var s = new Set([1]).values();
      ok = ok && throwsTypeError(() => Object.create(s).next());

      ok = ok && throwsTypeError(() => Object.create("ab"[Symbol.iterator]()).next());

      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

