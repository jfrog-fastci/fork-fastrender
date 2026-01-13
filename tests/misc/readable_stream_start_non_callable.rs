use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Value, VmError};

#[test]
fn readable_stream_start_non_callable_throws() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

  realm.exec_script(
    r#"
globalThis.__ctor_threw = false;
try {
  new ReadableStream({ start: 5 });
} catch (e) {
  globalThis.__ctor_threw = true;
}
"#,
  )?;

  assert_eq!(realm.exec_script("globalThis.__ctor_threw")?, Value::Bool(true));

  Ok(())
}

