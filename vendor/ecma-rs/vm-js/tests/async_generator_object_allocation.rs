use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

#[test]
fn async_generator_function_call_allocates_async_generator_object_with_correct_prototype(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Basic async generator allocation + prototype selection.
  let value = rt.exec_script(
    r#"
async function* g() {}
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

  // `%AsyncGeneratorPrototype%.next` should recognize the async generator object and return a
  // Promise resolving to an iterator result object (rather than throwing a TypeError for an
  // incompatible receiver).
  let value = rt.exec_script(
    r#"
async function* g() {}
const gen = g();
var out = "";
const p = Object.getPrototypeOf(g).prototype.next.call(gen);
p.then(r => { out = String(r.done) + ":" + String(r.value); });
out
"#,
  )?;
  let Value::String(s) = value else {
    panic!("expected string result, got {value:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  let Value::String(s) = value else {
    panic!("expected string result, got {value:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "true:undefined");

  Ok(())
}

