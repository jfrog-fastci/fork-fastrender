//! Web Streams `tee()` integration tests.

use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

fn get_string(heap: &vm_js::Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn readable_stream_tee_works_for_string_streams_before_first_chunk_is_enqueued() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  // This tees the output of `pipeThrough(new TextDecoderStream())` immediately, before the
  // TransformStream has had a chance to enqueue any string chunks into its readable side.
  realm.exec_script(
    r#"
globalThis.__done = false;
globalThis.__error = null;
globalThis.__lockedAfterTee = null;
globalThis.__result0 = null;
globalThis.__result1 = null;

(async () => {
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([104, 105])); // "hi"
      controller.close();
    }
  }).pipeThrough(new TextDecoderStream());

  const branches = stream.tee();
  globalThis.__lockedAfterTee = stream.locked;

  const r0 = branches[0].getReader();
  const r1 = branches[1].getReader();
  const v0 = await r0.read();
  const v1 = await r1.read();

  globalThis.__result0 = v0.value;
  globalThis.__result1 = v1.value;
  globalThis.__done = true;
})().catch((e) => { globalThis.__error = e; globalThis.__done = true; });
"#,
  )?;

  realm.perform_microtask_checkpoint()?;

  let done = realm.exec_script("globalThis.__done")?;
  assert_eq!(done, Value::Bool(true));

  let err = realm.exec_script("globalThis.__error")?;
  assert_eq!(err, Value::Null, "expected no error, got {err:?}");

  let locked = realm.exec_script("globalThis.__lockedAfterTee")?;
  assert_eq!(locked, Value::Bool(true));

  let out0 = realm.exec_script("globalThis.__result0")?;
  let out1 = realm.exec_script("globalThis.__result1")?;
  assert_eq!(get_string(realm.heap(), out0), "hi");
  assert_eq!(get_string(realm.heap(), out1), "hi");

  Ok(())
}
