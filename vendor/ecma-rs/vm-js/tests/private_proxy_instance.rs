use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_accessors_do_not_invoke_proxy_traps_when_receiver_is_a_proxy() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      // If private elements are installed via ordinary `DefinePropertyOrThrow` on a Proxy receiver,
      // the `defineProperty` trap would observe the (internal) symbol key and privacy would be
      // broken. Private element initialization must therefore bypass traps.
      let log = [];
      let leaked = undefined;

      class Base {
        constructor() {
          // Ensure methods are found on the proxy by giving the target the derived prototype.
          const target = Object.create(new.target.prototype);
          return new Proxy(target, {
            defineProperty(t, key, desc) {
              log.push(key);
              leaked = key;
              return Reflect.defineProperty(t, key, desc);
            }
          });
        }
      }

      class C extends Base {
        get #x() { return 7; }
        getX() { return this.#x; }
        static hasX(o) { return #x in o; }
      }

      const c = new C();
      let ok = true;
      ok = ok && c.getX() === 7;
      ok = ok && C.hasX(c) === true;
      ok = ok && C.hasX({}) === false;
      ok = ok && log.length === 0 && leaked === undefined;
      ok
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn private_fields_do_not_invoke_proxy_get_traps_when_receiver_is_a_proxy_instance() -> Result<(), VmError>
{
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      let log = [];

      class ProxyBase {
        constructor() {
          return new Proxy(this, {
            get(obj, prop) {
              log.push(prop);
              return obj[prop];
            }
          });
        }
      }

      class Test extends ProxyBase {
        #f = 3;
        method() { return this.#f; }
      }

      const t = new Test();
      const r = t.method();
      r === 3 && log.length === 1 && log[0] === "method"
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn private_methods_do_not_invoke_proxy_get_traps_when_receiver_is_a_proxy_instance() -> Result<(), VmError>
{
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      let log = [];

      class ProxyBase {
        constructor() {
          return new Proxy(this, {
            get(obj, prop) {
              log.push(prop);
              return obj[prop];
            }
          });
        }
      }

      class Test extends ProxyBase {
        #f() { return 3; }
        method() { return this.#f(); }
      }

      const t = new Test();
      const r = t.method();
      r === 3 && log.length === 1 && log[0] === "method"
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn private_getters_do_not_invoke_proxy_get_traps_when_receiver_is_a_proxy_instance() -> Result<(), VmError>
{
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      let log = [];

      class ProxyBase {
        constructor() {
          return new Proxy(this, {
            get(obj, prop) {
              log.push(prop);
              return obj[prop];
            }
          });
        }
      }

      class Test extends ProxyBase {
        get #f() { return 3; }
        method() { return this.#f; }
      }

      const t = new Test();
      const r = t.method();
      r === 3 && log.length === 1 && log[0] === "method"
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
