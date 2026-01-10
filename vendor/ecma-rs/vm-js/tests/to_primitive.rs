use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_as_utf8(rt: &JsRuntime, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string value, got {v:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn string_constructor_coerces_objects_via_to_primitive() {
  let mut rt = new_runtime();
  let v = rt.exec_script("String({})").unwrap();
  assert_eq!(value_as_utf8(&rt, v), "[object Object]");
}

#[test]
fn number_constructor_uses_ordinary_to_primitive_valueof_then_tostring() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      "(() => {\n\
        let order = 0;\n\
        const o = {\n\
          valueOf() { order = order * 10 + 1; return {}; },\n\
          toString() { order = order * 10 + 2; return '2'; },\n\
        };\n\
        const n = Number(o);\n\
        return n === 2 && order === 12;\n\
      })()",
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

#[test]
fn to_primitive_prefers_symbol_to_primitive_when_present() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      "(() => {\n\
        const sym = Symbol.toPrimitive;\n\
        const o = {};\n\
        o[sym] = function(hint) { return hint === 'string' ? 'ok' : 1; };\n\
        return String(o) === 'ok' && Number(o) === 1;\n\
      })()",
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

#[test]
fn to_primitive_throws_when_symbol_to_primitive_is_not_callable() {
  let mut rt = new_runtime();
  let ok = rt
    .exec_script(
      "(() => {\n\
        try {\n\
          String({ [Symbol.toPrimitive]: 123 });\n\
          return false;\n\
        } catch (e) {\n\
          return e && e.name === 'TypeError';\n\
        }\n\
      })()",
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}
