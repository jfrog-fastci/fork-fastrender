use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn async_generator_yield_without_operand_yields_undefined_even_if_shadowed() -> Result<(), VmError>
{
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var ok = false;
      async function* g(){ var undefined = 123; yield; }
      g().next().then(r => { ok = (r.value === undefined && r.done === false); });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let ok = rt.exec_script("ok")?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn async_generator_yield_undefined_evaluates_operand_when_explicit() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  rt.exec_script(
    r#"
      var ok = false;
      async function* g(){ var undefined = 123; yield undefined; }
      g().next().then(r => { ok = (r.value === 123 && r.done === false); });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let ok = rt.exec_script("ok")?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}
