use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn assert_true_in_ast_and_compiled(source: &str) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(source)?;
  assert_eq!(value, Value::Bool(true));

  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, source)?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn derived_constructor_super_call_in_finally_arrow_initializes_this() -> Result<(), VmError> {
  assert_true_in_ast_and_compiled(
    r#"
      class B { constructor() { this.x = 1; } }
      class C extends B {
        constructor() {
          var f = () => super();
          try { return; } finally { f(); this.after = this.x; }
        }
      }
      var o = new C();
      o.after === 1 && o instanceof C
    "#,
  )?;
  Ok(())
}

#[test]
fn derived_constructor_super_call_in_catch_finally_arrow_initializes_this() -> Result<(), VmError> {
  assert_true_in_ast_and_compiled(
    r#"
      class B { constructor() { this.x = 2; } }
      class C extends B {
        constructor() {
          var f = () => super();
          try { throw null; } catch (e) { return; } finally { f(); this.after = this.x; }
        }
      }
      var o = new C();
      o.after === 2 && o instanceof C
    "#,
  )?;
  Ok(())
}

#[test]
fn derived_constructor_super_call_arrow_can_escape_constructor() -> Result<(), VmError> {
  assert_true_in_ast_and_compiled(
    r#"
      class B { constructor() { this.fromB = 1; } }
      class C extends B {
        constructor() {
          var f = () => { super(); this.after = 2; return this; };
          return f;
        }
      }

      var f = new C();
      var o = f();
      var second;
      try { f(); second = "no"; } catch (e) { second = e.name; }
      o.after === 2 && o.fromB === 1 && o instanceof C && second === "ReferenceError"
    "#,
  )?;
  Ok(())
}
