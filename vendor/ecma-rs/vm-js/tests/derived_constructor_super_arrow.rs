use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) script executor"
  );
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
fn derived_constructor_direct_eval_in_arrow_observes_initialized_this_and_super() -> Result<(), VmError> {
  assert_true_in_ast_and_compiled(
    r#"
      class B { m(){ return this; } }
      class C extends B {
        constructor() {
          let getThis = () => eval("this");
          let callSuper = () => eval("super.m()");
          let beforeThis, beforeSuper;
          try { getThis(); beforeThis = "no"; } catch (e) { beforeThis = e.name; }
          try { callSuper(); beforeSuper = "no"; } catch (e) { beforeSuper = e.name; }
          super();
          this.getThis = getThis;
          this.callSuper = callSuper;
          this.beforeThis = beforeThis;
          this.beforeSuper = beforeSuper;
        }
      }
      let o = new C();
      o.beforeThis === "ReferenceError" &&
        o.beforeSuper === "ReferenceError" &&
        o.getThis() === o &&
        o.callSuper() === o
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

#[test]
fn derived_constructor_async_arrow_super_calls_use_initialized_this() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      class B {
        m(v) {
          this._x = (this._x || 0) + v;
          return this._x;
        }
      }
      class C extends B {
        constructor() {
          super();
          this._x = 0;
          this.f = async () => {
            // `super.m(await ...)` exercises async call evaluation where the Super Reference receiver
            // must survive suspension during argument evaluation.
            let a = super.m(await Promise.resolve(1));
            // `super[await ...](await ...)` exercises computed super member calls that can suspend
            // both in the key and in the argument list.
            let b = super[await Promise.resolve("m")](await Promise.resolve(2));
            return String(a) + ":" + String(b) + ":" + String(this._x) + ":" + String(this instanceof C);
          };
        }
      }
      async function run() {
        let o = new C();
        return await o.f();
      }
      run().then(v => out = String(v));
    "#,
    "1:3:3:true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_call_with_await_initializes_this() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      class B {
        constructor(x) { this.x = x; }
      }
      class C extends B {
        constructor() {
          // Return an async arrow that captures the derived constructor `this` binding state.
          return async () => {
            // `super(await ...)` exercises async `super()` call argument evaluation + resumption.
            super(await Promise.resolve(1));
            return String(this.x) + ':' + String(this instanceof C);
          };
        }
      }
      async function run() {
        let f = new C();
        return await f();
      }
      run().then(v => out = String(v));
    "#,
    "1:true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_proxy_receiver_is_instance_across_await() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';

      var recv_get;
      var recv_set;
      var target = { x: 1 };
      var proxy = new Proxy(target, {
        get(t, p, r) {
          if (p === "x") recv_get = r;
          return Reflect.get(t, p, r);
        },
        set(t, p, v, r) {
          if (p === "x") recv_set = r;
          return Reflect.set(t, p, v, r);
        },
      });

      class B {}
      class C extends B {
        constructor() {
          // Defining an async arrow that uses `super` before calling `super()` is allowed. The arrow
          // captures the derived constructor `this` binding via a shared internal state cell, and
          // `super` operations must use the initialized `this` value as the Proxy receiver.
          let f = async () => {
            // `+=` performs a `get` (before the await) and a `set` (after resumption) on the same
            // Super Reference.
            super.x += await Promise.resolve(1);
            return recv_get === this && recv_set === this;
          };
          super();
          // Make `GetSuperBase()` return the Proxy object.
          Object.setPrototypeOf(C.prototype, proxy);
          this.f = f;
        }
      }

      async function run() {
        let o = new C();
        let ok = await o.f();
        return ok && recv_get === o && recv_set === o;
      }
      run().then(v => out = String(v));
    "#,
    "true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_computed_proxy_receiver_is_instance_across_await() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';

      var recv_get;
      var recv_set;
      var target = {
        get x() { return this._x; },
        set x(v) { this._x = v; },
      };
      var proxy = new Proxy(target, {
        get(t, p, r) {
          if (p === "x") recv_get = r;
          return Reflect.get(t, p, r);
        },
        set(t, p, v, r) {
          if (p === "x") recv_set = r;
          return Reflect.set(t, p, v, r);
        },
      });

      class B {}
      class C extends B {
        constructor() {
          let f = async () => {
            super[await Promise.resolve("x")]++;
            return recv_get === this && recv_set === this;
          };
          super();
          this._x = 1;
          // Make `GetSuperBase()` return the Proxy object.
          Object.setPrototypeOf(C.prototype, proxy);
          this.f = f;
        }
      }

      async function run() {
        let o = new C();
        let ok = await o.f();
        return ok && recv_get === o && recv_set === o && o._x === 2;
      }
      run().then(v => out = String(v));
    "#,
    "true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_calls_before_super_throw_before_await() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      async function run() {
        let log = '';
        class B {
          m(v) {
            this._x = (this._x || 0) + v;
            return this._x;
          }
        }

        class C extends B {
          constructor() {
            let keyThenable = { get then() { log += 'K'; return (resolve) => resolve('m'); } };
            let argThenable = { get then() { log += 'A'; return (resolve) => resolve(1); } };

            // Before `super()`: both calls must throw a ReferenceError *before* evaluating the
            // awaited key/argument expressions (so the thenables must not run).
            let p1 = (async () => {
              try { return super.m(await argThenable); } catch (e) { return e.name; }
            })();
            let p2 = (async () => {
              try { return super[await keyThenable](await argThenable); } catch (e) { return e.name; }
            })();

            super();
            this.p1 = p1;
            this.p2 = p2;
          }
        }

        let o = new C();
        let r1 = await o.p1;
        let r2 = await o.p2;
        return r1 + ',' + r2 + ':' + log;
      }
      run().then(v => out = String(v));
    "#,
    "ReferenceError,ReferenceError:",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_assignment_proxy_receiver_is_instance_across_await() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';

      var recv_set_1;
      var recv_set_2;
      var target = {
        get x() { return this._x; },
        set x(v) { this._x = v; },
      };
      var proxy = new Proxy(target, {
        set(t, p, v, r) {
          if (p === "x") {
            if (recv_set_1 === undefined) recv_set_1 = r;
            else recv_set_2 = r;
          }
          return Reflect.set(t, p, v, r);
        },
      });

      class B {}
      class C extends B {
        constructor() {
          let f = async () => {
            super.x = await Promise.resolve(1);
            super[await Promise.resolve("x")] = await Promise.resolve(2);
            return recv_set_1 === this && recv_set_2 === this && this._x === 2;
          };
          super();
          this._x = 0;
          Object.setPrototypeOf(C.prototype, proxy);
          this.f = f;
        }
      }

      async function run() {
        let o = new C();
        let ok = await o.f();
        return ok && recv_set_1 === o && recv_set_2 === o && o._x === 2;
      }
      run().then(v => out = String(v));
    "#,
    "true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_tagged_template_uses_initialized_this() -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      class B {
        tag(s, v) {
          this._x = v;
          return s[0] + String(v) + s[1];
        }
      }
      class C extends B {
        constructor() {
          super();
          this._x = 0;
          this.f = async () => {
            let a = super.tag`a${await Promise.resolve(1)}b`;
            let b = super[await Promise.resolve("tag")]`c${await Promise.resolve(2)}d`;
            return a + "," + b + ":" + String(this._x) + ":" + String(this instanceof C);
          };
        }
      }
      async function run() {
        let o = new C();
        return await o.f();
      }
      run().then(v => out = String(v));
    "#,
    "a1b,c2d:2:true",
  )?;
  Ok(())
}

#[test]
fn derived_constructor_async_arrow_super_tagged_template_before_super_throw_before_await(
) -> Result<(), VmError> {
  assert_async_out_in_ast_and_compiled(
    r#"
      var out = '';
      async function run() {
        let log = '';
        class B {
          tag(s, v) { return s[0] + String(v) + s[1]; }
        }

        class C extends B {
          constructor() {
            let keyThenable = { get then() { log += 'K'; return (resolve) => resolve('tag'); } };
            let subThenable = { get then() { log += 'S'; return (resolve) => resolve('x'); } };

            // Before `super()`: must throw a ReferenceError before awaiting any substitution or
            // computed-key expression.
            let p1 = (async () => {
              try { return super.tag`a${await subThenable}b`; } catch (e) { return e.name; }
            })();
            let p2 = (async () => {
              try { return super[await keyThenable]`a${await subThenable}b`; } catch (e) { return e.name; }
            })();

            super();
            this.p1 = p1;
            this.p2 = p2;
          }
        }

        let o = new C();
        let r1 = await o.p1;
        let r2 = await o.p2;
        return r1 + ',' + r2 + ':' + log;
      }
      run().then(v => out = String(v));
    "#,
    "ReferenceError,ReferenceError:",
  )?;
  Ok(())
}
