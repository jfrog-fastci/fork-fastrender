use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator `yield*` uses Promise jobs and async iterator protocol wiring. Use a slightly
  // larger heap than the default 1MiB used by many unit tests so we exercise delegation semantics
  // rather than heap pressure.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn yield_star_forwards_next_values_and_always_calls_delegate_next_with_one_argument(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let script = r#"
    var nextArgLens = [];
    var nextArgs = [];

    var iter = {
      i: 0,
      next: function (v) {
        nextArgLens.push(arguments.length);
        nextArgs.push(v);
        this.i++;
        if (this.i === 1) return Promise.resolve({ value: 'a', done: false });
        if (this.i === 2) return Promise.resolve({ value: 'b', done: false });
        return Promise.resolve({ value: 99, done: true });
      },
    };
    iter[Symbol.asyncIterator] = function () { return this; };

    var out = false;
    async function test() {
      async function* g() { return yield* iter; }
      var it = g();

      var r1 = await it.next('ignored');
      var r2 = await it.next();
      var r3 = await it.next(123);

      return (
        r1.value === 'a' && r1.done === false &&
        r2.value === 'b' && r2.done === false &&
        r3.value === 99 && r3.done === true &&
        nextArgLens.join(',') === '1,1,1' &&
        nextArgs[0] === undefined &&  // first next arg ignored by generator start
        nextArgs[1] === undefined &&
        nextArgs[2] === 123
      );
    }
    test().then(v => { out = v; });
    out
  "#;

  // Promise jobs have not run yet, so `out` should still be `false`.
  let value = rt.exec_script(script)?;
  assert_eq!(value, Value::Bool(false));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
