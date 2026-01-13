use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

fn get_string(heap: &vm_js::Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn debug_js_error(realm: &mut WindowRealm, v: Value) -> String {
  if matches!(v, Value::Null | Value::Undefined) {
    return format!("{v:?}");
  }
  // Best-effort: stringify and include stack if available.
  realm
    .exec_script(
      r#"
(() => {
  const e = globalThis.__error;
  try {
    if (!e) return "<no error>";
    if (typeof e === "object") {
      const name = ("name" in e && typeof e.name === "string") ? e.name : "<no name>";
      const message = ("message" in e && typeof e.message === "string") ? e.message : "<no message>";
      const stack = ("stack" in e && typeof e.stack === "string") ? e.stack : "";
      return `${name}: ${message}${stack ? "\n" + stack : ""}`;
    }
    return String(e);
  } catch (inner) {
    return "<unstringifiable error>";
  }
})()
"#,
    )
    .ok()
    .map(|v| {
      if matches!(v, Value::String(_)) {
        get_string(realm.heap(), v)
      } else {
        format!("{v:?}")
      }
    })
    .unwrap_or_else(|| "<failed to format error>".to_string())
}

#[test]
fn text_decoder_stream_is_installed_and_constructable() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  let ty = realm.exec_script("typeof TextDecoderStream")?;
  assert_eq!(get_string(realm.heap(), ty), "function");

  let ok = realm.exec_script(
    "(() => { try { new TextDecoderStream(); return true; } catch { return false; } })()",
  )?;
  assert_eq!(ok, Value::Bool(true));

  Ok(())
}

#[test]
fn text_decoder_stream_decodes_utf8_bytes_via_pipe_through() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__result = null;
globalThis.__error = null;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([104, 101, 108])); // "hel"
      controller.enqueue(new Uint8Array([108, 111])); // "lo"
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream());
  const reader = decoded.getReader();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += value;
  }
  return out;
})().then((v) => { globalThis.__result = v; }, (e) => { globalThis.__error = e; });
"#,
  )?;
  realm.perform_microtask_checkpoint()?;

  let err = realm.exec_script("globalThis.__error")?;
  assert_eq!(
    err,
    Value::Null,
    "expected no error, got {err:?}\n{}",
    debug_js_error(&mut realm, err)
  );
  let out = realm.exec_script("globalThis.__result")?;
  assert_eq!(get_string(realm.heap(), out), "hello");
  Ok(())
}

#[test]
fn text_decoder_stream_preserves_partial_multibyte_utf8_sequences_across_chunks() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__result = null;
globalThis.__error = null;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([0xE2, 0x82])); // partial "€"
      controller.enqueue(new Uint8Array([0xAC])); // completes "€"
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream());
  const reader = decoded.getReader();
  let out = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    out += value;
  }
  return out;
})().then((v) => { globalThis.__result = v; }, (e) => { globalThis.__error = e; });
"#,
  )?;
  realm.perform_microtask_checkpoint()?;

  let err = realm.exec_script("globalThis.__error")?;
  assert_eq!(
    err,
    Value::Null,
    "expected no error, got {err:?}\n{}",
    debug_js_error(&mut realm, err)
  );
  let out = realm.exec_script("globalThis.__result")?;
  assert_eq!(get_string(realm.heap(), out), "€");
  Ok(())
}

#[test]
fn text_decoder_stream_fatal_mode_errors_on_invalid_utf8() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__done = false;
globalThis.__threw = false;
(async () => {
  const src = new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array([0xFF])); // invalid UTF-8
      controller.close();
    },
  });
  const decoded = src.pipeThrough(new TextDecoderStream("utf-8", { fatal: true }));
  const reader = decoded.getReader();
  try {
    while (true) {
      const { done } = await reader.read();
      if (done) break;
    }
  } catch (e) {
    globalThis.__threw = true;
  }
  globalThis.__done = true;
})();
"#,
  )?;
  realm.perform_microtask_checkpoint()?;

  let done = realm.exec_script("globalThis.__done")?;
  assert_eq!(done, Value::Bool(true));
  let threw = realm.exec_script("globalThis.__threw")?;
  assert_eq!(threw, Value::Bool(true));
  Ok(())
}

