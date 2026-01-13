use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

/// Returns `true` if `async function*` is supported by the runtime.
///
/// `vm-js` historically parsed `async function*` (since it is valid ECMAScript syntax) but threw a
/// catchable `SyntaxError("async generator functions")` at runtime. These tests are meant to
/// exercise yield semantics once async generators are implemented, but should not fail while the
/// feature is still unavailable.
fn async_generators_supported(rt: &mut JsRuntime) -> Result<bool, VmError> {
  let supported = rt.exec_script(
    r#"
      (() => {
        try {
          (async function* () {});
          return true;
        } catch (e) {
          // Preserve unexpected failures (e.g. if parsing or error objects regress).
          if (e && e.name === "SyntaxError" && e.message === "async generator functions") {
            return false;
          }
          throw e;
        }
      })()
    "#,
  )?;
  Ok(supported == Value::Bool(true))
}

#[test]
fn async_generator_yield_without_operand_yields_undefined_even_if_shadowed() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !async_generators_supported(&mut rt)? {
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
  if !async_generators_supported(&mut rt)? {
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

