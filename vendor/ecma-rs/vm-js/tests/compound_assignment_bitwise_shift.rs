use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

#[test]
fn compound_assignment_bitwise_shift_number_ops() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = true;
      { let x = 5; x &= 3; ok = ok && (x === 1); }
      { let x = 5; x |= 3; ok = ok && (x === 7); }
      { let x = 5; x ^= 3; ok = ok && (x === 6); }
      { let x = 5; x <<= 1; ok = ok && (x === 10); }
      { let x = 5; x >>= 1; ok = ok && (x === 2); }
      { let x = -5; x >>>= 1; ok = ok && (x === 2147483645); }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compound_assignment_bitwise_shift_bigint_ops() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = true;
      { let x = 5n; x &= 3n; ok = ok && (x === 1n); }
      { let x = 5n; x |= 3n; ok = ok && (x === 7n); }
      { let x = 5n; x ^= 3n; ok = ok && (x === 6n); }
      { let x = 5n; x <<= 1n; ok = ok && (x === 10n); }
      { let x = 5n; x >>= 1n; ok = ok && (x === 2n); }

      // Negative shift counts reverse direction for BigInt.
      { let x = 8n; x <<= -1n; ok = ok && (x === 4n); }
      { let x = 8n; x >>= -1n; ok = ok && (x === 16n); }

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compound_assignment_bitwise_shift_bigint_unsigned_right_shift_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = false;
      let x = 1n;
      try {
        x >>>= 1n;
      } catch (e) {
        ok = e.name === "TypeError";
      }
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compound_assignment_bitwise_shift_bigint_number_mixing_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      function throwsTypeError(f) {
        try { f(); return false; } catch (e) { return e.name === "TypeError"; }
      }

      let ok = true;

      ok = ok && throwsTypeError(function() { let x = 1n; x &= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x &= 1n; });

      ok = ok && throwsTypeError(function() { let x = 1n; x |= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x |= 1n; });

      ok = ok && throwsTypeError(function() { let x = 1n; x ^= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x ^= 1n; });

      ok = ok && throwsTypeError(function() { let x = 1n; x <<= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x <<= 1n; });

      ok = ok && throwsTypeError(function() { let x = 1n; x >>= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x >>= 1n; });

      ok = ok && throwsTypeError(function() { let x = 1n; x >>>= 1; });
      ok = ok && throwsTypeError(function() { let x = 1; x >>>= 1n; });

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compound_assignment_bitwise_shift_reference_evaluation_order() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let log = [];
      let o = {};

      Object.defineProperty(o, "x", {
        get() { log.push("get"); return 5; },
        set(v) { log.push("set:" + v); },
        configurable: true,
      });

      function obj() { log.push("obj"); return o; }
      function key() { log.push("key"); return "x"; }
      function rhs() { log.push("rhs"); return 3; }

      obj()[key()] &= rhs();

      log.join(",") === "obj,key,get,rhs,set:1"
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
