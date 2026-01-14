use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn let_initializer_only_infers_name_for_anonymous_function_definitions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          let cover = (function () {});
          let xCover = (0, function () {});
          return cover.name === "cover" && xCover.name !== "xCover";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn let_initializer_does_not_override_class_static_name_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          let cls = class {};
          let xCls = class X {};
          let xCls2 = class { static name() {} };
          return cls.name === "cls" && xCls.name !== "xCls" && typeof xCls2.name === "function";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_default_infers_name_only_for_anonymous_function_definitions() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          let [cover = (function () {}), xCover = (0, function () {})] = [];
          return cover.name === "cover" && xCover.name !== "xCover";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

