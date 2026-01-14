use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_class_evaluation_preserves_method_function_kinds() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B {}
        class C extends (await Promise.resolve(B)) {
          *g() { yield 1; }
          async m() { return 1; }
          static *sg() { yield 1; }
          static async sm() { return 1; }
        }

        var ok_gen_tag = Object.prototype.toString.call(C.prototype.g) === "[object GeneratorFunction]";
        var ok_gen_proto = Object.getPrototypeOf(C.prototype.g) === Object.getPrototypeOf(function*(){});
        var ok_gen_instance_proto = Object.prototype.toString.call(C.prototype.g.prototype) === "[object Generator]";

        var ok_async_tag = Object.prototype.toString.call(C.prototype.m) === "[object AsyncFunction]";
        var ok_async_proto = Object.getPrototypeOf(C.prototype.m) === Object.getPrototypeOf(async function(){});

        var ok_sgen_tag = Object.prototype.toString.call(C.sg) === "[object GeneratorFunction]";
        var ok_sgen_proto = Object.getPrototypeOf(C.sg) === Object.getPrototypeOf(function*(){});
        var ok_sgen_instance_proto = Object.prototype.toString.call(C.sg.prototype) === "[object Generator]";

        var ok_sasync_tag = Object.prototype.toString.call(C.sm) === "[object AsyncFunction]";
        var ok_sasync_proto = Object.getPrototypeOf(C.sm) === Object.getPrototypeOf(async function(){});

        return (
          ok_gen_tag && ok_gen_proto && ok_gen_instance_proto &&
          ok_async_tag && ok_async_proto &&
          ok_sgen_tag && ok_sgen_proto && ok_sgen_instance_proto &&
          ok_sasync_tag && ok_sasync_proto
        );
      }
      f().then(v => out = String(v));
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "true");
  Ok(())
}

