use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

fn get_string(heap: &vm_js::Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn queuing_strategy_constructors_are_installed_and_behave_reasonably() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  let ty = realm.exec_script("typeof ByteLengthQueuingStrategy")?;
  assert_eq!(get_string(realm.heap(), ty), "function");
  let ty = realm.exec_script("typeof CountQueuingStrategy")?;
  assert_eq!(get_string(realm.heap(), ty), "function");

  realm.exec_script(
    r#"
globalThis.__ok = false;
globalThis.__error = null;
try {
  const b = new ByteLengthQueuingStrategy({ highWaterMark: 4 });
  const c = new CountQueuingStrategy({ highWaterMark: 7 });
  globalThis.__bHwm = b.highWaterMark;
  globalThis.__cHwm = c.highWaterMark;
  globalThis.__bSize = b.size(new Uint8Array([1, 2, 3]));
  globalThis.__cSize = c.size("ignored");
  globalThis.__negativeThrowsRangeError =
    (() => { try { new ByteLengthQueuingStrategy({ highWaterMark: -1 }); return false; } catch (e) { return e && e.name === "RangeError"; } })();
  globalThis.__missingArgsThrows =
    (() => { try { new CountQueuingStrategy(); return false; } catch (e) { return e instanceof TypeError; } })();
  globalThis.__ok = true;
} catch (e) {
  globalThis.__error = e;
  globalThis.__ok = false;
}
"#,
  )?;

  let ok = realm.exec_script("globalThis.__ok")?;
  assert_eq!(ok, Value::Bool(true));
  let err = realm.exec_script("globalThis.__error")?;
  assert_eq!(err, Value::Null, "expected no error, got {err:?}");

  let b_hwm = realm.exec_script("globalThis.__bHwm")?;
  assert_eq!(b_hwm, Value::Number(4.0));
  let c_hwm = realm.exec_script("globalThis.__cHwm")?;
  assert_eq!(c_hwm, Value::Number(7.0));

  let b_size = realm.exec_script("globalThis.__bSize")?;
  assert_eq!(b_size, Value::Number(3.0));
  let c_size = realm.exec_script("globalThis.__cSize")?;
  assert_eq!(c_size, Value::Number(1.0));

  let negative_throws = realm.exec_script("globalThis.__negativeThrowsRangeError")?;
  assert_eq!(negative_throws, Value::Bool(true));
  let missing_args_throws = realm.exec_script("globalThis.__missingArgsThrows")?;
  assert_eq!(missing_args_throws, Value::Bool(true));

  Ok(())
}

