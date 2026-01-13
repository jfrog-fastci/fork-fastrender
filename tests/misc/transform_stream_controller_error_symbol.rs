use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

#[test]
fn transform_stream_controller_error_symbol_errors_readable_without_throwing() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__done = false;
globalThis.__outerError = null;
globalThis.__afterError = null;
globalThis.__writeSettled = false;
globalThis.__writeRejected = false;
globalThis.__readSettled = false;
globalThis.__readRejected = false;

(async () => {
  const ts = new TransformStream({
    transform(_chunk, controller) {
      try {
        controller.error(Symbol('boom'));
        globalThis.__afterError = true;
      } catch (e) {
        globalThis.__afterError = false;
      }
    }
  });

  const writer = ts.writable.getWriter();
  try {
    await writer.write(new Uint8Array([1]));
    globalThis.__writeSettled = true;
  } catch (e) {
    globalThis.__writeSettled = true;
    globalThis.__writeRejected = true;
  }

  const reader = ts.readable.getReader();
  try {
    await reader.read();
    globalThis.__readSettled = true;
  } catch (e) {
    globalThis.__readSettled = true;
    globalThis.__readRejected = true;
  }

  globalThis.__done = true;
})().catch((e) => {
  globalThis.__outerError = e;
  globalThis.__done = true;
});
"#,
  )?;

  realm.perform_microtask_checkpoint()?;

  assert_eq!(realm.exec_script("globalThis.__done")?, Value::Bool(true));
  let outer_error = realm.exec_script("globalThis.__outerError")?;
  assert_eq!(
    outer_error,
    Value::Null,
    "expected no outer error, got {outer_error:?}"
  );

  assert_eq!(realm.exec_script("globalThis.__afterError")?, Value::Bool(true));
  assert_eq!(realm.exec_script("globalThis.__writeSettled")?, Value::Bool(true));
  assert_eq!(realm.exec_script("globalThis.__writeRejected")?, Value::Bool(false));
  assert_eq!(realm.exec_script("globalThis.__readSettled")?, Value::Bool(true));
  assert_eq!(realm.exec_script("globalThis.__readRejected")?, Value::Bool(true));

  Ok(())
}

