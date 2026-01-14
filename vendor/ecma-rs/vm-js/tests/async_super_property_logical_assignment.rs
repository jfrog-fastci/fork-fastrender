use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promises/async-await can allocate; give the tests some headroom.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_super_property_logical_assignment_across_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      var rhs_side = 0;

      class B {
        get x() { return this._x; }
        set x(v) { this._x = v; }
      }

      class D extends B {
        constructor(v) { super(); this._x = v; }

        async and_assign() {
          return super.x &&= await (rhs_side++, Promise.resolve(2));
        }
        async or_assign() {
          return super.x ||= await (rhs_side++, Promise.resolve(3));
        }
        async nullish_assign() {
          return super.x ??= await (rhs_side++, Promise.resolve(4));
        }

        async computed_and_assign() {
          return super[await Promise.resolve("x")] &&= await (rhs_side++, Promise.resolve(5));
        }
      }

      async function main() {
        let parts = [];

        // `&&=`: falsy short-circuits RHS.
        rhs_side = 0;
        let d0 = new D(0);
        parts.push(await d0.and_assign(), rhs_side, d0._x);

        // `&&=`: truthy evaluates RHS and assigns.
        rhs_side = 0;
        let d1 = new D(1);
        parts.push(await d1.and_assign(), rhs_side, d1._x);

        // `||=`: falsy evaluates RHS and assigns.
        rhs_side = 0;
        let d2 = new D(0);
        parts.push(await d2.or_assign(), rhs_side, d2._x);

        // `||=`: truthy short-circuits RHS.
        rhs_side = 0;
        let d3 = new D(1);
        parts.push(await d3.or_assign(), rhs_side, d3._x);

        // `??=`: nullish evaluates RHS and assigns.
        rhs_side = 0;
        let d4 = new D(null);
        parts.push(await d4.nullish_assign(), rhs_side, d4._x);

        // `??=`: non-nullish short-circuits RHS.
        rhs_side = 0;
        let d5 = new D(0);
        parts.push(await d5.nullish_assign(), rhs_side, d5._x);

        // Computed super key with `await` + `&&=` short-circuit semantics.
        rhs_side = 0;
        let d6 = new D(0);
        parts.push(await d6.computed_and_assign(), rhs_side, d6._x);

        rhs_side = 0;
        let d7 = new D(1);
        parts.push(await d7.computed_and_assign(), rhs_side, d7._x);

        return parts.join(",");
      }

      main().then(v => out = String(v));
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "0,0,0,2,1,2,3,1,3,1,0,1,4,1,4,0,0,0,0,0,0,5,1,5"
  );

  Ok(())
}

