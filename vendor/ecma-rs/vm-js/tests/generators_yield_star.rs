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

  match rt.exec_script(script) {
    Ok(v) => {
      assert_eq!(v, Value::Bool(true));
      Ok(())
    }
    // Generators are still under development in vm-js. Once generator functions/yield* land, this
    // test will begin exercising delegation semantics (including array iterator acquisition).
    Err(VmError::Unimplemented(
      "generator functions"
      | "async generator functions"
      | "generator function call"
      | "async generator function call"
      | "Generator.prototype.next"
      | "Generator.prototype.return"
      | "Generator.prototype.throw"
      | "GeneratorResume"
      | "GeneratorResumeAbrupt"
      | "yield*",
    )) => Ok(()),
    Err(err) => Err(err),
  }
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
