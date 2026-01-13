use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests intentionally create multiple Promises and await points to exercise compound and
  // logical assignment operators. Keep the heap limit large enough to avoid spurious OOM failures
  // as builtin coverage grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_await_compound_assignment_minus_equals() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x = 5;
        x -= await Promise.resolve(2);
        return x;
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "3");
  Ok(())
}

#[test]
fn async_await_compound_assignment_mul_div_mod_equals_number_and_bigint() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let a = 6;
        a *= await Promise.resolve(7);

        let b = 8;
        b /= await Promise.resolve(2);

        let c = 9;
        c %= await Promise.resolve(4);

        let d = 6n;
        d *= await Promise.resolve(7n);

        let e = 9n;
        e /= await Promise.resolve(2n);

        let f = 9n;
        f %= await Promise.resolve(2n);

        return [
          a, typeof a,
          b, typeof b,
          c, typeof c,
          d, typeof d,
          e, typeof e,
          f, typeof f,
        ].join("|");
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, value),
    "42|number|4|number|1|number|42|bigint|4|bigint|1|bigint"
  );
  Ok(())
}

#[test]
fn async_await_shift_and_bitwise_assignment_ops_number_and_bigint() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let a = 1;
        a <<= await Promise.resolve(2);

        let b = 8;
        b >>= await Promise.resolve(1);

        let c = -1;
        c >>>= await Promise.resolve(1);

        let d = 1;
        d |= await Promise.resolve(2);

        let e = 3;
        e &= await Promise.resolve(1);

        let f = 3;
        f ^= await Promise.resolve(1);

        let g = 1n;
        g <<= await Promise.resolve(2n);

        let h = 8n;
        h >>= await Promise.resolve(1n);

        let i = 1n;
        i |= await Promise.resolve(2n);

        let j = 3n;
        j &= await Promise.resolve(1n);

        let k = 3n;
        k ^= await Promise.resolve(1n);

        let typeErr = "";
        try {
          let bad = 1n;
          bad >>>= await Promise.resolve(1n);
          typeErr = "no error";
        } catch (e) {
          typeErr = e && e.name;
        }

        return [
          a, typeof a,
          b, typeof b,
          c, typeof c,
          d, typeof d,
          e, typeof e,
          f, typeof f,
          g, typeof g,
          h, typeof h,
          i, typeof i,
          j, typeof j,
          k, typeof k,
          typeErr,
        ].join("|");
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, value),
    "4|number|4|number|2147483647|number|3|number|1|number|2|number|4|bigint|4|bigint|3|bigint|1|bigint|2|bigint|TypeError"
  );
  Ok(())
}

#[test]
fn async_await_logical_and_assignment_evaluates_rhs_when_truthy() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x = 1;
        x &&= await Promise.resolve(2);
        return x;
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "2");
  Ok(())
}

#[test]
fn async_await_logical_and_assignment_short_circuits_without_awaiting_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var hit = "";
      async function f() {
        let x = 0;
        x &&= (hit = "rhs", await Promise.resolve(1));
        return String(x) + "|" + hit;
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "0|");
  Ok(())
}

#[test]
fn async_await_logical_or_assignment_evaluates_rhs_when_falsy() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x = 0;
        x ||= await Promise.resolve(2);
        return x;
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "2");
  Ok(())
}

#[test]
fn async_await_logical_or_assignment_short_circuits_without_awaiting_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var hit = "";
      async function f() {
        let x = 1;
        x ||= (hit = "rhs", await Promise.resolve(2));
        return String(x) + "|" + hit;
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "1|");
  Ok(())
}

#[test]
fn async_await_nullish_assignment_evaluates_rhs_when_nullish() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let x;
        x ??= await Promise.resolve(2);
        return x;
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "2");
  Ok(())
}

#[test]
fn async_await_nullish_assignment_short_circuits_without_awaiting_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var hit = "";
      async function f() {
        let x = 0;
        x ??= (hit = "rhs", await Promise.resolve(2));
        return String(x) + "|" + hit;
      }
      f().then(v => out = v);
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "0|");
  Ok(())
}

#[test]
fn async_await_compound_assignment_getvalue_happens_before_awaiting_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      var log = "";
      var obj = {
        get x() { log += "get,"; return 5; },
        set x(v) { log += "set:" + v + ","; },
      };

      async function f() {
        obj.x -= (log += "rhs-pre,", await Promise.resolve(2), log += "rhs-post,", 2);
        return log;
      }

      f().then(v => out = v);
      log
    "#,
  )?;
  // `obj.x` must be GetValue'd (triggering the getter) before we reach the `await` in the RHS.
  assert_eq!(value_to_string(&rt, value), "get,rhs-pre,");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "get,rhs-pre,rhs-post,set:3,");
  Ok(())
}
