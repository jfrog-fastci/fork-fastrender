use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn json_stringify_serializes_objects_and_arrays_recursively() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o = {a: 1, b: [true, null, "x"], c: {d: 2}};
         JSON.stringify(o) === '{"a":1,"b":[true,null,"x"],"c":{"d":2}}'"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_calls_to_json() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = {x: 1, toJSON: function(k){ return 5; }}; JSON.stringify(o) === "5""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_supports_replacer_function() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o = {a: 1, b: 2, c: undefined};
         var s = JSON.stringify(o, function(k, v) {
           if (k === "b") return undefined;
           if (typeof v === "number") return v + 1;
           return v;
         });
         var a = [1, undefined];
         var s2 = JSON.stringify(a, function(k, v) { return v; });
         s === '{"a":2}' && s2 === '[1,null]'"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_supports_replacer_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var o = {a: 1, b: 2, c: 3};
         JSON.stringify(o, ["b", "a", "b", 1]) === '{"b":2,"a":1}'"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_supports_space_indentation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var s = JSON.stringify({a: 1, b: {c: 2}}, null, 2);
         s === '{\n  "a": 1,\n  "b": {\n    "c": 2\n  }\n}'"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_throws_on_cycles() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a = {}; a.self = a;
         var ok = false;
         try { JSON.stringify(a); } catch (e) { ok = e.name === "TypeError"; }
         ok"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn json_stringify_escapes_u2028_u2029_and_lone_surrogates() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"var ok = true;
       ok = ok && JSON.stringify("\u2028\u2029") === "\"\\u2028\\u2029\"";
       ok = ok && JSON.stringify("\uD834\uDD1E") === "\"\uD834\uDD1E\"";
       ok = ok && JSON.stringify("\uD834") === "\"\\ud834\"";
       ok"#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn json_stringify_throws_on_bigint() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"var ok = false;
       try { JSON.stringify(1n); } catch (e) { ok = e.name === "TypeError"; }
       ok"#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn json_has_symbol_to_string_tag() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"JSON[Symbol.toStringTag] === "JSON""#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
