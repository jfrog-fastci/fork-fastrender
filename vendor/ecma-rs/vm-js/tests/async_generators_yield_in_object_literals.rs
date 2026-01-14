use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Async generator tests allocate Promise/job machinery; use a slightly larger heap than the
  // minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy().to_string()
}

#[test]
fn async_generators_yield_in_object_literals() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var actual = [];

      async function* g() {
        const o = {
          a: 0,
          ...(yield "spread"),
          [(yield "k")]: (yield "v"),
          [(yield "m")]() { return 2; },
          get [(yield "get")]() { return 1; },
          set [(yield "set")](x) { this._ = x; },
        };
        return o;
      }

      var it = g();
      var p1 = it.next();
      actual.push(p1 instanceof Promise);

      (async function run() {
        var r1 = await p1;
        actual.push([r1.value, r1.done]);

        var r2 = await it.next({ x: 1 });
        actual.push([r2.value, r2.done]);

        var r3 = await it.next("prop");
        actual.push([r3.value, r3.done]);

        var r4 = await it.next(10);
        actual.push([r4.value, r4.done]);

        var r5 = await it.next("foo");
        actual.push([r5.value, r5.done]);

        var r6 = await it.next("g");
        actual.push([r6.value, r6.done]);

        var r7 = await it.next("s");
        var o = r7.value;
        var ok =
          r7.done === true &&
          o.a === 0 &&
          o.x === 1 &&
          o.prop === 10 &&
          typeof o.foo === "function" &&
          o.foo() === 2 &&
          o.g === 1 &&
          ((o.s = 7), o._ === 7);
        actual.push([r7.done, ok]);
      })();

      JSON.stringify(actual)
    "#,
  )?;

  // The first `next()` must synchronously return a Promise.
  assert_eq!(value_to_utf8(&rt, value), r#"[true]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_utf8(&rt, value),
    r#"[true,["spread",false],["k",false],["v",false],["m",false],["get",false],["set",false],[true,true]]"#
  );

  Ok(())
}

#[test]
fn async_generators_yield_in_object_literals_proto_setter_and_super() -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !_async_generator_support::supports_async_generators(&mut rt)? {
    return Ok(());
  }

  let value = rt.exec_script(
    r#"
      var actual = [];
      const proto = { get x() { return this.y + 1; } };

      async function* g() {
        const o = {
          __proto__: (yield "proto"),
          y: 41,
          m() { return super.x; },
        };
        return o;
      }

      var it = g();
      var p1 = it.next();
      actual.push(p1 instanceof Promise);

      (async function run() {
        var r1 = await p1;
        actual.push([r1.value, r1.done]);

        var r2 = await it.next(proto);
        var o = r2.value;
        var proto_ok = Object.getPrototypeOf(o) === proto;
        var desc = Object.getOwnPropertyDescriptor(o, "__proto__");
        var desc_ok = desc === undefined;
        var m_res;
        var m_ok = false;
        try {
          m_res = o.m();
          m_ok = m_res === 42;
        } catch (e) {
          m_res = String(e && e.message !== undefined ? e.message : e);
        }
        var ok = r2.done === true && proto_ok && desc_ok && m_ok;
        actual.push([r2.done, ok]);
      })();

      JSON.stringify(actual)
    "#,
  )?;

  assert_eq!(value_to_utf8(&rt, value), r#"[true]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(
    value_to_utf8(&rt, value),
    r#"[true,["proto",false],[true,true]]"#
  );

  Ok(())
}
