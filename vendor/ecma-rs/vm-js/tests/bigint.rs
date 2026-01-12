use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  // Use a larger heap for BigInt tests: we intentionally allocate BigInts beyond 256 bits.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn assert_termination_reason(err: VmError, expected: TerminationReason) {
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, expected),
    other => panic!("expected VmError::Termination({expected:?}), got {other:?}"),
  }
}

#[test]
fn bigint_constructor_conversions_and_errors() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Primitive conversions.
  assert_eq!(rt.exec_script("BigInt(1) === 1n")?, Value::Bool(true));
  assert_eq!(rt.exec_script("BigInt(true) === 1n")?, Value::Bool(true));
  assert_eq!(rt.exec_script("BigInt(false) === 0n")?, Value::Bool(true));
  assert_eq!(rt.exec_script("BigInt(\"123\") === 123n")?, Value::Bool(true));
  // `StringToBigInt` treats the empty/whitespace-only string as 0n.
  assert_eq!(rt.exec_script("BigInt(\"\") === 0n")?, Value::Bool(true));
  assert_eq!(rt.exec_script("BigInt(\"   \") === 0n")?, Value::Bool(true));

  // Error cases.
  assert_eq!(
    rt.exec_script("try { BigInt(1.5); false } catch(e) { e.name === \"RangeError\" }")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script("try { BigInt(\"1n\"); false } catch(e) { e.name === \"SyntaxError\" }")?,
    Value::Bool(true)
  );
  // Signed non-decimal radix forms are rejected (`BigInt(\"-0x10\")` throws).
  assert_eq!(
    rt.exec_script("try { BigInt(\"-0x10\"); false } catch(e) { e.name === \"SyntaxError\" }")?,
    Value::Bool(true)
  );

  // BigInt/Number mixing throws for arithmetic operators.
  assert_eq!(
    rt.exec_script("try { 1n + 1; false } catch(e) { e.name === \"TypeError\" }")?,
    Value::Bool(true)
  );

  Ok(())
}

#[test]
fn bigint_supports_values_beyond_256_bits() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Ensure shifts and comparisons work beyond the old 256-bit inline BigInt implementation.
  assert_eq!(
    rt.exec_script("(1n << 300n) > (1n << 256n)")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script("((1n << 300n) >> 300n) === 1n")?,
    Value::Bool(true)
  );

  // BigInt.prototype.toString should exist and work.
  let v = rt.exec_script("(1n << 300n).toString(16).length === 76")?;
  assert_eq!(v, Value::Bool(true));

  Ok(())
}

#[test]
fn bigint_as_int_n_and_as_uint_n_work() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  assert_eq!(
    rt.exec_script("BigInt.asIntN(8, 255n) === (-1n)")?,
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script("BigInt.asUintN(8, -1n) === 255n")?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn bigint_division_is_budgeted() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // Use a budget that is sufficient to parse/instantiate but not enough to complete the internal
  // BigInt long division loop (which must call `vm.tick()`).
  rt.vm.set_budget(Budget {
    fuel: Some(200),
    deadline: None,
    check_time_every: 1,
  });

  let err = rt.exec_script("(1n << 65536n) / 3n").unwrap_err();
  assert_termination_reason(err, TerminationReason::OutOfFuel);
  Ok(())
}

