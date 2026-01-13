use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

#[test]
fn transform_stream_async_transform_rejection_errors_readable_side() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__done = false;
globalThis.__outerError = null;
globalThis.__writeSettled = false;
globalThis.__writeRejected = false;
globalThis.__readSettled = false;
globalThis.__readRejected = false;

(async () => {
  const ts = new TransformStream({
    transform() {
      return Promise.reject("boom");
    }
  });

  const reader = ts.readable.getReader();
  const readPromise = reader.read(); // should be rejected once transform errors the readable side.

  const writer = ts.writable.getWriter();
  const writePromise = writer.write(new Uint8Array([1]));

  try {
    await writePromise;
    globalThis.__writeSettled = true;
  } catch (e) {
    globalThis.__writeSettled = true;
    globalThis.__writeRejected = true;
  }

  try {
    await readPromise;
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

  let done = realm.exec_script("globalThis.__done")?;
  assert_eq!(done, Value::Bool(true));

  let outer_error = realm.exec_script("globalThis.__outerError")?;
  assert_eq!(
    outer_error,
    Value::Null,
    "expected no outer error, got {outer_error:?}"
  );

  let write_settled = realm.exec_script("globalThis.__writeSettled")?;
  let write_rejected = realm.exec_script("globalThis.__writeRejected")?;
  assert_eq!(write_settled, Value::Bool(true));
  assert_eq!(write_rejected, Value::Bool(true));

  let read_settled = realm.exec_script("globalThis.__readSettled")?;
  let read_rejected = realm.exec_script("globalThis.__readRejected")?;
  assert_eq!(read_settled, Value::Bool(true));
  assert_eq!(read_rejected, Value::Bool(true));

  Ok(())
}

