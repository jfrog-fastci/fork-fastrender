use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn untagged_template_literal_interpolates_substitutions() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var x=1; `a${x}b` === "a1b""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tagged_template_literal_calls_tag_with_template_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function tag(a){ return a[0]; } tag`hi` === "hi""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tagged_template_literal_template_object_is_cached_per_site() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
function tag(a) { return a; }
function f() { return tag`hi`; }
var a1 = f();
var a2 = f();
a1 === a2
"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tagged_template_literal_raw_and_cooked_strings_are_distinct() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
function tag(a) { return a[0] === "\n" && a.raw[0] === "\\n"; }
tag`\n`
"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tagged_template_literal_invalid_escape_produces_undefined_cooked_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
function tag(a) { return a[0] === undefined && a.raw[0] === "\\1"; }
tag`\1`
"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tagged_template_literal_template_object_is_frozen() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
function tag(a) { return a; }
function f() { return tag`hi`; }
var t = f();

(function() {
  "use strict";
  var ok = true;

  try { t[0] = "x"; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
  try { t.raw[0] = "x"; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
  try { t.raw = []; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
  try { t.extra = 1; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
  try { t.length = 0; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }
  try { t.raw.length = 0; ok = false; } catch (e) { ok = ok && (e instanceof TypeError); }

  return ok;
})()
"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
