use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn logical_assignment_short_circuit_rhs_not_evaluated() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          let called = 0;
          function sideEffect() { called++; }

          let x = false;
          x &&= (sideEffect(), true);
          if (called !== 0) return false;

          x = true;
          x ||= (sideEffect(), false);
          if (called !== 0) return false;

          x = 0;
          x ??= (sideEffect(), 1);
          if (called !== 0) return false;

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn logical_assignment_when_assigned_returns_rhs_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          let x = true;
          let v = (x &&= 10);
          if (x !== 10 || v !== 10) return false;

          x = false;
          v = (x ||= 11);
          if (x !== 11 || v !== 11) return false;

          x = null;
          v = (x ??= 12);
          if (x !== 12 || v !== 12) return false;

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn logical_assignment_member_reference_getter_setter_semantics() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          // `&&=`
          {
            let getCount = 0;
            let setCount = 0;
            let rhsCount = 0;
            let stored = 0;

            const o = {
              get f() { getCount++; return stored; },
              set f(v) { setCount++; stored = v; },
            };

            o.f &&= (rhsCount++, 1);
            if (getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = 1;
            o.f &&= (rhsCount++, 2);
            if (getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 2) return false;
          }

          // `||=`
          {
            let getCount = 0;
            let setCount = 0;
            let rhsCount = 0;
            let stored = 1;

            const o = {
              get f() { getCount++; return stored; },
              set f(v) { setCount++; stored = v; },
            };

            o.f ||= (rhsCount++, 2);
            if (getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = 0;
            o.f ||= (rhsCount++, 3);
            if (getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 3) return false;
          }

          // `??=`
          {
            let getCount = 0;
            let setCount = 0;
            let rhsCount = 0;
            let stored = 0;

            const o = {
              get f() { getCount++; return stored; },
              set f(v) { setCount++; stored = v; },
            };

            o.f ??= (rhsCount++, 1);
            if (getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = null;
            o.f ??= (rhsCount++, 2);
            if (getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 2) return false;
          }

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn logical_assignment_anonymous_function_name_inference() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          let o = {};
          o.f ??= function() {};
          if (o.f.name !== "f") return false;

          o = { f: true };
          o.f &&= function() {};
          if (o.f.name !== "f") return false;

          o = { f: 0 };
          o.f ||= function() {};
          if (o.f.name !== "f") return false;

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

