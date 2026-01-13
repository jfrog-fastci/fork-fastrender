use fastrender::js::{WindowRealm, WindowRealmConfig};
use fastrender::js::window_realm::DomBindingsBackend;
use vm_js::{Heap, Value, VmError};

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn assert_chrome_not_exposed_by_default(mut realm: WindowRealm) -> Result<(), VmError> {
  let ty = realm.exec_script("typeof chrome")?;
  assert_eq!(
    get_string(realm.heap(), ty),
    "undefined",
    "chrome should not be installed in non-chrome realms by default",
  );

  // Ensure attempting to access the chrome API surface throws (and does not crash the VM).
  match realm.exec_script("chrome.navigation") {
    Ok(value) => panic!(
      "expected chrome.navigation to throw because chrome is undefined, got {value:?}"
    ),
    Err(_err) => {}
  }

  Ok(())
}

#[test]
fn chrome_global_is_not_exposed_by_default_handwritten_dom() -> Result<(), VmError> {
  let realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
  assert_chrome_not_exposed_by_default(realm)
}

#[test]
fn chrome_global_is_not_exposed_by_default_webidl_dom() -> Result<(), VmError> {
  let realm = WindowRealm::new(
    WindowRealmConfig::new("https://example.invalid/")
      .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
  )?;
  assert_chrome_not_exposed_by_default(realm)
}

