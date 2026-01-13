use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

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
