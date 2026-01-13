use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn generator_yield_in_private_assignment_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      class C {
        #x = 1;
        *g(v) {
          this.#x = (yield v);
          return this.#x;
        }
      }
      const c = new C();
      const it = c.g(10);
      const r1 = it.next();
      if (r1.value !== 10 || r1.done !== false) throw new Error("bad first yield");
      const r2 = it.next(42);
      if (r2.value !== 42 || r2.done !== true) throw new Error("bad assignment result");
      "ok"
    "#,
  )?;
  assert_eq!(value_to_utf8(&rt, value), "ok");
  Ok(())
}

#[test]
fn generator_yield_in_private_update_base() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      class C {
        #x = 1;
        getX() { return this.#x; }
        *g() {
          return (yield this).#x++;
        }
      }
      const c = new C();
      const it = c.g();
      const r1 = it.next();
      if (r1.value !== c || r1.done !== false) throw new Error("bad yielded this");
      const r2 = it.next(r1.value);
      if (r2.value !== 1 || r2.done !== true) throw new Error("bad update result");
      if (c.getX() !== 2) throw new Error("bad final value");
      "ok"
    "#,
  )?;
  assert_eq!(value_to_utf8(&rt, value), "ok");
  Ok(())
}
