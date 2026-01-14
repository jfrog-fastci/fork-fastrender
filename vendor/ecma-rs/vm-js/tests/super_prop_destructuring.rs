use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
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
