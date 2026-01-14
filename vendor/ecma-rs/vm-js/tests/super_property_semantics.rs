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
    static method() {
      var dot;
      var expr;
      try { super.x; dot = "no"; } catch (e) { dot = e.name; }
      try { super["x"]; expr = "no"; } catch (e) { expr = e.name; }
      return dot + "," + expr;
    }
  }
  var clsRes = C.prototype.method.call(receiver);
  var staticRes = C.method();

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_null_prototype_super_base_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(NULL_PROTO_SUPER_BASE)?;
  assert_value_is_utf8(
    &rt,
    value,
    "TypeError,TypeError;TypeError,TypeError;TypeError,TypeError",
  );
  Ok(())
}

#[test]
fn super_property_null_prototype_super_base_throws_type_error_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, NULL_PROTO_SUPER_BASE)?;
  assert_value_is_utf8(
    &rt,
    value,
    "TypeError,TypeError;TypeError,TypeError;TypeError,TypeError",
  );
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

  class SA {}
  class SB extends SA {}
  class SC extends SB {
    static method() {
      return [
        (() => super.fromA)(),
        (() => super.fromB)(),
        (() => super["fromA"])(),
        (() => super["fromB"])(),
      ].join(",");
    }
  }
  SA.fromA = "a";
  SA.fromB = "a";
  SB.fromB = "b";
  SC.fromA = "c";
  SC.fromB = "c";
  var staticRes = SC.method();

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_arrow_inherits_super_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(ARROW_INHERITS_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
  Ok(())
}

#[test]
fn super_property_arrow_inherits_super_binding_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, ARROW_INHERITS_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
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

  class SA {}
  class SB extends SA {}
  class SC extends SB {
    static method() {
      return [
        eval("super.fromA"),
        eval("super.fromB"),
        eval("super['fromA']"),
        eval("super['fromB']"),
      ].join(",");
    }
  }
  SA.fromA = "a";
  SA.fromB = "a";
  SB.fromB = "b";
  SC.fromA = "c";
  SC.fromB = "c";
  var staticRes = SC.method();

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_direct_eval_resolves_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(DIRECT_EVAL_SEES_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
  Ok(())
}

#[test]
fn super_property_direct_eval_resolves_super_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, DIRECT_EVAL_SEES_SUPER)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
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
    static method() {
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
  var staticRes = C.method();

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_computed_key_error_propagation() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(COMPUTED_KEY_ERRORS)?;
  assert_value_is_utf8(
    &rt,
    value,
    "true,true,ReferenceError;true,true,ReferenceError;true,true,ReferenceError",
  );
  Ok(())
}

#[test]
fn super_property_computed_key_error_propagation_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, COMPUTED_KEY_ERRORS)?;
  assert_value_is_utf8(
    &rt,
    value,
    "true,true,ReferenceError;true,true,ReferenceError;true,true,ReferenceError",
  );
  Ok(())
}

const VALUE_LOOKUP: &str = r#"
  var A = { fromA: "a", fromB: "a" };
  var B = { fromB: "b" };
  Object.setPrototypeOf(B, A);

  var obj = {
    fromA: "c",
    fromB: "c",
    method() {
      return [
        super.fromA,
        super.fromB,
        super["fromA"],
        super["fromB"],
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
        super.fromA,
        super.fromB,
        super["fromA"],
        super["fromB"],
      ].join(",");
    }
  }
  CA.prototype.fromA = "a";
  CA.prototype.fromB = "a";
  CB.prototype.fromB = "b";
  CC.prototype.fromA = "c";
  CC.prototype.fromB = "c";
  var clsRes = CC.prototype.method();

  class SA {}
  class SB extends SA {}
  class SC extends SB {
    static method() {
      return [
        super.fromA,
        super.fromB,
        super["fromA"],
        super["fromB"],
      ].join(",");
    }
  }
  SA.fromA = "a";
  SA.fromB = "a";
  SB.fromB = "b";
  SC.fromA = "c";
  SC.fromB = "c";
  var staticRes = SC.method();

  objRes + ";" + clsRes + ";" + staticRes
"#;

#[test]
fn super_property_value_lookup() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(VALUE_LOOKUP)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
  Ok(())
}

#[test]
fn super_property_value_lookup_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, VALUE_LOOKUP)?;
  assert_value_is_utf8(&rt, value, "a,b,a,b;a,b,a,b;a,b,a,b");
  Ok(())
}

const THIS_UNINITIALIZED: &str = r#"
  var dotErr;
  class Dot extends Object {
    constructor() {
      try { super.x; } catch (e) { dotErr = e.name; }
    }
  }
  try { new Dot(); } catch (_) {}

  var exprErr;
  var side = 0;
  class Expr extends Object {
    constructor() {
      try { super[(side = 1, "x")]; } catch (e) { exprErr = e.name; }
    }
  }
  try { new Expr(); } catch (_) {}

  dotErr + ";" + exprErr + ":" + side
"#;

#[test]
fn super_property_uninitialized_this_throws_reference_error_before_key_eval() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(THIS_UNINITIALIZED)?;
  assert_value_is_utf8(&rt, value, "ReferenceError;ReferenceError:0");
  Ok(())
}

const THIS_UNINITIALIZED_PUTVALUE_CONTEXTS: &str = r#"
  function errName(e) { return typeof e === "string" ? e : e.name; }

  function dotAssign() {
    var after = "no";
    class C extends Object {
      constructor() {
        super.x = 0;
        after = "yes";
      }
    }
    var err = "no";
    try { new C(); } catch (e) { err = errName(e); }
    return err + "," + after + ",0";
  }

  function exprAssign() {
    var baseCalls = 0;
    class Base {
      constructor() {
        baseCalls++;
        throw "base";
      }
    }
    var after = "no";
    class Derived extends Base {
      constructor() {
        super[super()] = 0;
        after = "yes";
      }
    }
    var err = "no";
    try { new Derived(); } catch (e) { err = errName(e); }
    return err + "," + after + "," + baseCalls;
  }

  function exprCompoundAssign() {
    var baseCalls = 0;
    class Base {
      constructor() {
        baseCalls++;
        throw "base";
      }
    }
    var after = "no";
    class Derived extends Base {
      constructor() {
        super[super()] += 0;
        after = "yes";
      }
    }
    var err = "no";
    try { new Derived(); } catch (e) { err = errName(e); }
    return err + "," + after + "," + baseCalls;
  }

  function exprIncrement() {
    var baseCalls = 0;
    class Base {
      constructor() {
        baseCalls++;
        throw "base";
      }
    }
    var after = "no";
    class Derived extends Base {
      constructor() {
        super[super()]++;
        after = "yes";
      }
    }
    var err = "no";
    try { new Derived(); } catch (e) { err = errName(e); }
    return err + "," + after + "," + baseCalls;
  }

  dotAssign() + ";" + exprAssign() + ";" + exprCompoundAssign() + ";" + exprIncrement()
"#;

#[test]
fn super_property_uninitialized_this_throws_reference_error_before_key_eval_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, THIS_UNINITIALIZED)?;
  assert_value_is_utf8(&rt, value, "ReferenceError;ReferenceError:0");
  Ok(())
}

const THIS_UNINITIALIZED_GETVALUE_SUPER_CALL: &str = r#"
  function errName(e) { return typeof e === "string" ? e : e.name; }

  var baseCalls = 0;
  class Base {
    constructor() {
      baseCalls++;
      throw "base";
    }
  }

  class Derived extends Base {
    constructor() {
      return super[super()];
    }
  }

  var err = "no";
  try { new Derived(); } catch (e) { err = errName(e); }
  err + "," + baseCalls
"#;

#[test]
fn super_property_uninitialized_this_getvalue_does_not_evaluate_super_call() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(THIS_UNINITIALIZED_GETVALUE_SUPER_CALL)?;
  assert_value_is_utf8(&rt, value, "ReferenceError,0");
  Ok(())
}

#[test]
fn super_property_uninitialized_this_getvalue_does_not_evaluate_super_call_compiled(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, THIS_UNINITIALIZED_GETVALUE_SUPER_CALL)?;
  assert_value_is_utf8(&rt, value, "ReferenceError,0");
  Ok(())
}

const ACCESSORS_SEE_SUPER: &str = r#"
  class Base {
    method() { return this.marker; }
    get x() { return this.marker; }
    static method() { return this.marker; }
    static get x() { return this.marker; }
  }

  class Derived extends Base {
    get y() {
      return [super.method(), super.x, super["method"](), super["x"]].join(",");
    }
    set y(v) {
      this.out = [super.method(), super.x, super["method"](), super["x"], v].join(",");
    }
    static get y() {
      return [super.method(), super.x, super["method"](), super["x"]].join(",");
    }
    static set y(v) {
      this.out = [super.method(), super.x, super["method"](), super["x"], v].join(",");
    }
  }

  var inst = new Derived();
  inst.marker = "inst";
  var instGet = inst.y;
  inst.y = "v";
  var instSet = inst.out;

  Derived.marker = "stat";
  var statGet = Derived.y;
  Derived.y = "w";
  var statSet = Derived.out;

  instGet + ";" + instSet + ";" + statGet + ";" + statSet
"#;

#[test]
fn super_property_in_accessors() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(ACCESSORS_SEE_SUPER)?;
  assert_value_is_utf8(
    &rt,
    value,
    "inst,inst,inst,inst;inst,inst,inst,inst,v;stat,stat,stat,stat;stat,stat,stat,stat,w",
  );
  Ok(())
}

#[test]
fn super_property_in_accessors_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, ACCESSORS_SEE_SUPER)?;
  assert_value_is_utf8(
    &rt,
    value,
    "inst,inst,inst,inst;inst,inst,inst,inst,v;stat,stat,stat,stat;stat,stat,stat,stat,w",
  );
  Ok(())
}

const OBJECT_LITERAL_COMPUTED_PROPERTY_NAMES: &str = r#"
  function ID(x) { return x; }

  var protoGet = { m() { return " proto m"; } };
  var objMethods = {
    ["a"]() { return "a" + super.m(); },
    [ID("b")]() { return "b" + super.m(); },
    [0]() { return "0" + super.m(); },
    [ID(1)]() { return "1" + super.m(); },
  };
  Object.setPrototypeOf(objMethods, protoGet);
  var methodsRes = [objMethods.a(), objMethods.b(), objMethods[0](), objMethods[1]()].join(",");

  var objGetters = {
    get ["a"]() { return "a" + super.m(); },
    get [ID("b")]() { return "b" + super.m(); },
    get [0]() { return "0" + super.m(); },
    get [ID(1)]() { return "1" + super.m(); },
  };
  Object.setPrototypeOf(objGetters, protoGet);
  var gettersRes = [objGetters.a, objGetters.b, objGetters[0], objGetters[1]].join(",");

  var value = "";
  var protoSet = { m(name, v) { value = name + " " + v; } };
  var objSetters = {
    set ["a"](v) { super.m("a", v); },
    set [ID("b")](v) { super.m("b", v); },
    set [0](v) { super.m("0", v); },
    set [ID(1)](v) { super.m("1", v); },
  };
  Object.setPrototypeOf(objSetters, protoSet);
  objSetters.a = 2; var a = value;
  objSetters.b = 3; var b = value;
  objSetters[0] = 4; var c = value;
  objSetters[1] = 5; var d = value;
  var settersRes = [a, b, c, d].join(",");

  methodsRes + ";" + gettersRes + ";" + settersRes
"#;

#[test]
fn super_property_object_literal_computed_property_names() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(OBJECT_LITERAL_COMPUTED_PROPERTY_NAMES)?;
  assert_value_is_utf8(
    &rt,
    value,
    "a proto m,b proto m,0 proto m,1 proto m;a proto m,b proto m,0 proto m,1 proto m;a 2,b 3,0 4,1 5",
  );
  Ok(())
}

#[test]
fn super_property_object_literal_computed_property_names_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, OBJECT_LITERAL_COMPUTED_PROPERTY_NAMES)?;
  assert_value_is_utf8(
    &rt,
    value,
    "a proto m,b proto m,0 proto m,1 proto m;a proto m,b proto m,0 proto m,1 proto m;a 2,b 3,0 4,1 5",
  );
  Ok(())
}

#[test]
fn super_property_uninitialized_this_putvalue_does_not_evaluate_expr() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(THIS_UNINITIALIZED_PUTVALUE_CONTEXTS)?;
  assert_value_is_utf8(
    &rt,
    value,
    "ReferenceError,no,0;ReferenceError,no,0;ReferenceError,no,0;ReferenceError,no,0",
  );
  Ok(())
}

#[test]
fn super_property_uninitialized_this_putvalue_does_not_evaluate_expr_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, THIS_UNINITIALIZED_PUTVALUE_CONTEXTS)?;
  assert_value_is_utf8(
    &rt,
    value,
    "ReferenceError,no,0;ReferenceError,no,0;ReferenceError,no,0;ReferenceError,no,0",
  );
  Ok(())
}

const GETSUPERBASE_BEFORE_TOPROPERTYKEY: &str = r#"
  var proto = { p: "ok" };
  var proto2 = { p: "bad" };
  var obj = {
    __proto__: proto,
    m() { return super[key]; }
  };
  var key = {
    toString() {
      Object.setPrototypeOf(obj, proto2);
      return "p";
    }
  };
  var getValueRes = obj.m();

  var putValueRes = "unset";
  var proto3 = { set p(v) { putValueRes = "ok"; } };
  var proto4 = { set p(v) { putValueRes = "bad"; } };
  var obj2 = {
    __proto__: proto3,
    m() { super[key2] = 10; }
  };
  var key2 = {
    toString() {
      Object.setPrototypeOf(obj2, proto4);
      return "p";
    }
  };
  obj2.m();

  var proto5 = { p: 1 };
  var proto6 = { p: -1 };
  var obj3 = {
    __proto__: proto5,
    m() { return super[key3] += 1; }
  };
  var key3 = {
    toString() {
      Object.setPrototypeOf(obj3, proto6);
      return "p";
    }
  };
  var compoundRes = obj3.m();

  var proto7 = { p: 1 };
  var proto8 = { p: -1 };
  var obj4 = {
    __proto__: proto7,
    m() { return ++super[key4]; }
  };
  var key4 = {
    toString() {
      Object.setPrototypeOf(obj4, proto8);
      return "p";
    }
  };
  var incRes = obj4.m();

  getValueRes + ";" + putValueRes + ";" + compoundRes + ";" + incRes
"#;

#[test]
fn super_property_getsuperbase_before_topropertykey() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(GETSUPERBASE_BEFORE_TOPROPERTYKEY)?;
  assert_value_is_utf8(&rt, value, "ok;ok;2;2");
  Ok(())
}

#[test]
fn super_property_getsuperbase_before_topropertykey_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, GETSUPERBASE_BEFORE_TOPROPERTYKEY)?;
  assert_value_is_utf8(&rt, value, "ok;ok;2;2");
  Ok(())
}

const POISONED_UNDERSCORE_PROTO: &str = r#"
  Object.defineProperty(Object.prototype, "__proto__", {
    get: function() { throw "should not be called"; },
  });

  var obj = {
    superExpression() {
      return super["CONSTRUCTOR".toLowerCase()];
    },
    superIdentifierName() {
      return super.toString();
    },
  };

  var ok1 = obj.superExpression() === Object;
  var ok2 = obj.superIdentifierName() === "[object Object]";
  ok1 && ok2 ? "ok" : "bad"
"#;

#[test]
fn super_property_poisoned_underscore_proto_does_not_trigger_getter() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(POISONED_UNDERSCORE_PROTO)?;
  assert_value_is_utf8(&rt, value, "ok");
  Ok(())
}

#[test]
fn super_property_poisoned_underscore_proto_does_not_trigger_getter_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, POISONED_UNDERSCORE_PROTO)?;
  assert_value_is_utf8(&rt, value, "ok");
  Ok(())
}
