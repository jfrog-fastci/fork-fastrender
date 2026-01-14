use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Some of these tests use eval; give them a slightly larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<super_property_semantics>", source)?;
  rt.exec_compiled_script(script)
}

const RECEIVER_BINDING: &str = r#"
  var receiver = { marker: "receiver" };

  var parentObj = {
    getThis: function() { return this; },
    get This() { return this; },
  };
  var obj = {
    method() {
      var a = super.getThis() === receiver;
      var b = super.This === receiver;
      var c = super["getThis"]() === receiver;
      var d = super["This"] === receiver;
      return [a, b, c, d].join(",");
    }
  };
  Object.setPrototypeOf(obj, parentObj);
  var objRes = obj.method.call(receiver);

  class Parent {
    getThis() { return this; }
    get This() { return this; }
  }
  class C extends Parent {
    method() {
      var a = super.getThis() === receiver;
      var b = super.This === receiver;
      var c = super["getThis"]() === receiver;
      var d = super["This"] === receiver;
      return [a, b, c, d].join(",");
    }
  }
  var clsRes = C.prototype.method.call(receiver);

  class StaticParent {
    static getThis() { return this; }
    static get This() { return this; }
  }
  class S extends StaticParent {
    static method() {
      var a = super.getThis() === receiver;
      var b = super.This === receiver;
      var c = super["getThis"]() === receiver;
      var d = super["This"] === receiver;
      return [a, b, c, d].join(",");
    }
  }
  var staticRes = S.method.call(receiver);

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_receiver_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(RECEIVER_BINDING)?;
  assert_value_is_utf8(
    &rt,
    value,
    "true,true,true,true;true,true,true,true;true,true,true,true",
  );
  Ok(())
}

#[test]
fn super_property_receiver_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, RECEIVER_BINDING)?;
  assert_value_is_utf8(
    &rt,
    value,
    "true,true,true,true;true,true,true,true;true,true,true,true",
  );
  Ok(())
}

const PUTVALUE_STRICT_SLOPPY: &str = r#"
  function has(o, p) { return Object.prototype.hasOwnProperty.call(o, p); }

  // Sloppy object literal method (non-strict super reference): silent failure.
  var sloppyDot = {
    method() {
      super.x = 8;
      Object.freeze(this);
      var threw = "no";
      try { super.y = 9; } catch (e) { threw = e.name; }
      return threw + "," + has(this, "x") + "," + has(this, "y");
    }
  };
  var sloppyExpr = {
    method() {
      super["x"] = 8;
      Object.freeze(this);
      var threw = "no";
      try { super["y"] = 9; } catch (e) { threw = e.name; }
      return threw + "," + has(this, "x") + "," + has(this, "y");
    }
  };
  var sloppyRes = sloppyDot.method() + ";" + sloppyExpr.method();

  // Strict object literal method: TypeError.
  var strictRes = (function () {
    "use strict";
    var strictDot = {
      method() {
        super.x = 8;
        Object.freeze(this);
        try { super.y = 9; return "no"; }
        catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
      }
    };
    var strictExpr = {
      method() {
        super["x"] = 8;
        Object.freeze(this);
        try { super["y"] = 9; return "no"; }
        catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
      }
    };
    return strictDot.method() + ";" + strictExpr.method();
  })();

  // Class methods are always strict.
  class K {
    dot() {
      super.x = 8;
      Object.freeze(this);
      try { super.y = 9; return "no"; }
      catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
    }
    expr() {
      super["x"] = 8;
      Object.freeze(this);
      try { super["y"] = 9; return "no"; }
      catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
    }
    static dot() {
      super.x = 8;
      Object.freeze(this);
      try { super.y = 9; return "no"; }
      catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
    }
    static expr() {
      super["x"] = 8;
      Object.freeze(this);
      try { super["y"] = 9; return "no"; }
      catch (e) { return e.name + "," + has(this, "x") + "," + has(this, "y"); }
    }
  }
  var k1 = {};
  var k2 = {};
  var k3 = {};
  var k4 = {};
  var classRes = [
    K.prototype.dot.call(k1),
    K.prototype.expr.call(k2),
    K.dot.call(k3),
    K.expr.call(k4),
  ].join(";");

  sloppyRes + "|" + strictRes + "|" + classRes
"#;

#[test]
fn super_property_putvalue_strict_vs_sloppy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(PUTVALUE_STRICT_SLOPPY)?;
  assert_value_is_utf8(
    &rt,
    value,
    "no,true,false;no,true,false|TypeError,true,false;TypeError,true,false|TypeError,true,false;TypeError,true,false;TypeError,true,false;TypeError,true,false",
  );
  Ok(())
}

#[test]
fn super_property_putvalue_strict_vs_sloppy_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, PUTVALUE_STRICT_SLOPPY)?;
  assert_value_is_utf8(
    &rt,
    value,
    "no,true,false;no,true,false|TypeError,true,false;TypeError,true,false|TypeError,true,false;TypeError,true,false;TypeError,true,false;TypeError,true,false",
  );
  Ok(())
}

const NULL_PROTO_SUPER_BASE: &str = r#"
  var receiver = {};

  var obj = {
    method() {
      var dot;
      var expr;
      try { super.x; dot = "no"; } catch (e) { dot = e.name; }
      try { super["x"]; expr = "no"; } catch (e) { expr = e.name; }
      return dot + "," + expr;
    }
  };
  Object.setPrototypeOf(obj, null);
  var objRes = obj.method.call(receiver);

  class C extends null {
    method() {
      var dot;
      var expr;
      try { super.x; dot = "no"; } catch (e) { dot = e.name; }
      try { super["x"]; expr = "no"; } catch (e) { expr = e.name; }
      return dot + "," + expr;
    }
  }
  var clsRes = C.prototype.method.call(receiver);

  objRes + ";" + clsRes
"#;

#[test]
fn super_property_null_prototype_super_base_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(NULL_PROTO_SUPER_BASE)?;
  assert_value_is_utf8(&rt, value, "TypeError,TypeError;TypeError,TypeError");
  Ok(())
}

#[test]
fn super_property_null_prototype_super_base_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, NULL_PROTO_SUPER_BASE)?;
  assert_value_is_utf8(&rt, value, "TypeError,TypeError;TypeError,TypeError");
  Ok(())
}

const ARROW_INHERITS_SUPER: &str = r#"
  var A = { fromA: "a", fromB: "a" };
  var B = { fromB: "b" };
  Object.setPrototypeOf(B, A);

  var obj = {
    fromA: "c",
    fromB: "c",
    method() {
      return [
        (() => super.fromA)(),
        (() => super.fromB)(),
        (() => super["fromA"])(),
        (() => super["fromB"])(),
      ].join(",");
    }
  };
  Object.setPrototypeOf(obj, B);
  var objRes = obj.method();

  class CA {}
  class CB extends CA {}
  class CC extends CB {
    method() {
      return [
        (() => super.fromA)(),
        (() => super.fromB)(),
        (() => super["fromA"])(),
        (() => super["fromB"])(),
      ].join(",");
    }
  }
  CA.prototype.fromA = "a";
  CA.prototype.fromB = "a";
  CB.prototype.fromB = "b";
  CC.prototype.fromA = "c";
  CC.prototype.fromB = "c";
  var clsRes = CC.prototype.method();

  objRes + ";" + clsRes
"#;

#[test]
fn super_property_arrow_inherits_super_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(ARROW_INHERITS_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b");
  Ok(())
}

#[test]
fn super_property_arrow_inherits_super_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, ARROW_INHERITS_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b");
  Ok(())
}

const DIRECT_EVAL_SEES_SUPER: &str = r#"
  var A = { fromA: "a", fromB: "a" };
  var B = { fromB: "b" };
  Object.setPrototypeOf(B, A);

  var obj = {
    fromA: "c",
    fromB: "c",
    method() {
      return [
        eval("super.fromA"),
        eval("super.fromB"),
        eval("super['fromA']"),
        eval("super['fromB']"),
      ].join(",");
    }
  };
  Object.setPrototypeOf(obj, B);
  var objRes = obj.method();

  class CA {}
  class CB extends CA {}
  class CC extends CB {
    method() {
      return [
        eval("super.fromA"),
        eval("super.fromB"),
        eval("super['fromA']"),
        eval("super['fromB']"),
      ].join(",");
    }
  }
  CA.prototype.fromA = "a";
  CA.prototype.fromB = "a";
  CB.prototype.fromB = "b";
  CC.prototype.fromA = "c";
  CC.prototype.fromB = "c";
  var clsRes = CC.prototype.method();

  objRes + ";" + clsRes
"#;

#[test]
fn super_property_direct_eval_resolves_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(DIRECT_EVAL_SEES_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b");
  Ok(())
}

#[test]
fn super_property_direct_eval_resolves_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, DIRECT_EVAL_SEES_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b");
  Ok(())
}

const COMPUTED_KEY_ERRORS: &str = r#"
  var thrown = {};
  function thrower() { throw thrown; }
  var badToString = { toString: function() { throw thrown; } };

  var obj = {
    method() {
      var e1;
      var e2;
      var e3;
      try { super[thrower()]; e1 = "no"; } catch (e) { e1 = (e === thrown); }
      try { super[badToString]; e2 = "no"; } catch (e) { e2 = (e === thrown); }
      try { super[test262unresolvable]; e3 = "no"; } catch (e) { e3 = e.name; }
      return [e1, e2, e3].join(",");
    }
  };
  var objRes = obj.method();

  class C {
    method() {
      var e1;
      var e2;
      var e3;
      try { super[thrower()]; e1 = "no"; } catch (e) { e1 = (e === thrown); }
      try { super[badToString]; e2 = "no"; } catch (e) { e2 = (e === thrown); }
      try { super[test262unresolvable]; e3 = "no"; } catch (e) { e3 = e.name; }
      return [e1, e2, e3].join(",");
    }
  }
  var clsRes = C.prototype.method();

  objRes + ";" + clsRes
"#;

#[test]
fn super_property_computed_key_error_propagation() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(COMPUTED_KEY_ERRORS)?;
  assert_value_is_utf8(&rt, value, "true,true,ReferenceError;true,true,ReferenceError");
  Ok(())
}

#[test]
fn super_property_computed_key_error_propagation_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, COMPUTED_KEY_ERRORS)?;
  assert_value_is_utf8(&rt, value, "true,true,ReferenceError;true,true,ReferenceError");
  Ok(())
}

