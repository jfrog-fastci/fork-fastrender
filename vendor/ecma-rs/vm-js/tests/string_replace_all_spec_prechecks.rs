use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn replace_all_flags_getter_throw_happens_before_symbol_replace_and_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const log = [];
      const thisValue = {
        toString() {
          log.push("this");
          throw "thisToString";
        }
      };
      const replaceValue = {
        toString() {
          log.push("replace");
          throw "replaceToString";
        }
      };
      const searchValue = {
        get [Symbol.match]() {
          log.push("@@match");
          return true;
        },
        get flags() {
          log.push("flags");
          throw "flagsThrow";
        },
        get [Symbol.replace]() {
          log.push("@@replace");
          return function () {
            log.push("@@replaceCall");
            return "ok";
          };
        }
      };

      let threw = false;
      try {
        String.prototype.replaceAll.call(thisValue, searchValue, replaceValue);
      } catch (e) {
        threw = (e === "flagsThrow");
      }

      threw && log.join(",") === "@@match,flags";
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn replace_all_flags_to_string_throw_happens_before_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const log = [];
      const thisValue = {
        toString() {
          log.push("this");
          throw "thisToString";
        }
      };
      const replaceValue = {
        toString() {
          log.push("replace");
          throw "replaceToString";
        }
      };

      const flagsObj = {
        toString() {
          log.push("flagsToString");
          throw "flagsToString";
        }
      };

      const searchValue = {
        get [Symbol.match]() {
          log.push("@@match");
          return true;
        },
        get flags() {
          log.push("flags");
          return flagsObj;
        }
      };

      let threw = false;
      try {
        String.prototype.replaceAll.call(thisValue, searchValue, replaceValue);
      } catch (e) {
        threw = (e === "flagsToString");
      }

      threw && log.join(",") === "@@match,flags,flagsToString";
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn replace_all_flags_require_object_coercible_happens_before_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      function isTypeError(thunk) {
        try {
          thunk();
          return false;
        } catch (e) {
          return e instanceof TypeError;
        }
      }

      const log = [];
      function makeThis(tag) {
        return {
          toString() {
            log.push(tag);
            throw tag;
          }
        };
      }

      const replaceValue = {
        toString() {
          log.push("replace");
          throw "replaceToString";
        }
      };

      const searchNull = {
        get [Symbol.match]() {
          log.push("@@matchNull");
          return true;
        },
        get flags() {
          log.push("flagsNull");
          return null;
        }
      };

      const searchUndef = {
        get [Symbol.match]() {
          log.push("@@matchUndef");
          return true;
        },
        get flags() {
          log.push("flagsUndef");
          return undefined;
        }
      };

      const okNull = isTypeError(() => String.prototype.replaceAll.call(makeThis("thisNull"), searchNull, replaceValue));
      const okUndef = isTypeError(() => String.prototype.replaceAll.call(makeThis("thisUndef"), searchUndef, replaceValue));

      okNull && okUndef && log.join(",") === "@@matchNull,flagsNull,@@matchUndef,flagsUndef";
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn replace_all_symbol_match_throw_happens_before_flags_symbol_replace_and_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const log = [];
      const thisValue = {
        toString() {
          log.push("this");
          throw "thisToString";
        }
      };
      const replaceValue = {
        toString() {
          log.push("replace");
          throw "replaceToString";
        }
      };
      const searchValue = {
        get [Symbol.match]() {
          log.push("@@match");
          throw "matchThrow";
        },
        get flags() {
          log.push("flags");
          return "g";
        },
        get [Symbol.replace]() {
          log.push("@@replace");
          return function () { return "ok"; };
        }
      };

      let threw = false;
      try {
        String.prototype.replaceAll.call(thisValue, searchValue, replaceValue);
      } catch (e) {
        threw = (e === "matchThrow");
      }

      threw && log.join(",") === "@@match";
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn replace_all_non_global_regexp_like_throws_before_symbol_replace_and_tostring() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      const log = [];
      const thisValue = {
        toString() {
          log.push("this");
          throw "thisToString";
        }
      };
      const replaceValue = {
        toString() {
          log.push("replace");
          throw "replaceToString";
        }
      };
      const searchValue = {
        get [Symbol.match]() {
          log.push("@@match");
          return true;
        },
        get flags() {
          log.push("flags");
          return "i"; // no "g"
        },
        get [Symbol.replace]() {
          log.push("@@replace");
          return function () { return "ok"; };
        }
      };

      let threwTypeError = false;
      try {
        String.prototype.replaceAll.call(thisValue, searchValue, replaceValue);
      } catch (e) {
        threwTypeError = e instanceof TypeError;
      }

      threwTypeError && log.join(",") === "@@match,flags";
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

