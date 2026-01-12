use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn string_code_points() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var poop = "\uD83D\uDCA9";
      String.fromCodePoint(0x61) === "a" &&
      String.fromCodePoint(0x1F4A9) === poop &&
      String.fromCodePoint(0xD800).charCodeAt(0) === 0xD800 &&
      String.fromCodePoint(NaN).charCodeAt(0) === 0 &&
      (function () {
        try { String.fromCodePoint(0x110000); return false; }
        catch (e) { return e.name === "RangeError"; }
      })() &&
      (function () {
        try { String.fromCodePoint(Infinity); return false; }
        catch (e) { return e.name === "RangeError"; }
      })() &&
      poop.codePointAt(0) === 0x1F4A9 &&
      poop.codePointAt(1) === 0xDCA9 &&
      "abc".codePointAt(99) === undefined &&
      "abc".codePointAt(-1) === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_at() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var s = "A\uD83D\uDCA9B";
      var poop = "\uD83D\uDCA9";
      s.at(1) === poop &&
      s.at(-1) === "B" &&
      s.at(99) === undefined &&
      s.at(-99) === undefined &&
      poop.at(1).charCodeAt(0) === 0xDCA9
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_padding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      "abc".padStart(5) === "  abc" &&
      "abc".padEnd(5) === "abc  " &&
      "abc".padStart(6, "0") === "000abc" &&
      "abc".padEnd(6, "0") === "abc000" &&
      "abc".padStart(5, "012") === "01abc" &&
      "abc".padEnd(5, "012") === "abc01" &&
      "abc".padStart(5, "") === "abc" &&
      "abc".padEnd(5, "") === "abc" &&
      "abc".padStart(3, "0") === "abc" &&
      "abc".padEnd(3, "0") === "abc"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_raw() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      String.raw({ raw: ["a", "b", "c"] }, 1, 2) === "a1b2c" &&
      String.raw({ raw: ["a", "b", "c"] }, 1) === "a1bc" &&
      String.raw({ raw: [1, 2, 3] }, "x", "y") === "1x2y3" &&
      String.raw({ raw: [] }, "x") === "" &&
      String.raw`hi${1}` === "hi1" &&
      (function () {
        try { String.raw(); return false; }
        catch (e) { return e.name === "TypeError"; }
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
