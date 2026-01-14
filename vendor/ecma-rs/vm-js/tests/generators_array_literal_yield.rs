use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_array_literal_yield_multiple_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ (yield 1), (yield 2) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(10);
      var r3 = it.next(20);
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true &&
      Array.isArray(r3.value) &&
      r3.value.length === 2 &&
      r3.value[0] === 10 &&
      r3.value[1] === 20
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ ...(yield 0), 4 ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next([1, 2, 3]);
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 4 &&
      r2.value[0] === 1 &&
      r2.value[1] === 2 &&
      r2.value[2] === 3 &&
      r2.value[3] === 4
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_elision_and_yield_preserves_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ , (yield 1) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("x");
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 2 &&
      r2.value[1] === "x" &&
      Object.prototype.hasOwnProperty.call(r2.value, 0) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_throw_aborts_remaining_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ (yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var threw = false;
      try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      threw === true &&
      ran === false &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_return_aborts_remaining_elements() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ (yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.return(99);
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 99 && r2.done === true &&
      ran === false &&
      r3.done === true && r3.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_throw_aborts_remaining_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ ...(yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var threw = false;
      try { it.throw("boom"); } catch (e) { threw = (e === "boom"); }
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      threw === true &&
      ran === false &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_return_aborts_remaining_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ran = false;
      function* g() { return [ ...(yield 1), (ran = true) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.return(99);
      var r3 = it.next();
      r1.value === 1 && r1.done === false &&
      r2.value === 99 && r2.done === true &&
      ran === false &&
      r3.done === true && r3.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread_non_iterable_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ ...(yield 1) ]; }
      var it = g();
      var r1 = it.next();
      var threw = false;
      var name;
      try {
        it.next(123); // not iterable
      } catch (e) {
        threw = true;
        name = e && e.name;
      }
      var r2 = it.next();
      r1.value === 1 && r1.done === false &&
      threw === true && name === 'TypeError' &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_spread_yield_star_final_value_is_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* inner() { yield 1; yield 2; return [3, 4]; }
      function* g() { return [ ...(yield* inner()), 5 ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("x");
      var r3 = it.next("y");
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.done === true &&
      Array.isArray(r3.value) &&
      r3.value.length === 3 &&
      r3.value[0] === 3 &&
      r3.value[1] === 4 &&
      r3.value[2] === 5
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_after_spread_uses_updated_index() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ ...(yield 0), (yield 1) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next([1, 2]);
      var r3 = it.next(99);
      r1.value === 0 && r1.done === false &&
      r2.value === 1 && r2.done === false &&
      r3.done === true &&
      Array.isArray(r3.value) &&
      r3.value.length === 3 &&
      r3.value[0] === 1 &&
      r3.value[1] === 2 &&
      r3.value[2] === 99
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread_preserves_hole_before_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ , ...(yield 0) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next([1, 2]);
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 3 &&
      Object.prototype.hasOwnProperty.call(r2.value, 0) === false &&
      r2.value[1] === 1 &&
      r2.value[2] === 2
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread_preserves_left_to_right_eval_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = '';
      function f() { log += 'f'; return 1; }
      function g() { log += 'g'; return 2; }

      function* gen() { return [ f(), ...(yield 0), g() ]; }
      var it = gen();
      var r1 = it.next();
      var r2 = it.next([10]);

      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 3 &&
      r2.value[0] === 1 &&
      r2.value[1] === 10 &&
      r2.value[2] === 2 &&
      log === 'fg'
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_elisions_around_spread_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ , ...(yield 0), , 4 ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next([1, 2]);
      r1.value === 0 && r1.done === false &&
      r2.done === true &&
      Array.isArray(r2.value) &&
      r2.value.length === 5 &&
      Object.prototype.hasOwnProperty.call(r2.value, 0) === false &&
      r2.value[1] === 1 &&
      r2.value[2] === 2 &&
      Object.prototype.hasOwnProperty.call(r2.value, 3) === false &&
      r2.value[4] === 4
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_multiple_yields_mixed_single_and_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() { return [ (yield 1), ...(yield 2), (yield 3) ]; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("a");
      var r3 = it.next([10, 20]);
      var r4 = it.next("b");
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.done === true &&
      Array.isArray(r4.value) &&
      r4.value.length === 4 &&
      r4.value[0] === "a" &&
      r4.value[1] === 10 &&
      r4.value[2] === 20 &&
      r4.value[3] === "b"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread_get_iterator_happens_after_resume() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = '';
      var obj = {};
       Object.defineProperty(obj, Symbol.iterator, {
         get: function () {
           log += 'i';
           return function () {
             log += 'c';
             var done = false;
             return {
               next: function () {
                 log += 'n';
                 if (done) return { done: true };
                 done = true;
                 return { value: 1, done: false };
               }
             };
           };
         }
       });

      function* g() { return [ ...(yield 0) ]; }
      var it = g();
      var r1 = it.next();
      var log_before = log;
      var r2 = it.next(obj);

      r1.value === 0 && r1.done === false &&
      log_before === '' &&
      r2.done === true &&
      Array.isArray(r2.value) &&
       r2.value.length === 1 &&
       r2.value[0] === 1 &&
       log === 'icnn'
     "#,
     )
     .unwrap();
   assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_array_literal_yield_in_spread_next_throw_does_not_invoke_return() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = '';
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { log += 'n'; throw 'boom'; },
          return: function () { log += 'r'; return { done: true }; }
        };
      };

      function* g() { return [ ...(yield 0) ]; }
      var it = g();
      var r1 = it.next();

      var threw = false;
      try {
        it.next(iterable);
      } catch (e) {
        threw = (e === 'boom');
      }

      var r2 = it.next();

      r1.value === 0 && r1.done === false &&
      threw === true &&
      log === 'n' &&
      r2.done === true && r2.value === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
