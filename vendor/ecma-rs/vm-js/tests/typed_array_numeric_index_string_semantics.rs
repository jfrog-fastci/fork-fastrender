use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_heap_limit(bytes: usize) -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(bytes, bytes));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime() -> JsRuntime {
  new_runtime_with_heap_limit(2 * 1024 * 1024)
}

#[test]
fn typed_array_canonical_numeric_index_string_keys_do_not_fall_back_to_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      (function () {
        // Poison the prototype chain with numeric-looking keys. TypedArrays must not consult the
        // prototype chain for canonical numeric index string keys.
        Object.prototype["-1"] = 123;
        Object.prototype["1.5"] = 456;

        let u = new Uint8Array(2);

        let ok_get = u["-1"] === undefined && u["1.5"] === undefined;
        let ok_in = ("-1" in u) === false && ("1.5" in u) === false;

        // Invalid numeric indices should be ignored (and therefore not throw in strict mode).
        let ok_set_strict = (function () {
          "use strict";
          try {
            u["-1"] = 1;
            u["1.5"] = 1;
            return true;
          } catch (e) {
            return false;
          }
        })();

        let ok_no_own_props =
          u.hasOwnProperty("-1") === false &&
          u.hasOwnProperty("1.5") === false;

        return ok_get && ok_in && ok_set_strict && ok_no_own_props;
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn typed_array_set_receiver_semantics_match_spec() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      {
        let u = new Uint8Array(1);
        let receiver = {};

        // When receiver != target and the key is a valid integer index, fall back to ordinary set
        // semantics (define on receiver rather than writing through the typed array).
        let ok_valid = Reflect.set(u, "0", 7, receiver) === true && u[0] === 0 && receiver[0] === 7;

        // When the key is a canonical numeric index string but not a valid integer index, the set
        // is a no-op that still reports success.
        let ok_invalid = Reflect.set(u, "-1", 9, receiver) === true && receiver["-1"] === undefined;

        // `TypedArray.[[Set]]` only performs value conversion when `receiver === target`.
        //
        // This must not throw even though `1n` cannot be converted to Number, because `receiver !== target`
        // and the index is invalid.
        let ok_no_convert = (function () {
          try {
            return Reflect.set(u, "1.5", 1n, receiver) === true && receiver["1.5"] === undefined;
          } catch (e) {
            return false;
          }
        })();

        // When `receiver === target`, `TypedArraySetElement` performs value conversion even for invalid indices.
        let threw_non_integer = (function () {
          "use strict";
          try {
            u["1.5"] = 1n;
            return false;
          } catch (e) {
            return e instanceof TypeError;
          }
        })();

        let threw_negative = (function () {
          "use strict";
          try {
            u["-1"] = 1n;
            return false;
          } catch (e) {
            return e instanceof TypeError;
          }
        })();

        ok_valid && ok_invalid && ok_no_convert && threw_non_integer && threw_negative
      }
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
