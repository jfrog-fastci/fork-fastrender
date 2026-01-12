use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn function_prototype_bind_accepts_callable_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let p = new Proxy(function f(a,b){ return a+b; }, {});
      let g = p.bind(null, 1);
      g(2) === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn bind_callable_proxy_hits_apply_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let applyCount = 0;
      let seenThis = null;
      let seenLen = 0;

      let p = new Proxy(function () {}, {
        apply(t, thisArg, args) {
          applyCount++;
          seenThis = thisArg;
          seenLen = args.length;
          return 99;
        },
      });

      let bound = Function.prototype.bind.call(p, 123, 1, 2);
      let result = bound(3);

      result === 99 && applyCount === 1 && seenThis === 123 && seenLen === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn function_prototype_bind_accepts_constructable_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let p = new Proxy(function C(){ this.x=1; }, {});
      let B = p.bind(null);
      (new B()).x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn bind_constructable_proxy_hits_construct_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let constructCount = 0;
      let seenNewTargetIsProxy = false;
      let seenLen = 0;

      let p = new Proxy(function () {}, {
        construct(t, args, nt) {
          constructCount++;
          seenNewTargetIsProxy = (nt === p);
          seenLen = args.length;
          return {};
        },
      });

      let Bound = Function.prototype.bind.call(p, null, 1, 2);
      new Bound(3);

      constructCount === 1 && seenNewTargetIsProxy === true && seenLen === 3
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn bind_revoked_proxy_throws_type_error() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let r = Proxy.revocable(function () {}, {});
      r.revoke();
      try {
        Function.prototype.bind.call(r.proxy, null);
        false
      } catch (e) {
        e instanceof TypeError
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn bind_name_and_length_use_get_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let nameGets = 0;
      let lengthGets = 0;

      let p = new Proxy(function () {}, {
        get(t, prop, receiver) {
          if (prop === "length") {
            lengthGets++;
            return 5;
          }
          if (prop === "name") {
            nameGets++;
            return "abc";
          }
          return t[prop];
        },
      });

      let bound = Function.prototype.bind.call(p, null, 1, 2);

      bound.length === 3 && bound.name === "bound abc" && nameGets === 1 && lengthGets === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

