use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_for_in_rhs_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            for (var k in (yield {a: 1, b: 2})) {
              return k;
            }
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || typeof r1.value !== "object") return false;
          var r2 = it.next(r1.value);
          return r2.done === true && r2.value === "a";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_in_body_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var o = {a: 1, b: 2};
            var seen = "";
            for (var k in o) {
              seen += k;
              yield k;
            }
            return seen;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== "a") return false;
          var r2 = it.next();
          if (r2.done !== false || r2.value !== "b") return false;
          var r3 = it.next();
          return r3.done === true && r3.value === "ab";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_body_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var out = [];
            for (var i = 0; i < 2; i++) {
              out.push(i);
              yield i;
            }
            return out.join(",");
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var r2 = it.next();
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next();
          return r3.done === true && r3.value === "0,1";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_post_can_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var i = 0;
            for (; i < 2; i = yield i) {}
            return i;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var r2 = it.next(1);
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next(2);
          return r3.done === true && r3.value === 2;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_for_triple_let_per_iteration_env_can_suspend() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g() {
            var fns = [];
            for (let i = 0; i < 2; i++) {
              fns.push(() => i);
              yield i;
            }
            return fns[0]() === 0 && fns[1]() === 1;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          var r2 = it.next();
          if (r2.done !== false || r2.value !== 1) return false;
          var r3 = it.next();
          return r3.done === true && r3.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

