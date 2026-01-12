use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Generator resume-abrupt semantics exercise nested `try/finally` + `yield*` delegation paths and
  // allocate non-trivial continuation state. Use a larger heap limit than the default 1MiB used by
  // many unit tests so we focus on semantics rather than heap pressure.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_return_runs_finally_and_can_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var log = '';
      function* g() {
        try {
          yield 1;
        } finally {
          log += 'f';
          yield 2;
        }
      }

      var it = g();
      var r1 = it.next();
      var r2 = it.return(42);
      var r3 = it.next();
      var r4 = it.next();

      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 42 && r3.done === true &&
      r4.value === undefined && r4.done === true &&
      log === 'f'
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_throw_runs_finally_and_rethrows_after_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var log = '';
      function* g() {
        try {
          yield 1;
        } finally {
          log += 'f';
          yield 2;
        }
      }

      var it = g();
      var r1 = it.next();
      var r2 = it.throw(99);

      var threw = false;
      var caught;
      try {
        it.next();
      } catch (e) {
        threw = true;
        caught = e;
      }

      var r4 = it.next();

      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      threw === true && caught === 99 &&
      r4.value === undefined && r4.done === true &&
      log === 'f'
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_return_forwards_to_delegate_return() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var log = '';
      var iterable = {};

      iterable[Symbol.iterator] = function () {
        log += 'i';
        return {
          next: function () {
            log += 'n';
            return { value: 1, done: false };
          },
          return: function (v) {
            log += 'r' + v;
            return { value: 'ret:' + v, done: true };
          }
        };
      };

      function* g() { return yield* iterable; }

      var it = g();
      var r1 = it.next();
      var r2 = it.return(9);
      var r3 = it.next();

      r1.value === 1 && r1.done === false &&
      r2.value === 'ret:9' && r2.done === true &&
      r3.value === undefined && r3.done === true &&
      log === 'inr9'
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_throw_without_delegate_throw_closes_and_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let v = rt.exec_script(
    r#"
      var log = '';
      var iterable = {};

      iterable[Symbol.iterator] = function () {
        return {
          next: function () {
            return { value: 1, done: false };
          },
          return: function () {
            log += 'r';
            return { value: 0, done: true };
          }
        };
      };

      function* g() { yield* iterable; }

      var it = g();
      var r1 = it.next();

      var threw = false;
      var name;
      try {
        it.throw(99);
      } catch (e) {
        threw = true;
        name = e && e.name;
      }

      var r3 = it.next();

      r1.value === 1 && r1.done === false &&
      threw === true && name === 'TypeError' &&
      r3.value === undefined && r3.done === true &&
      log === 'r'
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
