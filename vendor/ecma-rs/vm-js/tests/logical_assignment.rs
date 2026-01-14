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
          let r = (x &&= (sideEffect(), true));
          if (called !== 0 || x !== false || r !== false) return false;

          x = true;
          r = (x ||= (sideEffect(), false));
          if (called !== 0 || x !== true || r !== true) return false;

          x = 0;
          r = (x ??= (sideEffect(), 1));
          if (called !== 0 || x !== 0 || r !== 0) return false;

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

            let res = (o.f &&= (rhsCount++, 1));
            if (res !== 0 || stored !== 0 || getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = 1;
            res = (o.f &&= (rhsCount++, 2));
            if (res !== 2 || getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 2) return false;
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

            let res = (o.f ||= (rhsCount++, 2));
            if (res !== 1 || stored !== 1 || getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = 0;
            res = (o.f ||= (rhsCount++, 3));
            if (res !== 3 || getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 3) return false;
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

            let res = (o.f ??= (rhsCount++, 1));
            if (res !== 0 || stored !== 0 || getCount !== 1 || setCount !== 0 || rhsCount !== 0) return false;

            stored = null;
            res = (o.f ??= (rhsCount++, 2));
            if (res !== 2 || getCount !== 2 || setCount !== 1 || rhsCount !== 1 || stored !== 2) return false;
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
          // Binding refs.
          let x = 0;
          x ||= function() {};
          if (x.name !== "x") return false;

          let y = 1;
          y &&= function() {};
          if (y.name !== "y") return false;

          let z;
          z ??= function() {};
          if (z.name !== "z") return false;

          let o = {};
          o.f ??= function() {};
          if (o.f.name !== "f") return false;

          o = { f: true };
          o.f &&= function() {};
          if (o.f.name !== "f") return false;

          o = { f: 0 };
          o.f ||= function() {};
          if (o.f.name !== "f") return false;

          // Computed refs.
          const prop = "comp";
          o[prop] ||= function() {};
          if (o[prop].name !== "comp") return false;

          // Private refs: assignment works, but private fields do not participate in name inference.
          class C {
            static #f = 1;
            static test() {
              C.#f &&= function() {};
              return C.#f.name;
            }
          }
          if (C.test() !== "") return false;

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn logical_assignment_computed_member_evaluation_order() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          const log = [];
          let stored = 1;
          const obj = {
            get a() { log.push("get"); return stored; },
            set a(v) { log.push("set:" + v); stored = v; },
          };
          function base() { log.push("base"); return obj; }
          function key() { log.push("key"); return "a"; }
          function rhs() { log.push("rhs"); return 2; }

          // `&&=` short-circuits when the LHS is falsy.
          let res = (base()[key()] &&= rhs());
          if (res !== 2 || stored !== 2 || log.join(",") !== "base,key,get,rhs,set:2") return false;

          log.length = 0;
          stored = 0;
          res = (base()[key()] &&= rhs());
          if (res !== 0 || stored !== 0 || log.join(",") !== "base,key,get") return false;

          // `||=` short-circuits when the LHS is truthy.
          log.length = 0;
          stored = 0;
          res = (base()[key()] ||= rhs());
          if (res !== 2 || stored !== 2 || log.join(",") !== "base,key,get,rhs,set:2") return false;

          log.length = 0;
          stored = 5;
          res = (base()[key()] ||= rhs());
          if (res !== 5 || stored !== 5 || log.join(",") !== "base,key,get") return false;

          // `??=` short-circuits only on non-nullish values, even when falsy.
          log.length = 0;
          stored = 0;
          res = (base()[key()] ??= rhs());
          if (res !== 0 || stored !== 0 || log.join(",") !== "base,key,get") return false;

          log.length = 0;
          stored = undefined;
          res = (base()[key()] ??= rhs());
          if (res !== 2 || stored !== 2 || log.join(",") !== "base,key,get,rhs,set:2") return false;

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}
