use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_private_in_operator_yield_rhs_true_false() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (function () {
        class C {
          #x;
          *hasGen() { return #x in (yield 0); }
        }
        class D { #x; }
        const c = new C();
        function run(o) {
          const it = c.hasGen();
          const r1 = it.next();
          if (r1.done !== false || r1.value !== 0) return false;
          const r2 = it.next(o);
          return r2.done === true && r2.value;
        }
        return run(c) === true &&
          run(new C()) === true &&
          run(new D()) === false &&
          run({}) === false &&
          // Brand checks must not consult the prototype chain.
          run(Object.create(c)) === false;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_in_operator_yield_rhs_non_object_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (function () {
        class C {
          #x;
          *g() {
            try {
              return #x in (yield 0);
            } catch (e) {
              return e.name;
            }
          }
        }
        const it = (new C()).g();
        const r1 = it.next();
        const r2 = it.next(1);
        return r1.done === false && r1.value === 0 &&
          r2.done === true && r2.value === "TypeError";
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_private_in_operator_yield_rhs_proxy_returns_false() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (function () {
        class C {
          #x;
          *hasGen() { return #x in (yield 0); }
        }
        const c = new C();
        const proxy = new Proxy(c, {
          has() { throw new Error("has trap should not run"); },
          getOwnPropertyDescriptor() {
            throw new Error("getOwnPropertyDescriptor trap should not run");
          },
        });
        const it = c.hasGen();
        const r1 = it.next();
        const r2 = it.next(proxy);
        return r1.done === false && r1.value === 0 &&
          r2.done === true && r2.value === false;
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

