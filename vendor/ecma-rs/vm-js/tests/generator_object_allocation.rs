use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

#[test]
fn generator_function_call_allocates_generator_object_with_correct_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Basic generator allocation + prototype selection.
  let value = rt.exec_script(
    r#"
function* g() {}

let ok = true;

const gen1 = g();
ok = ok && typeof gen1 === "object";
ok = ok && Object.getPrototypeOf(gen1) === g.prototype;

g.prototype = null;
const gen2 = g();
ok = ok && Object.getPrototypeOf(gen2) === Object.getPrototypeOf(g).prototype;

ok;
"#,
  )?;
  assert_eq!(value, Value::Bool(true));

  // `%GeneratorPrototype%.next` should recognize the generator object and return an iterator result
  // object (rather than throwing a TypeError for an incompatible receiver).
  let value = rt.exec_script(
    r#"
function* g() {}
const gen = g();
const r = Object.getPrototypeOf(g).prototype.next.call(gen);
typeof r === "object" && r.value === undefined && r.done === true;
"#,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
