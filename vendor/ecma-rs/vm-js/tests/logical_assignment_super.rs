use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn logical_assignment_super_property_dot_and_computed() -> Result<(), VmError> {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(
      r#"
        (() => {
          // `&&=` (dot)
          {
            const rhs = 2;
            const proto = { p: 1 };
            const obj = { __proto__: proto, p: 0, m() { return (super.p &&= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: 1, m() { return (super.p &&= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj.p !== 1 || proto.p !== 0) return false;
          }

          // `||=` (dot)
          {
            const rhs = 2;
            const proto = { p: 1 };
            const obj = { __proto__: proto, p: 0, m() { return (super.p ||= rhs); } };
            const res = obj.m();
            if (res !== 1 || obj.p !== 0 || proto.p !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: 1, m() { return (super.p ||= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== 0) return false;
          }

          // `??=` (dot)
          {
            const rhs = 2;
            const proto = { p: null };
            const obj = { __proto__: proto, p: 1, m() { return (super.p ??= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj.p !== 2 || proto.p !== null) return false;
          }
          {
            const rhs = 2;
            const proto = { p: 0 };
            const obj = { __proto__: proto, p: null, m() { return (super.p ??= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj.p !== null || proto.p !== 0) return false;
          }

          // Computed super properties.
          const k = 123;

          // `&&=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: 1 };
            const obj = { __proto__: proto, [k]: 0, m() { return (super[k] &&= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] &&= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj[k] !== 1 || proto[k] !== 0) return false;
          }

          // `||=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: 1 };
            const obj = { __proto__: proto, [k]: 0, m() { return (super[k] ||= rhs); } };
            const res = obj.m();
            if (res !== 1 || obj[k] !== 0 || proto[k] !== 1) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ||= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== 0) return false;
          }

          // `??=` (computed)
          {
            const rhs = 2;
            const proto = { [k]: null };
            const obj = { __proto__: proto, [k]: 1, m() { return (super[k] ??= rhs); } };
            const res = obj.m();
            if (res !== 2 || obj[k] !== 2 || proto[k] !== null) return false;
          }
          {
            const rhs = 2;
            const proto = { [k]: 0 };
            const obj = { __proto__: proto, [k]: null, m() { return (super[k] ??= rhs); } };
            const res = obj.m();
            if (res !== 0 || obj[k] !== null || proto[k] !== 0) return false;
          }

          return true;
        })()
      "#,
    )?,
    Value::Bool(true)
  );

  Ok(())
}

