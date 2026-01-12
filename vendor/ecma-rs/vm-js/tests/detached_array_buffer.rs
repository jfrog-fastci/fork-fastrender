use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn detached_array_buffer_and_uint8_array_accessors_and_slice() {
  let mut rt = new_runtime();

  // Create the objects in JS so `ab` and `u` are available as globals for subsequent assertions.
  let ab_value = rt
    .exec_script("var ab = new ArrayBuffer(8); var u = new Uint8Array(ab, 2, 2); ab")
    .unwrap();
  let Value::Object(ab) = ab_value else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap.detach_array_buffer(ab).unwrap();

  let ok = rt
    .exec_script(
      r#"
        var ok = true;

        ok = ok && ab.byteLength === 0;
        var threw = false;
        try { ab.slice(0, 1); } catch (e) { threw = e.name === "TypeError"; }
        ok = ok && threw;

        ok = ok && u.length === 0 && u.byteLength === 0 && u.byteOffset === 0;
        threw = false;
        try { u.slice(0, 1); } catch (e) { threw = e.name === "TypeError"; }
        ok = ok && threw;

        ok;
      "#,
    )
    .unwrap();
  assert_eq!(ok, Value::Bool(true));
}

