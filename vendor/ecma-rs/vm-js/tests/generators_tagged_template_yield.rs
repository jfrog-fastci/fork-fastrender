use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_tagged_template_yield_in_substitution() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function tag(strings, x) { return strings[0] + x + strings[1]; }
      function* g() { return tag`a${yield 1}b`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("X");
      r1.value === 1 && r1.done === false &&
      r2.value === "aXb" && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_tagged_template_yield_in_tag_expression() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function tag(strings) { return strings[0]; }
      function* g() { return (yield tag)`x`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(tag);
      r1.value === tag && r1.done === false &&
      r2.value === "x" && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_tagged_template_yield_in_member_base_and_substitution() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      "use strict";
      var obj = {
        prefix: "P",
        tag: function(strings, x) {
          return this.prefix + strings[0] + x + strings[1];
        }
      };
      function* g() { return (yield obj).tag`a${yield 1}b`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(obj);
      var r3 = it.next("Z");
      r1.value === obj && r1.done === false &&
      r2.value === 1 && r2.done === false &&
      r3.value === "PaZb" && r3.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_tagged_template_yield_in_computed_member_key() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      "use strict";
      var obj = {
        prefix: "P",
        m: function(strings) { return this.prefix + strings[0]; }
      };
      function* g() { return obj[(yield 1)]`x`; }
      var it = g();
      var r1 = it.next();
      var r2 = it.next("m");
      r1.value === 1 && r1.done === false &&
      r2.value === "Px" && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_tagged_template_yield_in_super_member_tag() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class Base {
        tag(strings, x) { return this.prefix + strings[0] + x + strings[1]; }
      }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        *g() { return super.tag`a${yield 1}b`; }
      }
      var it = new Derived().g();
      var r1 = it.next();
      var r2 = it.next("Z");
      r1.value === 1 && r1.done === false &&
      r2.value === "PaZb" && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_tagged_template_yield_in_super_computed_member_key() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      class Base {
        tag(strings) { return this.prefix + strings[0]; }
      }
      class Derived extends Base {
        constructor() { super(); this.prefix = "P"; }
        *g() { return super[(yield 1)]`x`; }
      }
      var it = new Derived().g();
      var r1 = it.next();
      var r2 = it.next("tag");
      r1.value === 1 && r1.done === false &&
      r2.value === "Px" && r2.done === true
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
