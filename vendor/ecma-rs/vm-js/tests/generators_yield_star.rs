use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Generator support (and the test262-style delegation wrapper) allocates a non-trivial amount of
  // runtime state. Use a larger heap limit than the default 1MiB used by many unit tests so this
  // test exercises yield* semantics rather than heap pressure.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn yield_star_over_array_delegates_values() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var log = '';

    function IteratorWrapper(iterator) {
        return {
            next: function (val) {
                log += 'n';
                return iterator.next(val);
            },

            throw: function (exn) {
                log += 't';
                return iterator.throw(exn);
            }
        };
    }

    function IterableWrapper(iterable) {
        var ret = {};

        ret[Symbol.iterator] = function () {
            log += 'i';
            return IteratorWrapper(iterable[Symbol.iterator]());
        }

        return ret;
    }

    function* d(x) { return yield* x; }

    // Wrapper iterable: yield* must call @@iterator to acquire the iterator and then call `next`
    // repeatedly.
    var it = d(IterableWrapper([1,2,3]));
    var r1 = it.next();
    var r2 = it.next();
    var r3 = it.next();
    var r4 = it.next();

    var ok1 =
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.value === undefined && r4.done === true &&
      log === 'innnn';

    // Array: yield* must still use the iterator protocol (i.e. it must call @@iterator), even when
    // the delegate is a normal Array.
    var saved = Array.prototype[Symbol.iterator];
    Array.prototype.__origIterator = saved;
    Array.prototype[Symbol.iterator] = function () {
      log += 'i';
      return IteratorWrapper(this.__origIterator());
    };

    it = d([1,2,3]);
    r1 = it.next();
    r2 = it.next();
    r3 = it.next();
    r4 = it.next();

    var ok2 =
      r1.value === 1 && r1.done === false &&
      r2.value === 2 && r2.done === false &&
      r3.value === 3 && r3.done === false &&
      r4.value === undefined && r4.done === true &&
      log === 'innnninnnn';

    Array.prototype[Symbol.iterator] = saved;
    Array.prototype.__origIterator = undefined;

    ok1 && ok2
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_yields_iterator_result_object_directly() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var valueGetterCalls = 0;

    var iterResult = {};
    Object.defineProperty(iterResult, "done", { value: false });
    Object.defineProperty(iterResult, "value", {
      get: function () {
        valueGetterCalls++;
        return 1;
      }
    });
    iterResult.extra = 123;

    var nextCount = 0;
    var iterator = {
      next: function () {
        nextCount++;
        if (nextCount === 1) return iterResult;
        return { value: 2, done: true };
      }
    };
    var iterable = {};
    iterable[Symbol.iterator] = function () { return iterator; };

    function* g() { yield* iterable; }

    var it = g();
    var r1 = it.next();
    var ok1 =
      r1 === iterResult &&
      r1.extra === 123 &&
      valueGetterCalls === 0;
    var v = r1.value;
    var ok2 = v === 1 && valueGetterCalls === 1;

    var r2 = it.next();
    var ok3 = r2.done === true && r2.value === undefined;

    ok1 && ok2 && ok3
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_delegate_next_is_always_called_with_one_argument() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // SpiderMonkey test262 staging analogue: `delegating-yield-11.js`.
  let script = r#"
    var nextArgLens = [];
    var nextArgs = [];

    var iter = {
      i: 0,
      next: function (v) {
        nextArgLens.push(arguments.length);
        nextArgs.push(v);
        if (this.i++ < 2) {
          return { value: this.i, done: false };
        }
        return { value: 99, done: true };
      }
    };
    iter[Symbol.iterator] = function () { return this; };

    function* g() { return yield* iter; }
    var it = g();

    it.next("ignored");
    it.next();
    it.next(123);

    nextArgLens.join(",") === "1,1,1" &&
    nextArgs[0] === undefined && // first `next` arg is ignored by generator start
    nextArgs[1] === undefined &&
    nextArgs[2] === 123
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_throw_delegates_to_iterator_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var log = '';

    var iterator = {
      next: function (v) {
        log += 'n' + v;
        return { value: 1, done: false };
      },

      throw: function (e) {
        log += 't' + e;
        return { value: 99, done: true };
      }
    };
    var iterable = {};
    iterable[Symbol.iterator] = function () { return iterator; };

    function* g() { return yield* iterable; }

    var it = g();
    var r1 = it.next(123);
    var ok1 = r1.value === 1 && r1.done === false;

    var r2 = it.throw('boom');
    var ok2 = r2.value === 99 && r2.done === true;

    ok1 && ok2 && log === 'nundefinedtboom'
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_throw_without_throw_method_closes_iterator_and_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var returnCalls = 0;
    var returnArgsLen = null;

    var iterator = {
      next: function () { return { value: 1, done: false }; },
      return: function () { returnCalls++; returnArgsLen = arguments.length; return {}; }
    };
    var iterable = {};
    iterable[Symbol.iterator] = function () { return iterator; };

    function* g() { yield* iterable; }

    var it = g();
    it.next();

    var caught = false;
    try {
      it.throw('boom');
    } catch (e) {
      caught = (e instanceof TypeError) && (e !== 'boom');
    }

    caught && returnCalls === 1 && returnArgsLen === 0
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_throw_without_throw_method_propagates_iterator_close_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var returnCalls = 0;
    var returnArgsLen = null;

    var iterator = {
      next: function () { return { value: 1, done: false }; },
      return: function () {
        returnCalls++;
        returnArgsLen = arguments.length;
        throw "closeError";
      }
    };
    var iterable = {};
    iterable[Symbol.iterator] = function () { return iterator; };

    function* g() { yield* iterable; }
    var it = g();
    it.next();

    var caught = false;
    try {
      it.throw("boom");
    } catch (e) {
      caught = (e === "closeError");
    }

    caught && returnCalls === 1 && returnArgsLen === 0
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_return_delegates_to_iterator_return() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var log = '';

    var iterator = {
      next: function () { log += 'n'; return { value: 1, done: false }; },
      return: function (v) { log += 'r' + v; return { value: 77, done: true }; }
    };
    var iterable = {};
    iterable[Symbol.iterator] = function () { return iterator; };

    function* g() { yield* iterable; }

    var it = g();
    it.next();
    var r = it.return(42);

    r.done === true && r.value === 77 && log === 'nr42'
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_return_done_false_yields_iterator_result_object() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var valueGets = 0;
    var innerReturnResult = {};
    Object.defineProperty(innerReturnResult, "done", { value: false });
    Object.defineProperty(innerReturnResult, "value", {
      get: function () { valueGets++; return "continue"; }
    });

    var returnArgsLen = null;
    var iter = {
      i: 0,
      next: function () {
        if (this.i++ === 0) return { value: 1, done: false };
        return { value: 123, done: true };
      },
      return: function (v) {
        returnArgsLen = arguments.length;
        return innerReturnResult;
      }
    };
    iter[Symbol.iterator] = function () { return this; };

    function* g() { return yield* iter; }
    var it = g();
    it.next();

    var r1 = it.return("x");
    var ok1 = (r1 === innerReturnResult) && (valueGets === 0) && (returnArgsLen === 1);

    var r2 = it.next();
    var ok2 = r2.value === 123 && r2.done === true;

    ok1 && ok2
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn yield_star_return_pending_propagates_out_of_generator() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = r#"
    var after = false;

    function* inner() {
      try { yield 1; } finally { yield 2; }
    }
    function* outer() {
      yield* inner();
      after = true;
    }

    var it = outer();
    var r1 = it.next();
    var r2 = it.return('R');
    var r3 = it.next();

    r1.value === 1 && r1.done === false &&
    r2.value === 2 && r2.done === false &&
    r3.value === 'R' && r3.done === true &&
    after === false
  "#;

  let v = rt.exec_script(script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}
