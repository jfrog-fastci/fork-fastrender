use vm_js::{Heap, HeapLimits, JsRuntime, RootId, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();

  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));

  // Root the thrown value across any subsequent allocations / script runs.
  let root: RootId = rt.heap_mut().add_root(thrown).expect("root thrown value");

  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt
    .exec_script("globalThis.TypeError.prototype")
    .expect("evaluate TypeError.prototype");
  let Value::Object(type_error_proto) = type_error_proto else {
    panic!("expected TypeError.prototype to be an object");
  };

  let thrown_proto = rt
    .heap()
    .object_prototype(thrown_obj)
    .expect("get thrown prototype");
  assert_eq!(thrown_proto, Some(type_error_proto));

  rt.heap_mut().remove_root(root);
}

#[test]
fn error_prototype_to_string_observes_proxy_get_traps() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
      var keys = [];
      var receivers = [];
      var p;
      var handler = {
        get: function (t, k, r) {
          keys.push(String(k));
          receivers.push(r === p);
          if (k === "name") return "MyError";
          if (k === "message") return "Boom";
          return undefined;
        }
      };
      p = new Proxy({}, handler);
      var s = Error.prototype.toString.call(p);
      s === "MyError: Boom"
        && keys.join(",") === "name,message"
        && receivers.length === 2
        && receivers[0] === true
        && receivers[1] === true;
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn error_prototype_to_string_throws_type_error_on_revoked_proxy() {
  let mut rt = new_runtime();
  assert_throws_type_error(
    &mut rt,
    r#"
      let r = Proxy.revocable({}, { get() { return "x"; } });
      r.revoke();
      Error.prototype.toString.call(r.proxy);
    "#,
  );
}

