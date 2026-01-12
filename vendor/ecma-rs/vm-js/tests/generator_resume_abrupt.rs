use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn generator_prototype_return_aborts_execution() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let out = rt.exec_script(
    r#"
       (() => {
         var beforeCount = 0;
         var afterCount = 0;
         var iter = function*() {
          beforeCount += 1;
          yield;
          afterCount += 1;
         }();
         iter.next();
         var result = iter.return(595);
         return result.done === true
          && result.value === 595
          && beforeCount === 1
          && afterCount === 0;
      })()
    "#,
  )?;
  assert!(matches!(out, Value::Bool(true)));
  Ok(())
}

#[test]
fn generator_prototype_throw_throws_value() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let out = rt.exec_script(
    r#"
      (() => {
        var iter = function*() { yield 1; }();
        iter.next();
        try {
          iter.throw(42);
          return false;
        } catch (e) {
          return e === 42;
        }
      })()
    "#,
  )?;
  assert!(matches!(out, Value::Bool(true)));
  Ok(())
}
