use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_method_access_on_proxy_instance_does_not_invoke_get_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        let arr = [];

        class ProxyBase {
          constructor() {
            return new Proxy(this, {
              get(obj, prop) {
                arr.push(prop);
                return obj[prop];
              }
            });
          }
        }

        class Test extends ProxyBase {
          #f() { return 3; }
          method() { return this.#f(); }
        }

        let t = new Test();
        let r = t.method();

        r === 3 && arr.length === 1 && arr[0] === 'method'
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

