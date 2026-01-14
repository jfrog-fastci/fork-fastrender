use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests use Promises/async-await; give them a slightly larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  rt.exec_compiled_script(script)
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

// Regression tests for async `super` property assignment/update when the derived constructor `this`
// binding is captured as a heap-owned `DerivedConstructorState` (e.g. by an async arrow function
// created before `super()`).

const SOURCE_BEFORE_SUPER: &str = r#"
  var out = "";
  class B {
    get x() { return this._x; }
    set x(v) { this._x = v; }
  }
  class D extends B {
    constructor() {
      const f = async () => {
        super.x = await Promise.resolve(1);
      };
      f().then(() => out = "fulfilled").catch(e => out = e.name + ":" + e.message);
      super();
    }
  }
  new D();
"#;

#[test]
fn async_super_property_before_super_in_async_arrow_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(SOURCE_BEFORE_SUPER)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "ReferenceError:Must call super constructor in derived class before accessing 'this'"
  );
  Ok(())
}

#[test]
fn async_super_property_before_super_in_async_arrow_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  exec_compiled(&mut rt, SOURCE_BEFORE_SUPER)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "ReferenceError:Must call super constructor in derived class before accessing 'this'"
  );
  Ok(())
}

const SOURCE_COMPUTED_BEFORE_SUPER_KEY_ORDER: &str = r#"
  var out = "";
  var key_eval = 0;
  class B {}
  class D extends B {
    constructor() {
      const f = async () => {
        super[await (key_eval++, "x")] = 1;
      };
      f().then(() => out = "fulfilled").catch(e => out = e.name + ":" + e.message);
      super();
    }
  }
  new D();
"#;

#[test]
fn async_super_computed_property_before_super_does_not_eval_key_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(SOURCE_COMPUTED_BEFORE_SUPER_KEY_ORDER)?;

  let key_eval = rt.exec_script("key_eval")?;
  assert_eq!(key_eval, Value::Number(0.0));

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "ReferenceError:Must call super constructor in derived class before accessing 'this'"
  );
  Ok(())
}

const SOURCE_SUPER_PROPERTY_ASSIGNMENT: &str = r#"
  var out = "";
  class B {}
  B.prototype.x = 1;
  class D extends B {
    constructor() {
      // Create the async arrow before `super()` so it captures the derived-constructor `this`
      // binding via `DerivedConstructorState`.
      const f = async () => {
        super.x = await Promise.resolve(2);
        return String(this.x);
      };
      super();
      f().then(v => out = v).catch(e => out = e.name + ":" + e.message);
    }
  }
  new D();
"#;

#[test]
fn async_super_property_assignment_in_derived_constructor_arrow_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(SOURCE_SUPER_PROPERTY_ASSIGNMENT)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2");
  Ok(())
}

#[test]
fn async_super_property_assignment_in_derived_constructor_arrow_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  exec_compiled(&mut rt, SOURCE_SUPER_PROPERTY_ASSIGNMENT)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2");
  Ok(())
}

const SOURCE_SUPER_COMPUTED_PROPERTY_ASSIGNMENT: &str = r#"
  var out = "";
  class B {}
  B.prototype.x = 1;
  class D extends B {
    constructor() {
      const f = async () => {
        super[await Promise.resolve("x")] = await Promise.resolve(5);
        return String(this.x);
      };
      super();
      f().then(v => out = v).catch(e => out = e.name + ":" + e.message);
    }
  }
  new D();
"#;

#[test]
fn async_super_computed_property_assignment_in_derived_constructor_arrow_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(SOURCE_SUPER_COMPUTED_PROPERTY_ASSIGNMENT)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "5");
  Ok(())
}

#[test]
fn async_super_computed_property_assignment_in_derived_constructor_arrow_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  exec_compiled(&mut rt, SOURCE_SUPER_COMPUTED_PROPERTY_ASSIGNMENT)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "5");
  Ok(())
}

const SOURCE_SUPER_COMPUTED_PROPERTY_UPDATE: &str = r#"
  var out = "";
  class B {}
  B.prototype.x = 1;
  class D extends B {
    constructor() {
      const f = async () => {
        const old = super[await Promise.resolve("x")]++;
        return old + "," + this.x;
      };
      super();
      f().then(v => out = String(v)).catch(e => out = e.name + ":" + e.message);
    }
  }
  new D();
"#;

#[test]
fn async_super_computed_property_update_in_derived_constructor_arrow_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(SOURCE_SUPER_COMPUTED_PROPERTY_UPDATE)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1,2");
  Ok(())
}

#[test]
fn async_super_computed_property_update_in_derived_constructor_arrow_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  exec_compiled(&mut rt, SOURCE_SUPER_COMPUTED_PROPERTY_UPDATE)?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1,2");
  Ok(())
}

