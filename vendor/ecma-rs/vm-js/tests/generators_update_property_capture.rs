use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_update_computed_member_evaluates_base_key_and_old_value_once_across_yield_postfix() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var baseCount = 0;
        var keyCount = 0;
        var getCount = 0;
        var setCount = 0;
        var stored = 0;

        var o1 = {
          get a() { getCount++; return 1; },
          set a(v) { setCount++; stored = v; },
        };

        var o2 = { a: 100 };
        function getO() { baseCount++; return o1; }

        function* g() { return getO()[(keyCount++, yield 0)]++; }
        var it = g();
        var r1 = it.next();
        var ok1 =
          r1.value === 0 && r1.done === false &&
          // Base + key are evaluated before the yield, but the old value is not read until after
          // resumption (because we don't have the computed key yet).
          baseCount === 1 && keyCount === 1 &&
          getCount === 0 && setCount === 0;

        // Rebind the base producer; the update expression must not re-evaluate it after resuming.
        getO = function () { baseCount++; return o2; };

        var r2 = it.next("a");
        var ok2 =
          r2.value === 1 && r2.done === true &&
          stored === 2 &&
          // No re-evaluation after resumption.
          baseCount === 1 && keyCount === 1 &&
          // Getter and setter each invoked exactly once.
          getCount === 1 && setCount === 1 &&
          // And nothing was written to the rebound object.
          o2.a === 100;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_update_computed_member_evaluates_base_key_and_old_value_once_across_yield_prefix() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var baseCount = 0;
        var keyCount = 0;
        var getCount = 0;
        var setCount = 0;
        var stored = 0;

        var o1 = {
          get a() { getCount++; return 1; },
          set a(v) { setCount++; stored = v; },
        };

        var o2 = { a: 100 };
        function getO() { baseCount++; return o1; }

        function* g() { return ++getO()[(keyCount++, yield 0)]; }
        var it = g();
        var r1 = it.next();
        var ok1 =
          r1.value === 0 && r1.done === false &&
          baseCount === 1 && keyCount === 1 &&
          getCount === 0 && setCount === 0;

        // Rebind the base producer; the update expression must not re-evaluate it after resuming.
        getO = function () { baseCount++; return o2; };

        var r2 = it.next("a");
        var ok2 =
          r2.value === 2 && r2.done === true &&
          stored === 2 &&
          baseCount === 1 && keyCount === 1 &&
          getCount === 1 && setCount === 1 &&
          o2.a === 100;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

