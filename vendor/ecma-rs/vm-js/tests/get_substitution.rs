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
fn get_substitution_capture_index_parsing_is_spec_compliant() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          // `$nn` two-digit fallback: `$11` with only 1 capture => `$1` + `1`.
          "a".replace(/(a)/, "$11"),

          // `$10` with only 1 capture => `$1` + `0`.
          "a".replace(/(a)/, "$10"),

          // `$10` prefers the 2-digit capture when it exists.
          "abcdefghij".replace(/(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)/, "$10"),

          // `$01` is capture 1 when present.
          "a".replace(/(a)/, "$01"),

          // `$0` and `$00` are not capture references.
          "a".replace(/a/, "$0"),
          "a".replace(/a/, "$00"),

          // `$<name>` is literal when there is no named captures object.
          "a".replace(/a/, "$<foo>"),

          // `$020` uses capture 2 (when present) and leaves trailing `0` literal.
          "ab".replace(/(a)(b)/, "$020")
        ].join("|")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "a1|a0|j|a|$0|$00|$<foo>|b0");
}

#[test]
fn get_substitution_replace_all_uses_same_patterns() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          "aba".replaceAll("a", "$$"),
          "aba".replaceAll("a", "$&"),
          "aba".replaceAll("a", "$`"),
          "aba".replaceAll("a", "$'")
        ].join("|")
      "#,
    )
     .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "$b$|aba|bab|bab");
}

#[test]
fn replace_all_falls_back_to_string_search_when_symbol_replace_is_undefined() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var customRE = /./g;
        Object.defineProperty(customRE, Symbol.replace, { value: undefined });

        [
          // `ToString(/./g)` must use RegExp.prototype.toString, not Object.prototype.toString.
          String(customRE),
          // With @@replace undefined, replaceAll must proceed in string-search mode (searchString
          // is "/./g") so `$<` is treated literally.
          '------------------- /./g -------/./g'.replaceAll(customRE, 'a($<$<)')
        ].join("|")
      "#,
    )
    .unwrap();
  assert_eq!(
    as_utf8_lossy(&rt, value),
    "/./g|------------------- a($<$<) -------a($<$<)"
  );
}

#[test]
fn regexp_named_capture_replacement_uses_dollar_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          "abcd".replace(/(?<fst>.)(?<snd>.)/, "$<snd>$<fst>"),
          "abcd".replace(/(?<fst>.)(?<snd>.)/g, "$<snd>$<fst>")
        ].join("|")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "bacd|badc");
}
