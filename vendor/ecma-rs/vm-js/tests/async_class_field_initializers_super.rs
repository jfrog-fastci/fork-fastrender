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
fn async_class_super_in_field_initializers_repairs_snippet_spans() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B { m() { return 1; } }
        class D extends (await Promise.resolve(B)) {
          y = (() => super.m())();
          v = (
            // a
            (() => super.m())
          )();
          u = (
            (() => "http://" + super.m())
          )
          ();
        }
        var d = new D();
        return d.y === 1 && d.v === 1 && d.u === "http://1";
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

#[test]
fn async_class_field_initializer_eval_can_access_super_property() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B { m() { return 1; } }
        class D extends (await Promise.resolve(B)) {
          x = eval("super.m()");
        }
        try {
          return String((new D()).x === 1);
        } catch (e) {
          return e.name;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "true");

  Ok(())
}
