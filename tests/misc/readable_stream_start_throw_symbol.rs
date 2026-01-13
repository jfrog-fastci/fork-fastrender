use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

#[test]
fn readable_stream_start_throw_symbol_errors_stream_without_throwing_constructor() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__ctor_threw = false;
globalThis.__read_rejected = null;
globalThis.__done = false;
globalThis.__error = null;

let rs;
try {
  rs = new ReadableStream({ start() { throw Symbol('boom'); } });
} catch (e) {
  globalThis.__ctor_threw = true;
}

(async () => {
  const reader = rs.getReader();
  try {
    await reader.read();
    globalThis.__read_rejected = false;
  } catch (e) {
    globalThis.__read_rejected = true;
  }
  globalThis.__done = true;
})().catch((e) => { globalThis.__error = e; globalThis.__done = true; });
"#,
  )?;

  realm.perform_microtask_checkpoint()?;

  assert_eq!(realm.exec_script("globalThis.__done")?, Value::Bool(true));
  assert_eq!(realm.exec_script("globalThis.__ctor_threw")?, Value::Bool(false));
  assert_eq!(realm.exec_script("globalThis.__read_rejected")?, Value::Bool(true));

  let err = realm.exec_script("globalThis.__error")?;
  assert_eq!(err, Value::Null, "expected no error, got {err:?}");

  Ok(())
}

