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

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn assert_async_out_in_ast_and_compiled(source: &str, expected: &str) -> Result<(), VmError> {
  // AST interpreter path.
  {
    let mut rt = new_runtime();
    rt.exec_script(source)?;

    // No microtasks run yet.
    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), expected);
  }

  // Compiled (HIR) script path.
  {
    let mut rt = new_runtime();
    exec_compiled(&mut rt, source)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), "");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let out = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out), expected);
  }

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

#[test]
fn derived_constructor_super_property_operations_use_initialized_this() -> Result<(), VmError> {
  assert_true_in_ast_and_compiled(
    r#"
      class B {
        constructor() { this.base = 1; }
        get x(){ return this._x; }
        set x(v){ this._x = v; }
        m(){ return this.base; }
      }

      class C extends B {
        constructor() {
          let keyEvaluated = false;
          let key = () => { keyEvaluated = true; return "x"; };

          let get = () => super.x;
          let getComputed = () => super[key()];
          let set = (v) => { super.x = v; };
          let setComputed = (v) => { super[key()] = v; };
          let callM = () => super.m();

          let beforeGet, beforeSet;
          let beforeComputedGet, beforeComputedSet;

          try { get(); beforeGet = "no"; } catch (e) { beforeGet = e.name; }
          try { set(1); beforeSet = "no"; } catch (e) { beforeSet = e.name; }

          keyEvaluated = false;
          try { getComputed(); beforeComputedGet = "no"; } catch (e) { beforeComputedGet = e.name; }
          let computedGetKeyBefore = keyEvaluated;

          keyEvaluated = false;
          try { setComputed(2); beforeComputedSet = "no"; } catch (e) { beforeComputedSet = e.name; }
          let computedSetKeyBefore = keyEvaluated;

          super();

          set(10);
          let afterGet = get();
          let afterThisX = this._x;

          keyEvaluated = false;
          setComputed(20);
          let afterComputedGet = getComputed();
          let afterThisX2 = this._x;
          let computedKeyAfter = keyEvaluated;

          let afterM = super.m();
          let afterArrowM = callM();

          this.ok =
            beforeGet === "ReferenceError" &&
            beforeSet === "ReferenceError" &&
            beforeComputedGet === "ReferenceError" &&
            beforeComputedSet === "ReferenceError" &&
            computedGetKeyBefore === false &&
            computedSetKeyBefore === false &&
            afterGet === 10 &&
            afterThisX === 10 &&
            afterComputedGet === 20 &&
            afterThisX2 === 20 &&
            computedKeyAfter === true &&
            afterM === 1 &&
            afterArrowM === 1;
        }
      }

      let o = new C();
      o.ok === true
    "#,
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_property_ops_use_initialized_this() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      class B {
        get x(){ return this._x; }
        set x(v){ this._x = v; }
      }
      class C extends B {
        constructor() {
          super();
          this.f = async () => {
            // `super.x = await ...` exercises async assignment with a Super Reference target.
            super.x = await Promise.resolve(1);
            // `super[await ...] = await ...` and `super[await ...]++` exercise super computed member
            // assignment + update resumption frames.
            super[await Promise.resolve("x")] = await Promise.resolve(2);
            super[await Promise.resolve("x")]++;
            return this._x;
          };
        }
      }
      async function run() {
        let o = new C();
        let v = await o.f();
        return String(v) + ":" + (v === 3);
      }
      run().then(v => out = String(v));
    "#,
    "3:true",
  )?;
  Ok(())
}
