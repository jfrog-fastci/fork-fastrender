use vm_js::regexp_case_fold;
use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn as_utf8_lossy(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn regexp_case_folding_includes_common_mappings() {
  // U+212A KELVIN SIGN (K) -> U+006B 'k'
  assert_eq!(regexp_case_fold(0x212A), 0x006B);
  // U+2126 OHM SIGN (Ω) -> U+03C9 GREEK SMALL LETTER OMEGA (ω)
  assert_eq!(regexp_case_fold(0x2126), 0x03C9);
  // U+017F LATIN SMALL LETTER LONG S (ſ) -> U+0073 's'
  assert_eq!(regexp_case_fold(0x017F), 0x0073);
}

#[test]
fn regexp_case_folding_ignores_full_mappings() {
  // U+00DF LATIN SMALL LETTER SHARP S (ß) has a *full* fold to "ss", but no simple/common fold.
  // RegExp `Canonicalize`/`scf` must therefore not expand it into multiple code points.
  assert_eq!(regexp_case_fold(0x00DF), 0x00DF);
}

#[test]
fn regexp_unicode_ignore_case_uses_common_case_folding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      [
        /K/iu.test("k"),
        /Ω/iu.test("ω"),
        /ſ/iu.test("S"),
        /ß/iu.test("ss"),
      ].join(",")
    "#,
    )
    .unwrap();

  // K => k (Common), Ω => ω (Common), ſ => s (Common), and ß must not expand to "ss".
  assert_eq!(as_utf8_lossy(&rt, value), "true,true,true,false");
}
