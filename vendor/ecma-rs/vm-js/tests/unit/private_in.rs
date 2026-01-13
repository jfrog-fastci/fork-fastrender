use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn private_in_brand_check_uses_own_property_and_rejects_proxies() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        static #x = 1;
        static get #y() { return 1; }
        static hasX(o) { return #x in o; }
        static hasY(o) { return #y in o; }
      }

      // A Proxy should fail the brand check without consulting any traps.
      const p = new Proxy(C, {
        has() { throw new Error("has trap should not run"); },
        getOwnPropertyDescriptor() { throw new Error("getOwnPropertyDescriptor trap should not run"); },
      });

      C.hasX(C) && C.hasY(C) &&
        !C.hasX({}) && !C.hasY({}) &&
        // Brand checks do not consult the prototype chain.
        !C.hasX(Object.create(C)) && !C.hasY(Object.create(C)) &&
        C.hasX(p) === false && C.hasY(p) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_in_throws_type_error_when_rhs_is_not_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        class C {
          static #x = 1;
          static hasX(o) { return #x in o; }
        }
        try {
          C.hasX(1);
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_in_supports_await_in_rhs_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      class C {
        static #x = 1;
        static async hasX(p) {
          return #x in await p;
        }
      }
      const proxy = new Proxy(C, {
        has() { throw new Error("has trap should not run"); },
        getOwnPropertyDescriptor() { throw new Error("getOwnPropertyDescriptor trap should not run"); },
      });

      C.hasX(Promise.resolve(C)).then(v => out += (v ? "t" : "f"));
      C.hasX(Promise.resolve(proxy)).then(v => out += (v ? "t" : "f"));
    "#,
  )?;

  // Before microtasks run, `out` is still empty.
  let out0 = rt.exec_script("out")?;
  assert_eq!(value_to_utf8(&rt, out0), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out1 = rt.exec_script("out")?;
  assert_eq!(value_to_utf8(&rt, out1), "tf");
  Ok(())
}
