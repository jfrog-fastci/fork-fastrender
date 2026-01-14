use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn string_case_and_locale_methods_require_object_coercible() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function isTypeError(thunk) {
        try { thunk(); return false; } catch (e) { return e && e.name === "TypeError"; }
      }

      let ok = true;
      ok = ok && isTypeError(() => String.prototype.toLowerCase.call(null));
      ok = ok && isTypeError(() => String.prototype.toUpperCase.call(undefined));
      ok = ok && isTypeError(() => String.prototype.toLocaleLowerCase.call(null));
      ok = ok && isTypeError(() => String.prototype.toLocaleUpperCase.call(undefined));
      ok = ok && isTypeError(() => String.prototype.localeCompare.call(null, "a"));
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_to_lower_case_final_sigma() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      // FINAL SIGMA special-casing (Σ → ς at end of word after skipping Case_Ignorable).
      let ok = true
        && "A\u03A3".toLowerCase() === "a\u03C2"
        && "A\u03A3".toLocaleLowerCase() === "a\u03C2"
        && "A\u180E\u03A3".toLowerCase() === "a\u180E\u03C2"
        && "A\u03A3B".toLowerCase() === "a\u03C3b";
      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_locale_compare_canonical_equivalence() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      // Canonical equivalence: "o" + COMBINING DIAERESIS vs precomposed "ö".
      "o\u0308".localeCompare("ö") === 0
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_normalize_forms_and_errors() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let ok = true;
      let s = "\u1E9B\u0323";
      ok = ok && s.normalize("NFC") === "\u1E9B\u0323";
      ok = ok && s.normalize("NFD") === "\u017F\u0323\u0307";

      try {
        "foo".normalize("not-a-form");
        ok = false;
      } catch (e) {
        ok = ok && e && e.name === "RangeError";
      }

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_well_formed_unicode_methods() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let leading = "\uD83D";
      let trailing = "\uDCA9";
      let whole = leading + trailing;
      let replacement = "\uFFFD";

      let ok = true
        && ("a" + leading + "b").isWellFormed() === false
        && ("a" + whole + "b").isWellFormed() === true
        && ("a" + leading + "b").toWellFormed() === ("a" + replacement + "b")
        && whole.slice(0, 1).toWellFormed() === replacement
        && whole.slice(1).toWellFormed() === replacement;

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_last_index_of_nan_position_is_len() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var __obj = { toString: function() { return "AB"; } };
      var __obj2 = { valueOf: function() { return NaN; } };
      var __obj3 = { valueOf: function() { return {}; }, toString: function() {} };
      var s = "ABBABABAB";
      s.lastIndexOf(__obj, __obj2) === 7 && s.lastIndexOf(__obj, __obj3) === 7
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
