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
      (function(){
        'use strict';

        let u = new Uint8Array(0);
        function throwsTypeError(fn){
          try { fn(); return false; }
          catch(e){ return e && e.name === 'TypeError'; }
        }

        return (
          throwsTypeError(()=>{ u['-1'] = Symbol(); }) &&
          throwsTypeError(()=>{ u['1.5'] = Symbol(); }) &&
          throwsTypeError(()=>{ u['NaN'] = Symbol(); }) &&
          throwsTypeError(()=>{ u['Infinity'] = Symbol(); }) &&
          throwsTypeError(()=>{ u['-0'] = Symbol(); }) &&
          throwsTypeError(()=>{ u['-1'] = 1n; })
        );
      })()
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

