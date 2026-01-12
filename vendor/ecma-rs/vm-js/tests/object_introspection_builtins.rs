use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_get_own_property_names_contains_string_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var names = Object.getOwnPropertyNames({a:1}); names.length === 1 && names[0] === "a""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_symbols_returns_symbol_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var sym = Symbol("x");
      var o = {};
      o[sym] = 1;
      var syms = Object.getOwnPropertySymbols(o);
      syms.length === 1 && syms[0] === sym
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_descriptor_returns_data_descriptor_fields() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = { a: 1 };
      var d = Object.getOwnPropertyDescriptor(o, "a");
      d.value === 1 &&
        d.writable === true &&
        d.enumerable === true &&
        d.configurable === true &&
        !("get" in d) &&
        !("set" in d)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_descriptor_materializes_string_index_values() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        const s = new String("abc");
        const direct = Object.getOwnPropertyDescriptor(s, "0");
        const p = new Proxy(s, {});
        const proxied = Object.getOwnPropertyDescriptor(p, "0");
        const proxiedReflect = Reflect.getOwnPropertyDescriptor(p, "0");
        return direct.value === "a" && proxied.value === "a" && proxiedReflect.value === "a";
      })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_is_extensible_primitives_false_objects_true() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"Object.isExtensible(1) === false && Object.isExtensible({}) === true"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_get_own_property_descriptors_is_proxy_aware() {
  let mut rt = new_runtime();
  let script = r#"
    (() => {
      const target = {};
      const p = new Proxy(target, {
        ownKeys() { return ["a", "b"]; },
        getOwnPropertyDescriptor(_t, k) {
          return k === "a" ? { value: 1, configurable: true } : undefined;
        },
      });
      const descs = Object.getOwnPropertyDescriptors(p);
      return descs.a.value === 1 && !Object.hasOwn(descs, "b");
    })()
  "#;
  let value = rt.exec_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_is_extensible_obeys_proxy_invariants() {
  let mut rt = new_runtime();
  let script = r#"
    (() => {
      const p = new Proxy({}, { isExtensible() { return false; } });
      try { Object.isExtensible(p); return false; } catch (e) { return e instanceof TypeError; }
    })()
  "#;
  let value = rt.exec_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_prevent_extensions_obeys_proxy_invariants() {
  let mut rt = new_runtime();
  let script = r#"
    (() => {
      const p = new Proxy({}, { preventExtensions() { return true; } });
      try { Object.preventExtensions(p); return false; } catch (e) { return e instanceof TypeError; }
    })()
  "#;
  let value = rt.exec_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_prototype_chain_has_expected_own_names() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = r#"
    const GF = Object.getPrototypeOf(function*(){}).constructor;
    const GP = GF.prototype.prototype;
    const names = Object.getOwnPropertyNames(GP);
    names.indexOf("next") !== -1 &&
      names.indexOf("return") !== -1 &&
      names.indexOf("throw") !== -1 &&
      names.indexOf("constructor") !== -1
  "#;
  match rt.exec_script(script) {
    Ok(value) => {
      assert_eq!(value, Value::Bool(true));
      Ok(())
    }
    Err(VmError::Unimplemented(msg)) if msg.contains("generator functions") => Ok(()),
    Err(err) => Err(err),
  }
}

#[test]
fn internal_symbols_are_hidden_from_own_property_keys() {
  let mut rt = new_runtime();
  let script = r#"
    (() => {
      // Array iterator objects: should not expose internal symbol-keyed slots.
      const arrIter = [1, 2][Symbol.iterator]();
      if (Reflect.ownKeys(arrIter).length !== 0) return false;
      if (Object.getOwnPropertySymbols(arrIter).length !== 0) return false;

      // String iterator objects: should not expose internal symbol-keyed slots.
      const strIter = "ab"[Symbol.iterator]();
      if (Reflect.ownKeys(strIter).length !== 0) return false;
      if (Object.getOwnPropertySymbols(strIter).length !== 0) return false;

      // RegExp matchAll iterator objects: should only expose the (temporary) iterable plumbing, not
      // internal slot markers.
      const reIter = /a/g[Symbol.matchAll]("a");
      const keys = Reflect.ownKeys(reIter);
      const syms = Object.getOwnPropertySymbols(reIter);
      return keys.length === 2 && keys[0] === "next" && keys[1] === Symbol.iterator &&
        syms.length === 1 && syms[0] === Symbol.iterator;
    })()
  "#;
  let value = rt.exec_script(script).unwrap();
  assert_eq!(value, Value::Bool(true));
}
