use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

fn get_string(heap: &vm_js::Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn readable_stream_tee_works_before_transform_stream_enqueues_first_chunk() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  // This intentionally tees the output of `pipeThrough(new TextEncoderStream())` immediately,
  // before the TransformStream has had a chance to enqueue any chunks into its readable side.
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
      controller.enqueue("hi");
      controller.close();
    }
  }).pipeThrough(new TextEncoderStream());

  const branches = stream.tee();
  globalThis.__lockedAfterTee = stream.locked;

  const r0 = branches[0].getReader();
  const r1 = branches[1].getReader();
  const p0 = r0.read();
  const p1 = r1.read();
  const v0 = await p0;
  const v1 = await p1;

  const dec = new TextDecoder();
  globalThis.__result0 = dec.decode(v0.value);
  globalThis.__result1 = dec.decode(v1.value);
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

