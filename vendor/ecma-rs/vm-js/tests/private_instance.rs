use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_instance_field_get_set() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #x = 1;
        getX() { return this.#x; }
        setX(v) { this.#x = v; }
      }
      const c = new C();
      c.getX() === 1 && (c.setX(2), c.getX() === 2)
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_instance_method_is_shared_and_named() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r##"
      class C {
        #m() { return 42; }
        getRef() { return this.#m; }
      }
      const a = new C();
      const b = new C();
      a.getRef() === b.getRef() && a.getRef().name === "#m" && a.getRef()() === 42
    "##,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_operator_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #x;
        static has(o) { return #x in o; }
      }
      C.has({}) === false && C.has(new C()) === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_rhs_non_object_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let caught = null;
      class C {
        #x;
        static test() {
          try { #x in 1; } catch (e) { caught = e; }
        }
      }
      C.test();
      caught instanceof TypeError
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn private_brand_check_cross_class_isolation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function classfactory() {
        return class {
          #x;
          static has(o) { return #x in o; }
        };
      }
      const C1 = classfactory();
      const C2 = classfactory();
      C1.has(new C1()) === true && C1.has(new C2()) === false
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn compiled_script_with_private_names_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      class C {
        #x = 1;
        getX() { return this.#x; }
      }
      (new C()).getX();
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn direct_eval_can_access_private_instance_field() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A {
        #x = 14;
        g() {
          return eval("this.#x");
        }
      }
      (new A()).g();
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(14.0));
}

#[test]
fn nested_class_can_reference_outer_private_name() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class A {
        #x = 5;
        makeB() {
          class B {
            #y = 1;
            getX(o) { return o.#x; }
          }
          return new B();
        }
      }
      const a = new A();
      const b = a.makeB();
      b.getX(a) === 5;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn nested_class_private_access_throws_type_error_on_wrong_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        class A {
          #x = 10;
          f() {
            class B {
              #y = 1;
              g() {
                // `#x` resolves to A's private name, but `this` is a B instance,
                // so the brand check fails and should throw a TypeError.
                return this.#x;
              }
            }
            this.y = new B();
          }
          constructor() { this.f(); }
          g() { return this.y.g(); }
        }
        const a = new A();
        try { a.g(); return false; } catch (e) { return e instanceof TypeError; }
      })();
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_instance_field_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #m = 'test262';
        static access(obj) { return obj?.#m; }
        static accessLen(obj) { return obj?.#m.length; }
        static accessLenParen(obj) { return (obj?.#m).length; }
      }

      let ok = true;
      ok = ok && C.access(new C()) === 'test262';
      ok = ok && C.access(null) === undefined;
      ok = ok && C.access(undefined) === undefined;

      let threw = false;
      try { C.access({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.access(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.access(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok = ok && C.accessLen(new C()) === 7;
      ok = ok && C.accessLen(null) === undefined;
      ok = ok && C.accessLen(undefined) === undefined;

      let threwLen = false;
      try { C.accessLen({}); } catch (e) { threwLen = e instanceof TypeError; }

      let threwParenNull = false;
      try { C.accessLenParen(null); } catch (e) { threwParenNull = e instanceof TypeError; }

      let threwParenUndef = false;
      try { C.accessLenParen(undefined); } catch (e) { threwParenUndef = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && threwLen && threwParenNull && threwParenUndef
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_instance_method_call_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;
      class C {
        #x = 'ok';
        #m() { return this.#x; }
        static call(obj) { return obj?.#m(++side); }
      }

      let ok = true;
      ok = ok && C.call(new C()) === 'ok' && side === 1;
      ok = ok && C.call(null) === undefined && side === 1;
      ok = ok && C.call(undefined) === undefined && side === 1;

      let threw = false;
      try { C.call({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.call(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.call(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_instance_accessor_get_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;
      class C {
        #v = 'ok';
        get #x() { side++; return this.#v; }
        static access(obj) { return obj?.#x; }
      }

      let ok = true;
      ok = ok && C.access(new C()) === 'ok' && side === 1;
      ok = ok && C.access(null) === undefined && side === 1;
      ok = ok && C.access(undefined) === undefined && side === 1;

      let threw = false;
      try { C.access({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.access(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.access(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_field_after_optional_chain_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #f = 'init';
        constructor(v) { this.#f = v; }
        method(o) { return o?.c.#f; }
        methodParen(o) { return (o?.c).#f; }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b';
      ok = ok && a.method(null) === undefined;
      ok = ok && a.method(undefined) === undefined;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok = ok && a.methodParen({ c: b }) === 'b';

      let threwParenNull = false;
      try { a.methodParen(null); } catch (e) { threwParenNull = e instanceof TypeError; }

      let threwParenUndef = false;
      try { a.methodParen(undefined); } catch (e) { threwParenUndef = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && threwParenNull && threwParenUndef
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_accessor_get_after_optional_chain_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;
      class C {
        #v = 'init';
        constructor(v) { this.#v = v; }
        get #x() { side++; return this.#v; }
        method(o) { return o?.c.#x; }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b' && side === 1;
      ok = ok && a.method(null) === undefined && side === 1;
      ok = ok && a.method(undefined) === undefined && side === 1;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_method_call_after_optional_chain_short_circuits_on_nullish_base() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;
      class C {
        #x = 'init';
        constructor(v) { this.#x = v; }
        #m() { return this.#x; }
        method(o) { return o?.c.#m(++side); }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b' && side === 1;
      ok = ok && a.method(null) === undefined && side === 1;
      ok = ok && a.method(undefined) === undefined && side === 1;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_field_with_two_optional_segments_short_circuits_on_nullish_intermediate() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      class C {
        #f = 'ok';
        method(o) { return o?.c?.#f; }
      }

      const c = new C();

      let ok = true;
      ok = ok && c.method({ c }) === 'ok';
      ok = ok && c.method(null) === undefined;
      ok = ok && c.method(undefined) === undefined;
      ok = ok && c.method({ c: null }) === undefined;
      ok = ok && c.method({ c: undefined }) === undefined;

      let threw = false;
      try { c.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { c.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { c.method({ c: new Proxy(c, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_method_call_with_two_optional_segments_short_circuits_and_skips_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;

      class C {
        #x;
        constructor(v) { this.#x = v; }
        #m(n) { return this.#x + n; }
        method(o) { return o?.c?.#m(++side); }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b1' && side === 1;
      ok = ok && a.method(null) === undefined && side === 1;
      ok = ok && a.method(undefined) === undefined && side === 1;
      ok = ok && a.method({ c: null }) === undefined && side === 1;
      ok = ok && a.method({ c: undefined }) === undefined && side === 1;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_private_tagged_template_short_circuits_and_parens_break_propagation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let side = 0;
      class C {
        #x = 'x';
        #tag(strings, v) { return strings[0] + v + strings[1] + this.#x; }
        static call(obj) { return obj?.#tag`hi${++side}`; }
        static callParen(obj) { return (obj?.#tag)`hi${++side}`; }
      }

      let ok = true;

      ok = ok && C.call(new C()) === "hi1x" && side === 1;
      ok = ok && C.call(null) === undefined && side === 1;
      ok = ok && C.call(undefined) === undefined && side === 1;

      let threw = false;
      try { C.call({}); } catch (e) { threw = e instanceof TypeError; }
      ok = ok && threw && side === 1;

      let threwProxy = false;
      try { C.call(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }
      ok = ok && threwProxy && side === 1;

      // Parentheses break optional-chain propagation and `this` binding:
      // - when base is nullish, tag call should proceed and evaluate substitutions, then throw.
      let beforeNull = side;
      let threwParenNull = false;
      try { C.callParen(null); } catch (e) { threwParenNull = e instanceof TypeError; }
      ok = ok && threwParenNull && side === beforeNull + 1;

      // - when base is an object, tag is retrieved, but called with `this = undefined`, so the
      //   private `this.#x` access inside `#tag` should throw *after* substitutions are evaluated.
      let beforeObj = side;
      let threwParenObj = false;
      try { C.callParen(new C()); } catch (e) { threwParenObj = e instanceof TypeError; }
      ok = ok && threwParenObj && side === beforeObj + 1;

      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn compiled_script_with_private_optional_chain_field_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      class C {
        #m = 'test262';
        static access(obj) { return obj?.#m; }
        static accessLen(obj) { return obj?.#m.length; }
        static accessLenParen(obj) { return (obj?.#m).length; }
      }

      let ok = true;
      ok = ok && C.access(new C()) === 'test262';
      ok = ok && C.access(null) === undefined;
      ok = ok && C.access(undefined) === undefined;

      let threw = false;
      try { C.access({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.access(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.access(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok = ok && C.accessLen(new C()) === 7;
      ok = ok && C.accessLen(null) === undefined;
      ok = ok && C.accessLen(undefined) === undefined;

      let threwLen = false;
      try { C.accessLen({}); } catch (e) { threwLen = e instanceof TypeError; }

      let threwParenNull = false;
      try { C.accessLenParen(null); } catch (e) { threwParenNull = e instanceof TypeError; }

      let threwParenUndef = false;
      try { C.accessLenParen(undefined); } catch (e) { threwParenUndef = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && threwLen && threwParenNull && threwParenUndef
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_private_optional_chain_method_call_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      let side = 0;
      class C {
        #x = 'ok';
        #m() { return this.#x; }
        static call(obj) { return obj?.#m(++side); }
      }

      let ok = true;
      ok = ok && C.call(new C()) === 'ok' && side === 1;
      ok = ok && C.call(null) === undefined && side === 1;
      ok = ok && C.call(undefined) === undefined && side === 1;

      let threw = false;
      try { C.call({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.call(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.call(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_private_optional_chain_accessor_get_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      let side = 0;
      class C {
        #v = 'ok';
        get #x() { side++; return this.#v; }
        static access(obj) { return obj?.#x; }
      }

      let ok = true;
      ok = ok && C.access(new C()) === 'ok' && side === 1;
      ok = ok && C.access(null) === undefined && side === 1;
      ok = ok && C.access(undefined) === undefined && side === 1;

      let threw = false;
      try { C.access({}); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { C.access(1); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { C.access(new Proxy(new C(), {})); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_private_field_after_optional_chain_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      class C {
        #f = 'init';
        constructor(v) { this.#f = v; }
        method(o) { return o?.c.#f; }
        methodParen(o) { return (o?.c).#f; }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b';
      ok = ok && a.method(null) === undefined;
      ok = ok && a.method(undefined) === undefined;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      ok = ok && a.methodParen({ c: b }) === 'b';

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      let threwParenNull = false;
      try { a.methodParen(null); } catch (e) { threwParenNull = e instanceof TypeError; }

      let threwParenUndef = false;
      try { a.methodParen(undefined); } catch (e) { threwParenUndef = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && threwParenNull && threwParenUndef
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_private_accessor_get_after_optional_chain_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      let side = 0;
      class C {
        #v = 'init';
        constructor(v) { this.#v = v; }
        get #x() { side++; return this.#v; }
        method(o) { return o?.c.#x; }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b' && side === 1;
      ok = ok && a.method(null) === undefined && side === 1;
      ok = ok && a.method(undefined) === undefined && side === 1;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_script_with_private_method_call_after_optional_chain_falls_back_and_executes() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "<inline>",
    r#"
      let side = 0;
      class C {
        #x = 'init';
        constructor(v) { this.#x = v; }
        #m() { return this.#x; }
        method(o) { return o?.c.#m(++side); }
      }

      const a = new C('a');
      const b = new C('b');

      let ok = true;
      ok = ok && a.method({ c: b }) === 'b' && side === 1;
      ok = ok && a.method(null) === undefined && side === 1;
      ok = ok && a.method(undefined) === undefined && side === 1;

      let threw = false;
      try { a.method({ c: {} }); } catch (e) { threw = e instanceof TypeError; }

      let threwPrim = false;
      try { a.method({ c: 1 }); } catch (e) { threwPrim = e instanceof TypeError; }

      let threwProxy = false;
      try { a.method({ c: new Proxy(b, {}) }); } catch (e) { threwProxy = e instanceof TypeError; }

      ok && threw && threwPrim && threwProxy && side === 1
    "#,
  )?;

  assert!(
    script.requires_ast_fallback,
    "compiled (HIR) executor does not support private names yet; compiled scripts must opt into AST fallback"
  );

  let value = rt.exec_compiled_script(script)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
