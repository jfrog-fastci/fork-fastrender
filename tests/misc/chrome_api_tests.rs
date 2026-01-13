use fastrender::js::{WindowRealm, WindowRealmConfig};
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::js::chrome_api::{install_chrome_api_bindings_vm_js, ChromeApiHandler, ChromeTabInfo};
use vm_js::{Heap, Value, VmError};
use std::sync::{Arc, Mutex};

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

#[derive(Default)]
struct RecordingChromeHandler {
  closed: Mutex<Vec<u64>>,
  activated: Mutex<Vec<u64>>,
  navigated: Mutex<Vec<String>>,
  go_back: Mutex<u32>,
  go_forward: Mutex<u32>,
  reload: Mutex<u32>,
  stop: Mutex<u32>,
}

impl ChromeApiHandler for RecordingChromeHandler {
  fn new_tab(&self, _url: Option<String>) -> u64 {
    1
  }

  fn close_tab(&self, id: u64) {
    self.closed.lock().unwrap().push(id);
  }

  fn activate_tab(&self, id: u64) {
    self.activated.lock().unwrap().push(id);
  }

  fn tabs_snapshot(&self) -> Vec<ChromeTabInfo> {
    vec![ChromeTabInfo {
      id: (1u64 << 53) - 1, // Number.MAX_SAFE_INTEGER
      url: "https://example.com/".to_string(),
      title: "Example".to_string(),
      active: true,
    }]
  }

  fn navigate(&self, input: String) {
    self.navigated.lock().unwrap().push(input);
  }

  fn go_back(&self) {
    *self.go_back.lock().unwrap() += 1;
  }

  fn go_forward(&self) {
    *self.go_forward.lock().unwrap() += 1;
  }

  fn reload(&self) {
    *self.reload.lock().unwrap() += 1;
  }

  fn stop(&self) {
    *self.stop.lock().unwrap() += 1;
  }
}

#[test]
fn chrome_tab_id_representation_is_safe_integer_number() -> Result<(), VmError> {
  const MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;

  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
  let handler = Arc::new(RecordingChromeHandler::default());

  let _bindings = {
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js(vm, heap, realm_ref, handler.clone())?
  };

  // closeTab(MAX_SAFE_INTEGER) should work (no throw) and preserve the exact id.
  realm.exec_script("chrome.tabs.closeTab(Number.MAX_SAFE_INTEGER)")?;
  assert_eq!(
    handler.closed.lock().unwrap().as_slice(),
    &[MAX_SAFE_INTEGER]
  );

  // Out of safe integer range must throw TypeError.
  let err_name = realm.exec_script(
    "try { chrome.tabs.closeTab(Number.MAX_SAFE_INTEGER + 1); 'no-error'; } catch (e) { e.name }",
  )?;
  assert_eq!(get_string(realm.heap(), err_name), "TypeError");

  // Non-integers must throw TypeError.
  let err_name = realm.exec_script("try { chrome.tabs.closeTab(1.5); 'no-error'; } catch (e) { e.name }")?;
  assert_eq!(get_string(realm.heap(), err_name), "TypeError");

  // Snapshot getters must surface ids as safe integers.
  let id = realm.exec_script("chrome.tabs.getAll()[0].id")?;
  let Value::Number(n) = id else {
    panic!("expected Number id, got {id:?}");
  };
  assert_eq!(n, MAX_SAFE_INTEGER as f64);
  Ok(())
}

#[test]
fn chrome_navigation_dispatches_to_handler() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
  let handler = Arc::new(RecordingChromeHandler::default());

  let _bindings = {
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js(vm, heap, realm_ref, handler.clone())?
  };

  let writable = realm.exec_script("Object.getOwnPropertyDescriptor(chrome, 'navigation').writable")?;
  assert_eq!(writable, Value::Bool(false));

  realm.exec_script("chrome.navigation.goBack()")?;
  realm.exec_script("chrome.navigation.goForward()")?;
  realm.exec_script("chrome.navigation.reload()")?;
  realm.exec_script("chrome.navigation.stop()")?;
  realm.exec_script("chrome.navigation.navigate('https://example.com')")?;

  assert_eq!(*handler.go_back.lock().unwrap(), 1);
  assert_eq!(*handler.go_forward.lock().unwrap(), 1);
  assert_eq!(*handler.reload.lock().unwrap(), 1);
  assert_eq!(*handler.stop.lock().unwrap(), 1);
  assert_eq!(
    handler.navigated.lock().unwrap().as_slice(),
    &["https://example.com".to_string()]
  );

  Ok(())
}

#[test]
fn chrome_navigation_navigate_validates_type_and_size() -> Result<(), VmError> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
  let handler = Arc::new(RecordingChromeHandler::default());

  let _bindings = {
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js(vm, heap, realm_ref, handler.clone())?
  };

  let err_name = realm.exec_script(
    "try { chrome.navigation.navigate(123); 'no-error'; } catch (e) { e.name }",
  )?;
  assert_eq!(get_string(realm.heap(), err_name), "TypeError");
  assert!(handler.navigated.lock().unwrap().is_empty());

  let err_name = realm.exec_script(
    "try { chrome.navigation.navigate('a'.repeat(9000)); 'no-error'; } catch (e) { e.name }",
  )?;
  assert_eq!(get_string(realm.heap(), err_name), "TypeError");
  assert!(handler.navigated.lock().unwrap().is_empty());

  Ok(())
}
