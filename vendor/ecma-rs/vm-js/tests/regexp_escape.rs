use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn regexp_escape_escapes_per_spec() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var ok = true;
    ok = ok && typeof RegExp.escape === 'function';

    // Initial char escape (DecimalDigit / AsciiLetter).
    ok = ok && RegExp.escape('1111') === '\\x31111';

    // Syntax characters.
    ok = ok && RegExp.escape('^$\\.*+?()[]{}|') === '\\^\\$\\\\\\.\\*\\+\\?\\(\\)\\[\\]\\{\\}\\|';

    // Line terminators.
    ok = ok && RegExp.escape('\u2028') === '\\u2028';

    // Solidus.
    ok = ok && RegExp.escape('/') === '\\/';

    // UTF-16 encode code points and whitespace escaping.
    ok = ok && RegExp.escape('Γειά σου') === 'Γειά\\x20σου';

    ok
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

