use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_script_returns_true_in_interpreter_and_compiled(source: &str) {
  // AST interpreter.
  {
    let mut rt = new_runtime();
    let value = rt.exec_script(source).unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  // Compiled HIR executor.
  {
    let mut rt = new_runtime();
    let script = CompiledScript::compile_script(rt.heap_mut(), "<inline>", source).unwrap();
    assert!(
      !script.requires_ast_fallback,
      "test script should execute via compiled (HIR) script executor"
    );
    let value = rt.exec_compiled_script(script).unwrap();
    assert_eq!(value, Value::Bool(true));
  }
}

#[test]
fn destructuring_assignment_to_super_properties_sets_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        Base.prototype.a = 0;
        Base.prototype.b = 0;

        class Derived extends Base {
          m() {
            [super.a, super['b']] = [1, 2];
            return this.hasOwnProperty('a') && this.a === 1
              && this.hasOwnProperty('b') && this.b === 2
              && Base.prototype.a === 0 && Base.prototype.b === 0;
          }
        }

        (new Derived()).m()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_to_super_property_sets_receiver() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        Base.prototype.a = 0;

        class Derived extends Base {
          m() {
            ({ x: super.a } = { x: 3 });
            return this.hasOwnProperty('a') && this.a === 3
              && Base.prototype.a === 0;
          }
        }

        (new Derived()).m()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_default_initializer_can_access_super_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { get a() { return 42; } }
        class Derived extends Base {
          m(o) {
            let { x = super.a } = o;
            return x === 42;
          }
        }
        (new Derived()).m({})
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_default_initializer_arrow_captures_home_object_for_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base { get a() { return 7; } }
        class Derived extends Base {
          m(o) {
            let { f = () => super.a } = o;
            return f() === 7;
          }
        }
        (new Derived()).m({})
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_to_super_computed_does_not_evaluate_key_before_super_in_derived_constructor() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          let side = 0;
          class Base {}
          class Derived extends Base {
            constructor() {
              [super[(side = 1, "m")]] = [1];
            }
          }
          try { new Derived(); return false; }
          catch (e) { return side === 0 && e.name === "ReferenceError"; }
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_to_super_computed_in_arrow_uses_initialized_this_and_does_not_evaluate_key_before_super(
) {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          let log = [];
          let side = 0;

          class Base {
            set m(v) {
              log.push('set:' + v + ':' + (this instanceof Derived));
              this._m = v;
            }
            get m() { return this._m; }
          }

          class Derived extends Base {
            constructor() {
              // Arrow captures the derived-constructor `this` state cell.
              let f = (v) => { [super[(side += 1, 'm')]] = [v]; return super.m; };

              let errName;
              let errMsg;
              try { f(1); } catch (e) { errName = e.name; errMsg = e.message; }

              super();
              this.v = f(2);
              this.side = side;
              this.errName = errName;
              this.errMsg = errMsg;
            }
          }

          let d = new Derived();
          return d.v === 2 &&
            d.side === 1 &&
            d.errName === 'ReferenceError' &&
            d.errMsg === "Must call super constructor in derived class before accessing 'this'" &&
            log.join(',') === 'set:2:true'
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_to_super_computed_evaluates_key_expression_before_getsuperbase() {
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      (() => {
        let log = [];
        let proto1 = { set p(v) { log.push("p1"); } };
        let proto2 = { set p(v) { log.push("p2"); } };

        let obj = {
          __proto__: proto1,
          m() {
            // `SuperProperty : super [ Expression ]` evaluates the key expression to a value
            // before `GetSuperBase`. Prototype mutations during key evaluation are observable.
            [super[(Object.setPrototypeOf(obj, proto2), "p")]] = [1];
          }
        };

        obj.m();
        return log.join(",") === "p2";
      })()
    "#,
  );
}

#[test]
fn destructuring_assignment_to_super_computed_getsuperbase_is_observed_before_topropertykey() {
  assert_script_returns_true_in_interpreter_and_compiled(
    r#"
      (() => {
        let log = [];
        let proto1 = { set p(v) { log.push("p1"); } };
        let proto2 = { set p(v) { log.push("p2"); } };

        let obj = {
          __proto__: proto1,
          m() {
            [super[key]] = [1];
          }
        };

        let key = {
          toString() {
            // `GetSuperBase` must be observed before `ToPropertyKey`, so prototype mutation during
            // key coercion does not affect the resolved super base.
            Object.setPrototypeOf(obj, proto2);
            return "p";
          }
        };

        obj.m();
        return log.join(",") === "p1";
      })()
    "#,
  );
}
