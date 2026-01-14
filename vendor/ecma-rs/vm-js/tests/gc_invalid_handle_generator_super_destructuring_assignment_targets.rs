use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during generator-mode destructuring so missing roots manifest as stale handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_super_computed_target_survives_gc_between_yields() -> Result<(), VmError>
{
  let mut rt = new_runtime_with_frequent_gc();

  // Exercise the generator destructuring assignment target pre-evaluation path for
  // `super[computed]`, ensuring all Super Reference components (receiver + base + key value) are
  // rooted correctly across:
  // - GC while suspended at the computed-key `yield`, and
  // - GC while suspended at a later default-value `yield`.
  //
  // This also stresses GC *between* yields (inside the default initializer) while the assignment
  // target is held in Rust locals.
  let value = rt.exec_script(
    r#"
      'use strict';

      function churn() {
        let junk = [];
        for (let i = 0; i < 200; i++) {
          junk.push(new Uint8Array(1024));
        }
        return junk.length;
      }

      class Base {
        set k(v) { this._k = v; }
      }

      class Derived extends Base {
        *g() {
          ({ a: super[(yield 0)] = (churn(), (yield 1)) } = {});
          return this._k;
        }
      }

      let it = (new Derived()).g();
      let r0 = it.next();
      // Stress GC while suspended after the computed-key `yield`.
      churn();
      let r1 = it.next("k");
      // Stress GC while suspended at the default-value `yield` (so the continuation is traced).
      churn();
      let r2 = it.next(42);

      r0.value === 0 && r0.done === false &&
      r1.value === 1 && r1.done === false &&
      r2.value === 42 && r2.done === true
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

