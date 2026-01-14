use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_member_assignment_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 1};
        (yield obj).a = 2;
        return obj.a;
      }
      var it = g();
      var r1 = it.next();
      var a1 = r1.value.a;
      var r2 = it.next(r1.value);
      r1.done === false &&
      a1 === 1 &&
      r2.done === true &&
      r2.value === 2 &&
      r1.value.a === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_computed_member_assignment_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 1};
        (yield obj)['a'] = 3;
        return obj.a;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_compound_assignment_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 1};
        return (yield obj).a += 5;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r1.done === false &&
      r2.done === true &&
      r2.value === 6 &&
      r1.value.a === 6
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_update_expression_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 1};
        var old = (yield obj).a++;
        return old === 1 && obj.a === 2;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(r1.value);
      r2.done === true && r2.value === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prefix_update_expression_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 1};
        var out = ++(yield obj).a;
        return out === 2 && obj.a === 2;
      }
      var it = g();
      var r1 = it.next();
      var a1 = r1.value.a;
      var r2 = it.next(r1.value);
      r1.done === false &&
      a1 === 1 &&
      r2.done === true &&
      r2.value === true &&
      r1.value.a === 2
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_prefix_decrement_update_expression_yield_in_base() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function* g(){
        var obj = {a: 2};
        var out = --(yield obj).a;
        return out === 1 && obj.a === 1;
      }
      var it = g();
      var r1 = it.next();
      var a1 = r1.value.a;
      var r2 = it.next(r1.value);
      r1.done === false &&
      a1 === 2 &&
      r2.done === true &&
      r2.value === true &&
      r1.value.a === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
