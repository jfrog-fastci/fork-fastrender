use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn typed_array_set_invalid_numeric_index_converts_value() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      'use strict';

      // Keep this script lightweight: it runs under a small heap limit, and we only need to verify
      // that `ToNumber(value)` happens *before* numeric index validation for typed array element
      // sets.
      var u = new Uint8Array(0);

      var ok1 = false;
      try { u['-1'] = Symbol(); } catch(e){ ok1 = e && e.name === 'TypeError'; }
      var ok2 = false;
      try { u['1.5'] = Symbol(); } catch(e){ ok2 = e && e.name === 'TypeError'; }
      var ok3 = false;
      try { u['NaN'] = Symbol(); } catch(e){ ok3 = e && e.name === 'TypeError'; }
      var ok4 = false;
      try { u['Infinity'] = Symbol(); } catch(e){ ok4 = e && e.name === 'TypeError'; }
      var ok5 = false;
      try { u['-0'] = Symbol(); } catch(e){ ok5 = e && e.name === 'TypeError'; }
      var ok6 = false;
      try { u['-1'] = 1n; } catch(e){ ok6 = e && e.name === 'TypeError'; }

      ok1 && ok2 && ok3 && ok4 && ok5 && ok6
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_set_invalid_numeric_index_converts_value_on_detached_buffer() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Create `ab` + `u` in the global scope and return `ab` so we can detach it from Rust.
  let ab = rt.exec_script("var ab = new ArrayBuffer(4); var u = new Uint8Array(ab); ab")?;
  let Value::Object(ab) = ab else {
    panic!("expected ArrayBuffer object");
  };

  rt.heap_mut().detach_array_buffer(ab)?;

  let value = rt.exec_script(
    r#"
      (function(){
        'use strict';
        function throwsTypeError(fn){
          try { fn(); return false; }
          catch(e){ return e && e.name === 'TypeError'; }
        }
        return throwsTypeError(()=>{ u['-1'] = Symbol(); }) && throwsTypeError(()=>{ u['-1'] = 1n; });
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
