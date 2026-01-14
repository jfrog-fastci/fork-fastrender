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

#[test]
fn generator_object_destructuring_assignment_target_super_computed_is_evaluated_before_getv() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g() {
              var src = { get a() { log.push("get"); return 7; } };
              ({a: super[(yield (log.push("yield"), 1))]} = src);
              return this._k === 7;
            }
          }
          var it = (new Derived()).g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true && log.join("|") === "yield|get|set";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_rest_target_super_computed_is_evaluated_before_copy_data_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g() {
              var src = { get a() { log.push("get"); return 1; } };
              ({...super[(yield (log.push("yield"), 1))]} = src);
              return this._k.a === 1;
            }
          }
          var it = (new Derived()).g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true && log.join("|") === "yield|get|set";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_target_super_computed_is_evaluated_before_iterator_step() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g(iterable) {
              [super[(yield (log.push("yield"), 1))]] = iterable;
              return this._k === 7;
            }
          }
          var iterable = {
            [Symbol.iterator]() {
              var done = false;
              return {
                next() {
                  log.push("next");
                  if (done) return { value: undefined, done: true };
                  done = true;
                  return { value: 7, done: false };
                }
              };
            }
          };
          var it = (new Derived()).g(iterable);
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return r2.done === true && r2.value === true && log.join("|") === "yield|next|set";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_rest_target_super_computed_is_evaluated_before_iterator_step() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g(iterable) {
              [...super[(yield (log.push("yield"), 1))]] = iterable;
              return this._k.length === 3 && this._k[0] === 1 && this._k[2] === 3;
            }
          }
          var iterable = {
            [Symbol.iterator]() {
              var i = 0;
              return {
                next() {
                  log.push("next");
                  if (i >= 3) return { value: undefined, done: true };
                  i += 1;
                  return { value: i, done: false };
                }
              };
            }
          };
          var it = (new Derived()).g(iterable);
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          if (log.join("|") !== "yield") return false;
          var r2 = it.next("k");
          return (
            r2.done === true &&
            r2.value === true &&
            log.join("|") === "yield|next|next|next|next|set"
          );
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_assignment_target_super_computed_throw_aborts_before_getv() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g() {
              var src = { get a() { log.push("get"); return 7; } };
              try {
                ({a: super[(yield 0)]} = src);
              } catch (e) {
                log.push("catch");
              }
              return log.join("|");
            }
          }
          var it = (new Derived()).g();
          var r0 = it.next();
          if (r0.done !== false || r0.value !== 0) return false;
          var r1 = it.throw("boom");
          return r1.done === true && r1.value === "catch" && log.join("|") === "catch";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_destructuring_rest_target_super_computed_throw_aborts_before_copy_data_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var log = [];
          class Base {
            set k(v) { log.push("set"); this._k = v; }
          }
          class Derived extends Base {
            *g() {
              var src = { get a() { log.push("get"); return 1; } };
              try {
                ({...super[(yield 0)]} = src);
              } catch (e) {
                log.push("catch");
              }
              return log.join("|");
            }
          }
          var it = (new Derived()).g();
          var r0 = it.next();
          if (r0.done !== false || r0.value !== 0) return false;
          var r1 = it.throw("boom");
          return r1.done === true && r1.value === "catch" && log.join("|") === "catch";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_assignment_target_super_computed_throw_closes_iterator_without_next() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCount = 0;
          var returnCount = 0;
          var iterable = {
            [Symbol.iterator]() {
              return {
                next() { nextCount++; return { value: 7, done: false }; },
                return() { returnCount++; return {}; },
              };
            }
          };
          class Base {}
          class Derived extends Base {
            *g(iterable) {
              [super[(yield 0)]] = iterable;
            }
          }
          var it = (new Derived()).g(iterable);
          var r0 = it.next();
          if (r0.done !== false || r0.value !== 0) return false;
          try { it.throw("boom"); } catch (e) {}
          return nextCount === 0 && returnCount === 1;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_destructuring_rest_target_super_computed_throw_closes_iterator_without_next() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var nextCount = 0;
          var returnCount = 0;
          var iterable = {
            [Symbol.iterator]() {
              return {
                next() { nextCount++; return { done: false }; },
                return() { returnCount++; return {}; },
              };
            }
          };
          class Base {}
          class Derived extends Base {
            *g(iterable) {
              [...super[(yield 0)]] = iterable;
            }
          }
          var it = (new Derived()).g(iterable);
          var r0 = it.next();
          if (r0.done !== false || r0.value !== 0) return false;
          try { it.throw("boom"); } catch (e) {}
          return nextCount === 0 && returnCount === 1;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
