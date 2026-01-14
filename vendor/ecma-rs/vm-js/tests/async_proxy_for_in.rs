use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await and Proxy trap calls both allocate Promise/job state; keep the heap limit large
  // enough to avoid spurious `VmError::OutOfMemory` failures as builtin coverage grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_for_in_calls_proxy_ownkeys_and_gopd_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const log = [];
        const target = { a: 1 };
        const handler = {
          ownKeys(t) {
            log.push("ownKeys");
            return ["a"];
          },
          getOwnPropertyDescriptor(t, k) {
            log.push("gopd:" + k);
            return { value: 1, writable: true, enumerable: true, configurable: true };
          }
        };
        const p = new Proxy(target, handler);

        var s = "";
        for (var k in p) {
          s += k;
          await 0;
        }
        out = log.join(",") + "|" + s;
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ownKeys,gopd:a|a");
  Ok(())
}

#[test]
fn async_for_in_observes_proxy_getprototypeof_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const log = [];
        const proto = { p: 1 };
        const target = {};
        const handler = {
          getPrototypeOf(t) {
            log.push("getPrototypeOf");
            return proto;
          }
        };
        const p = new Proxy(target, handler);

        var s = "";
        for (var k in p) {
          s += k;
          await 0;
        }
        out = log.join(",") + "|" + s;
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "getPrototypeOf|p");
  Ok(())
}

