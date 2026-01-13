use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn compound_assignment_arithmetic_numbers_sub_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5; let r=(x-=2); x===3 && r===3"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_numbers_mul_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5; let r=(x*=2); x===10 && r===10"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_numbers_div_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5; let r=(x/=2); x===2.5 && r===2.5"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_numbers_rem_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5; let r=(x%=2); x===1 && r===1"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_sub_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5n; let r=(x-=2n); x===3n && r===3n"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_mul_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5n; let r=(x*=2n); x===10n && r===10n"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_div_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5n; let r=(x/=2n); x===2n && r===2n"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_rem_assign() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(r#"let x=5n; let r=(x%=2n); x===1n && r===1n"#)?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_number_mix_sub_assign_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let ok = true
          && (() => { let x=1n; try { x -= 1; return false; } catch(e) { return e.name === "TypeError" && x === 1n; } })()
          && (() => { let x=1; try { x -= 1n; return false; } catch(e) { return e.name === "TypeError" && x === 1; } })();
        ok
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_number_mix_mul_assign_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let ok = true
          && (() => { let x=1n; try { x *= 1; return false; } catch(e) { return e.name === "TypeError" && x === 1n; } })()
          && (() => { let x=1; try { x *= 1n; return false; } catch(e) { return e.name === "TypeError" && x === 1; } })();
        ok
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_number_mix_div_assign_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let ok = true
          && (() => { let x=1n; try { x /= 1; return false; } catch(e) { return e.name === "TypeError" && x === 1n; } })()
          && (() => { let x=1; try { x /= 1n; return false; } catch(e) { return e.name === "TypeError" && x === 1; } })();
        ok
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_number_mix_rem_assign_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let ok = true
          && (() => { let x=1n; try { x %= 1; return false; } catch(e) { return e.name === "TypeError" && x === 1n; } })()
          && (() => { let x=1; try { x %= 1n; return false; } catch(e) { return e.name === "TypeError" && x === 1; } })();
        ok
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_div_by_zero_throws_rangeerror() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let x=1n;
        let ok = false;
        try { x /= 0n; } catch(e) { ok = e.name === "RangeError"; }
        ok && x === 1n
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_bigint_rem_by_zero_throws_rangeerror() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let x=1n;
        let ok = false;
        try { x %= 0n; } catch(e) { ok = e.name === "RangeError"; }
        ok && x === 1n
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_eval_order_sub_assign_get_before_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let log=[];
        let o={ get x(){ log.push('get'); return 5; }, set x(v){ log.push('set:'+v); } };
        function obj() { log.push("obj"); return o; }
        function key() { log.push("key"); return "x"; }
        function rhs() { log.push("rhs"); return 2; }
        obj()[key()] -= rhs();
        log.join(',') === 'obj,key,get,rhs,set:3'
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_eval_order_mul_assign_get_before_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let log=[];
        let o={ get x(){ log.push('get'); return 5; }, set x(v){ log.push('set:'+v); } };
        function obj() { log.push("obj"); return o; }
        function key() { log.push("key"); return "x"; }
        function rhs() { log.push("rhs"); return 2; }
        obj()[key()] *= rhs();
        log.join(',') === 'obj,key,get,rhs,set:10'
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_eval_order_div_assign_get_before_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let log=[];
        let o={ get x(){ log.push('get'); return 5; }, set x(v){ log.push('set:'+v); } };
        function obj() { log.push("obj"); return o; }
        function key() { log.push("key"); return "x"; }
        function rhs() { log.push("rhs"); return 2; }
        obj()[key()] /= rhs();
        log.join(',') === 'obj,key,get,rhs,set:2.5'
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}

#[test]
fn compound_assignment_arithmetic_eval_order_rem_assign_get_before_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();
  assert_eq!(
    rt.exec_script(
      r#"
        let log=[];
        let o={ get x(){ log.push('get'); return 5; }, set x(v){ log.push('set:'+v); } };
        function obj() { log.push("obj"); return o; }
        function key() { log.push("key"); return "x"; }
        function rhs() { log.push("rhs"); return 2; }
        obj()[key()] %= rhs();
        log.join(',') === 'obj,key,get,rhs,set:1'
      "#,
    )?,
    Value::Bool(true)
  );
  Ok(())
}
