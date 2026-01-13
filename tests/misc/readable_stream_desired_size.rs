use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

#[test]
fn readable_stream_default_controller_desired_size_tracks_queue() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__done = false;
globalThis.__error = null;
globalThis.__before = null;
globalThis.__after = null;

(async () => {
  const stream = new ReadableStream(
    {
      start(c) {
        globalThis.c = c;
      }
    },
    { highWaterMark: 1 },
  );

  globalThis.c.enqueue(new Uint8Array([1]));
  globalThis.c.enqueue(new Uint8Array([2]));
  globalThis.__before = globalThis.c.desiredSize;

  const reader = stream.getReader();
  await reader.read();
  globalThis.__after = globalThis.c.desiredSize;

  globalThis.__done = true;
})().catch((e) => { globalThis.__error = e; globalThis.__done = true; });
"#,
  )?;

  realm.perform_microtask_checkpoint()?;

  assert_eq!(realm.exec_script("globalThis.__done")?, Value::Bool(true));
  assert_eq!(realm.exec_script("globalThis.__error")?, Value::Null);

  let before = realm.exec_script("globalThis.__before")?;
  let after = realm.exec_script("globalThis.__after")?;

  let Value::Number(before_n) = before else {
    panic!("expected number, got {before:?}");
  };
  let Value::Number(after_n) = after else {
    panic!("expected number, got {after:?}");
  };

  assert!(
    before_n <= 0.0,
    "expected desiredSize to be <= 0 after enqueueing more than the highWaterMark, got {before_n}",
  );
  assert!(
    after_n > before_n,
    "expected desiredSize to increase after reading, before={before_n} after={after_n}",
  );

  Ok(())
}
