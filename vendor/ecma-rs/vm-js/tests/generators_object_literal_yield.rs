use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_literal_yield_in_property_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { a: (yield 1) }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      r2.value.a === 10
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { [(yield "k")]: 1 }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(123);
      r1.value === "k" && r1.done === false &&
      r2.done === true &&
      r2.value["123"] === 1 &&
      ("k" in r2.value) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { ...(yield 0), b: 2 }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next({ a: 1, b: 1 });
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      r2.value.a === 1 &&
      r2.value.b === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_value_then_method_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { a: (yield 1), m() { return this.a; } }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      r2.value.a === 10 &&
      typeof r2.value.m === "function" &&
      r2.value.m() === 10 &&
      r2.value.m.name === "m"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_value_then_getter_and_setter_members() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { a: (yield 1), get x() { return 123; }, set x(v) { this._v = v; } }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var obj = r2.value;
      obj.x = 20;
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      obj.a === 10 &&
      obj.x === 123 &&
      obj._v === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_computed_method_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return { [yield "k"]() { return 1; } }; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("m");
      var obj = r2.value;
      r1.value === "k" && r1.done === false &&
      r2.done === true &&
      typeof obj.m === "function" &&
      obj.m() === 1 &&
      obj.m.name === "m" &&
      ("k" in obj) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_yield_in_computed_getter_and_setter_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        return {
          get [yield "g"]() { return this._v; },
          set [yield "s"](v) { this._v = v; }
        };
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("x");
      var r3 = it.next("x");
      var obj = r3.value;
      obj.x = 20;
      r1.value === "g" && r1.done === false &&
      r2.value === "s" && r2.done === false &&
      r3.done === true &&
      obj._v === 20 &&
      obj.x === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
