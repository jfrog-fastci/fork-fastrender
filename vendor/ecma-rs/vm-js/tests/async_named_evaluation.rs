use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async/await tends to allocate more than simple synchronous scripts. Use a slightly larger heap
  // to avoid spurious OOMs in low-memory configurations.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_object_literal_sets_anonymous_function_name_for_property_value() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        const o = { a: function () {}, [await Promise.resolve("k")]: 0 };
        return o.a.name;
      }
      f().then(v => { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "a");
  Ok(())
}

#[test]
fn async_object_literal_infers_anonymous_class_name_across_await_in_extends() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        globalThis.saw = null;
        class Base {}
        const C = ({ a: class extends (await Promise.resolve(Base)) { static { globalThis.saw = this.name; } } }).a;
        return JSON.stringify([C.name, globalThis.saw]);
      }
      f().then(v => { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), r#"["a","a"]"#);
  Ok(())
}

#[test]
fn async_object_literal_infers_anonymous_class_name_across_await_in_computed_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        globalThis.saw = null;
        var obj = { a: class { static { globalThis.saw = this.name; } [await Promise.resolve("m")]() { return 2; } } };
        var C = obj.a;
        return JSON.stringify([globalThis.saw, C.name, new C().m()]);
      }
      f().then(v => { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), r#"["a","a",2]"#);
  Ok(())
}

#[test]
fn async_var_decl_sets_anonymous_function_name_even_if_later_declarator_awaits() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        let a = function () {}, b = await Promise.resolve(1);
        return a.name;
      }
      f().then(v => { out = v; });
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "a");
  Ok(())
}

