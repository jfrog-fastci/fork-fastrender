use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
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
      | "yield*"
      | "GeneratorResume"
      | "GeneratorResumeAbrupt"
      | "Generator.prototype.next"
      | "Generator.prototype.return"
      | "Generator.prototype.throw",
    )) => Ok(()),
    Err(err) => Err(err),
  }
}
