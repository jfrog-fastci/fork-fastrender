use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promises/async-await can allocate; give the tests a bit of headroom.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_super_tagged_template_computed_key_can_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";

      class B {
        static tag(strings) { return strings[0]; }
      }

      class D extends B {
        static async f() {
          // Ensure the async tagged-template evaluator can handle a Super Reference tag where the
          // computed key expression suspends.
          return super[await Promise.resolve("tag")]`x`;
        }
      }

      try {
        let p = D.f();
        p.then(v => out = "fulfilled:" + String(v))
         .catch(e => out = "rejected:" + e.name + ":" + e.message);
      } catch (e) {
        out = "threw:" + e.name + ":" + e.message;
      }
    "#,
  )?;

  // No microtasks yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "fulfilled:x");
  Ok(())
}

#[test]
fn async_super_tagged_template_member_substitution_can_await() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";

      class B {
        static tag(strings, v) { return strings[0] + v; }
      }

      class D extends B {
        static async f() {
          // The tag is a Super Reference member access, while the template substitution suspends.
          return super.tag`x${await Promise.resolve("y")}`;
        }
      }

      try {
        let p = D.f();
        p.then(v => out = "fulfilled:" + String(v))
         .catch(e => out = "rejected:" + e.name + ":" + e.message);
      } catch (e) {
        out = "threw:" + e.name + ":" + e.message;
      }
    "#,
  )?;

  // No microtasks yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "fulfilled:xy");
  Ok(())
}

#[test]
fn async_super_tagged_template_before_super_does_not_eval_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      var key_eval = 0;

      class B {
        tag(strings) { return strings[0]; }
      }

      class D extends B {
        constructor() {
          const f = async () => {
            // `super[expr]` requires an initialized `this` binding. In derived constructors before
            // `super()` returns, `GetThisBinding` must throw before evaluating the computed key
            // expression (including any `await` inside it).
            return super[await (key_eval++, "tag")]`x`;
          };

          f().then(v => out = "fulfilled:" + String(v))
           .catch(e => out = e.name + ":" + e.message);

          super();
        }
      }

      new D();
    "#,
  )?;

  let key_eval = rt.exec_script("key_eval")?;
  assert_eq!(key_eval, Value::Number(0.0));

  // No microtasks yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let key_eval = rt.exec_script("key_eval")?;
  assert_eq!(key_eval, Value::Number(0.0));

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "ReferenceError:Must call super constructor in derived class before accessing 'this'"
  );
  Ok(())
}

