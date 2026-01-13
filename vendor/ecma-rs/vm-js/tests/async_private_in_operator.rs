use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await tends to allocate more than simple synchronous scripts. Use a slightly larger heap
  // than the minimal 1MiB used by some unit tests to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_private_in_operator_await_rhs_true_false() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class C {
          #x;
          async hasAsync(o) { return #x in (await Promise.resolve(o)); }
        }
        class D { #x; }
        const c = new C();
        return (await c.hasAsync(c)) &&
          (await c.hasAsync(new C())) &&
          !(await c.hasAsync(new D())) &&
          !(await c.hasAsync({})) &&
          // Brand checks must not consult the prototype chain.
          !(await c.hasAsync(Object.create(c)));
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;

  // Async continuation must not run synchronously.
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "true");
  Ok(())
}

#[test]
fn async_private_in_operator_await_rhs_non_object_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out="";
      async function f(){
        class C {
          #x;
          async g(){
            try {
              return #x in (await Promise.resolve(1));
            } catch(e) {
              return e.name;
            }
          }
        }
        return await (new C()).g();
      }
      f().then(v=>out=v);
      out
    "#,
  )?;

  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "TypeError");
  Ok(())
}

#[test]
fn async_private_in_operator_await_rhs_proxy_returns_false() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class C {
          #x;
          async hasAsync(o) { return #x in (await Promise.resolve(o)); }
        }
        const c = new C();
        const proxy = new Proxy(c, {
          has() { throw new Error("has trap should not run"); },
          getOwnPropertyDescriptor() {
            throw new Error("getOwnPropertyDescriptor trap should not run");
          },
        });
        return await c.hasAsync(proxy);
      }
      f().then(v => out = String(v));
      out
    "#,
  )?;

  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "false");
  Ok(())
}
