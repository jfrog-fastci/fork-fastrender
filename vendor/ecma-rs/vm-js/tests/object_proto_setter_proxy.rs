use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_proto_setter_invokes_proxy_setprototypeof_trap() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const log = [];
      const newProto = {};

      const p = new Proxy({}, {
        setPrototypeOf(t, proto) {
          log.push(proto);
          return true;
        }
      });

      const desc = Object.getOwnPropertyDescriptor(Object.prototype, "__proto__");
      desc.set.call(p, newProto);

      log.length === 1 && log[0] === newProto
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_proto_setter_revoked_proxy_throws_typeerror() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const desc = Object.getOwnPropertyDescriptor(Object.prototype, "__proto__");
      const r = Proxy.revocable({}, {});
      r.revoke();

      let ok = false;
      try {
        desc.set.call(r.proxy, {});
      } catch (e) {
        ok = e instanceof TypeError;
      }
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_proto_setter_receiver_primitives_and_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const desc = Object.getOwnPropertyDescriptor(Object.prototype, "__proto__");

      const primitiveOk = desc.set.call(1, {}) === undefined;

      let nullOk = false;
      try {
        desc.set.call(null, {});
      } catch (e) {
        nullOk = e instanceof TypeError;
      }

      let undefOk = false;
      try {
        desc.set.call(undefined, {});
      } catch (e) {
        undefOk = e instanceof TypeError;
      }

      primitiveOk && nullOk && undefOk
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

