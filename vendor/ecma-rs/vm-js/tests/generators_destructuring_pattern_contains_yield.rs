use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_yield_in_object_destructuring_default_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = {};
          let res = ({a = yield 1} = rhs);
          return res === rhs && a === 42;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(42);
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_default_not_evaluated_when_present() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = {a: 5};
          let res = ({a = yield 1} = rhs);
          return res === rhs && a === 5;
        }
        var it = g();
        var r1 = it.next();
        return r1.done === true && r1.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_default_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = [];
          let res = ([a = yield 1] = rhs);
          return res === rhs && a === 7;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(7);
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_default_not_evaluated_when_present() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = [5];
          let res = ([a = yield 1] = rhs);
          return res === rhs && a === 5;
        }
        var it = g();
        var r1 = it.next();
        return r1.done === true && r1.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_key_preserves_assignment_result() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let rhs = {x: 5};
          let res = ({[(yield 1)]: a} = rhs);
          return res === rhs && a === 5;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next("x");
        return r2.done === true && r2.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_key_then_default_yields_twice() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let b = 0;
          ({[(yield 1)]: a, b = yield 2} = {x: 5});
          return a === 5 && b === 7;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next("x");
        if (r2.done !== false || r2.value !== 2) return false;
        var r3 = it.next(7);
        return r3.done === true && r3.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_two_defaults_yields_twice() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        function* g(){
          let a = 0;
          let b = 0;
          ([a = yield 1, b = yield 2] = []);
          return a === 3 && b === 4;
        }
        var it = g();
        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        var r2 = it.next(3);
        if (r2.done !== false || r2.value !== 2) return false;
        var r3 = it.next(4);
        return r3.done === true && r3.value === true;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_object_destructuring_computed_key_then_default_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        var steps = [];
        var a = 0;

        var rhs = new Proxy({x: undefined}, {
          get: function(t, k, _r) {
            steps.push("get:" + String(k));
            return t[k];
          },
        });

        var res;
        function* g() {
          res = ({[(steps.push("key"), (yield 1))]: a = (steps.push("default"), (yield 2))} = rhs);
          return res === rhs && a === 7;
        }

        var it = g();

        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;
        if (steps.join("|") !== "key") return false;

        var r2 = it.next("x");
        if (r2.done !== false || r2.value !== 2) return false;
        if (steps.join("|") !== "key|get:x|default") return false;

        var r3 = it.next(7);
        return (
          r3.done === true &&
          r3.value === true &&
          steps.join("|") === "key|get:x|default"
        );
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_yield_in_array_destructuring_default_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        var steps = [];
        var a = 0;
        var arr = new Proxy([undefined], {
          get: function(t, k, r) {
            steps.push(String(k));
            return Reflect.get(t, k, r);
          }
        });

        var res;
        function* g() {
          res = ([a = (steps.push("default"), (yield 1))] = arr);
          return res === arr && a === 3;
        }

        var it = g();

        var r1 = it.next();
        if (r1.done !== false || r1.value !== 1) return false;

        var idxLength = steps.indexOf("length");
        var idx0 = steps.indexOf("0");
        var idxDefault = steps.indexOf("default");
        if (idxLength < 0 || idx0 < 0 || idxDefault < 0) return false;
        if (!(idxLength < idx0 && idx0 < idxDefault)) return false;
        if (idxDefault !== steps.length - 1) return false;

        var r2 = it.next(3);
        return r2.done === true && r2.value === true && idxDefault === steps.length - 1;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
