use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_super_assignment_and_update_with_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class B {}
      B.prototype.x = 1;

      class D extends B {
        *assignDot() {
          super.y = yield 1;
          return this.y;
        }

        *assignComputed() {
          super["y"] = yield 2;
          return this.y;
        }

        *updateComputed() {
          const old = super[(yield "x")]++;
          return [old, this.x, B.prototype.x];
        }
      }

      const d = new D();

      const it1 = d.assignDot();
      const r1a = it1.next();
      const r1b = it1.next(10);
      const ok1 =
        r1a.value === 1 &&
        r1a.done === false &&
        r1b.value === 10 &&
        r1b.done === true &&
        d.y === 10;

      const it2 = d.assignComputed();
      const r2a = it2.next();
      const r2b = it2.next(20);
      const ok2 =
        r2a.value === 2 &&
        r2a.done === false &&
        r2b.value === 20 &&
        r2b.done === true &&
        d.y === 20;

      const it3 = d.updateComputed();
      const r3a = it3.next();
      const r3b = it3.next("x");
      const ok3 =
        r3a.value === "x" &&
        r3a.done === false &&
        Array.isArray(r3b.value) &&
        r3b.value[0] === 1 &&
        r3b.value[1] === 2 &&
        r3b.value[2] === 1 &&
        r3b.done === true &&
        d.x === 2 &&
        B.prototype.x === 1;

      ok1 && ok2 && ok3;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

