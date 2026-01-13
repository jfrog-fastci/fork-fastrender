use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during error construction so missing roots manifest as stale handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn range_error_creation_roots_property_keys_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // Derived from test262:
  // `built-ins/Array/prototype/slice/create-proxied-array-invalid-len.js#strict`.
  let value = rt.exec_script(
    r#"
      'use strict';

      // Create enough live heap pressure that subsequent allocations (including error
      // construction) trigger GC under the small `gc_threshold`.
      var junk = new Uint8Array(100000);

      var array = [];
      var maxLength = Math.pow(2, 32);
      var callCount = 0;
      var proxy = new Proxy(array, {
        get: function(_, name) {
          if (name === 'length') {
            return maxLength;
          }
          return array[name];
        },
        set: function() {
          callCount += 1;
          return true;
        }
      });

      try {
        Array.prototype.slice.call(proxy);
        false;
      } catch (e) {
        e instanceof RangeError && callCount === 0;
      }
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
