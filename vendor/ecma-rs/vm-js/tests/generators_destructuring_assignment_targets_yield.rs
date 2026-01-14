use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_assigns_into_computed_member_with_yield_in_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var o = {};
            ({a: o[(yield 1)]} = {a: 7});
            return o.k === 7;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assigns_into_member_with_yield_in_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var o;
            ({a: (o = yield 0).x} = {a: 7});
            return o.x === 7;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var obj = {};
          var r2 = it.next(obj);
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_rest_target_with_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var o = {};
            ({...o[(yield 1)]} = {a: 1, b: 2});
            return o.k.a === 1 && o.k.b === 2;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_rest_target_with_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var o = {};
            ([...o[(yield 1)]] = [1, 2, 3]);
            return o.k.length === 3 && o.k[0] === 1 && o.k[2] === 3;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_target_is_evaluated_before_getv() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          function* g() {
            var o = {};
            var src = { get a() { log.push("get"); return 7; } };
            ({a: o[(yield (log.push("yield"), 1))]} = src);
            return o.k === 7;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          // The assignment target yields before `GetV` reads `src.a`.
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true && log.join("|") === "yield|get";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_rest_target_is_evaluated_before_copy_data_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          function* g() {
            var o = {};
            var src = { get a() { log.push("get"); return 1; } };
            ({...o[(yield (log.push("yield"), 1))]} = src);
            return o.k.a === 1;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          // The rest assignment target yields before `CopyDataProperties` reads `src.a`.
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true && log.join("|") === "yield|get";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
