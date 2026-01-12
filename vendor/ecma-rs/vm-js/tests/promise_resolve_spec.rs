use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn await_promise_with_modified_prototype_but_same_constructor_still_awaits() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `await` uses `PromiseResolve(%Promise%, x)`. Per spec, when `x` is a Promise, it must read
  // `x.constructor` and return `x` only if that constructor is `%Promise%` (not based on
  // `x.[[Prototype]]`).
  //
  // This test uses a Promise with a non-standard prototype that:
  // - preserves `constructor === Promise` (so PromiseResolve should return the original Promise),
  // - but *removes* `then` (so an incorrect wrapper Promise would fulfill with the Promise object).
  let value = rt.exec_script(
    r#"
      var out = 0;
      var p = Promise.resolve(1);
      Object.setPrototypeOf(p, { constructor: Promise });
      async function f() { return await p; }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(1.0));
  Ok(())
}

#[test]
fn promise_resolve_observes_constructor_get() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `PromiseResolve(%Promise%, x)` must perform `Get(x, "constructor")` (observable via accessors /
  // Proxy traps). This ensures `await` is Proxy-aware and does not use a prototype-based shortcut.
  let value = rt.exec_script(
    r#"
      var log = [];

      // Force the constructor lookup to hit the prototype chain (and thus run an accessor).
      delete Promise.prototype.constructor;
      Object.setPrototypeOf(Promise.prototype, {
        get constructor() { log.push("constructor"); return Promise; }
      });

      var p = Promise.resolve(1);
      async function f() { await p; }
      f();

      log.join(",")
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "constructor");

  // Drain any Promise jobs enqueued by the async function so the queued `Job`s are not dropped with
  // live persistent roots.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  Ok(())
}

#[test]
fn promise_resolve_observes_constructor_get_via_proxy_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // `PromiseResolve(%Promise%, x)` must use the internal-method dispatch `Get`, so a Proxy in the
  // prototype chain can observe the lookup via its `get` trap.
  let value = rt.exec_script(
    r#"
      var log = [];

      var p = Promise.resolve(1);

      // Force the constructor lookup to hit the prototype chain.
      delete Promise.prototype.constructor;
      Object.setPrototypeOf(Promise.prototype, new Proxy({ constructor: Promise }, {
        get(target, key, receiver) {
          log.push(String(key));
          return Reflect.get(target, key, receiver);
        }
      }));

      async function f() { await p; }
      f();

      log.join(",")
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "constructor");

  // Drain any Promise jobs enqueued by the async function so the queued `Job`s are not dropped with
  // live persistent roots.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
  Ok(())
}

#[test]
fn await_rejects_when_constructor_lookup_hits_revoked_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // A revoked Proxy in the prototype chain must throw (as a JS exception), and `await` must turn
  // that throw into a rejection of the async function's promise (not a synchronous throw).
  let value = rt.exec_script(
    r#"
      var out = 0;

      var p = Promise.resolve(1);

      // Force PromiseResolve(%Promise%, p) to consult the prototype chain for `constructor` and
      // hit a revoked Proxy.
      var r = Proxy.revocable({ constructor: Promise }, {});
      var proto = {};
      Object.setPrototypeOf(proto, r.proxy);
      Object.setPrototypeOf(p, proto);
      r.revoke();

      async function f() { return await p; }
      f().then(
        function () { out = 1; },
        function (e) { out = e instanceof TypeError ? 2 : 3; }
      );

      out
    "#,
  )?;
  assert_eq!(value, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(out, Value::Number(2.0));
  Ok(())
}
