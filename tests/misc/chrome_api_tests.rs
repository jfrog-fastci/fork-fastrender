use fastrender::js::{WindowRealm, WindowRealmConfig};
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::js::chrome_api::{install_chrome_api_bindings_vm_js, ChromeApiHost, ChromeCommand};
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::WindowRealmHost;
use std::any::Any;
use vm_js::{Heap, Job, Value, VmError, VmHost, VmHostHooks};

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

struct TestHost {
  vm_host: (),
  realm: WindowRealm,
  cmds: Vec<ChromeCommand>,
}

impl TestHost {
  fn new(config: WindowRealmConfig) -> Result<Self, VmError> {
    Ok(Self {
      vm_host: (),
      realm: WindowRealm::new(config)?,
      cmds: Vec::new(),
    })
  }
}

impl WindowRealmHost for TestHost {
  fn vm_host_and_window_realm(
    &mut self,
  ) -> fastrender::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
    Ok((&mut self.vm_host, &mut self.realm))
  }
}

impl ChromeApiHost for TestHost {
  fn chrome_dispatch(&mut self, cmd: ChromeCommand) -> Result<(), fastrender::error::Error> {
    self.cmds.push(cmd);
    Ok(())
  }
}

struct Hooks<Host: WindowRealmHost + 'static> {
  inner: VmJsEventLoopHooks<Host>,
}

impl<Host: WindowRealmHost + 'static> Hooks<Host> {
  fn new(host: &mut Host) -> fastrender::error::Result<Self> {
    Ok(Self {
      inner: VmJsEventLoopHooks::new_with_host(host)?,
    })
  }
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for Hooks<Host> {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    vm_js::VmHostHooks::as_any_mut(&mut self.inner)
  }
}

#[test]
fn chrome_tab_id_representation_is_safe_integer_number() -> Result<(), VmError> {
  const MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;

  let mut host = TestHost::new(WindowRealmConfig::new("https://example.invalid/"))?;
  {
    let (vm, realm_ref, heap) = host.realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm_ref)?;
  }

  let mut hooks = Hooks::<TestHost>::new(&mut host).expect("create hooks");

  let err_name = {
    let (vm_host, realm) = host.vm_host_and_window_realm().expect("split");

    // closeTab(MAX_SAFE_INTEGER) should work (no throw) and preserve the exact id.
    realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.tabs.closeTab(Number.MAX_SAFE_INTEGER)")?;

    // Out of safe integer range must throw TypeError.
    realm.exec_script_with_host_and_hooks(
      vm_host,
      &mut hooks,
      "try { chrome.tabs.closeTab(Number.MAX_SAFE_INTEGER + 1); 'no-error'; } catch (e) { e.name }",
    )?
  };

  assert_eq!(
    host.cmds.as_slice(),
    &[ChromeCommand::CloseTab {
      tab_id: MAX_SAFE_INTEGER,
    }]
  );
  assert_eq!(get_string(host.realm.heap(), err_name), "TypeError");
  Ok(())
}

#[test]
fn chrome_navigation_dispatches_commands_to_host() -> Result<(), VmError> {
  let mut host = TestHost::new(WindowRealmConfig::new("https://example.invalid/"))?;
  {
    let (vm, realm_ref, heap) = host.realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm_ref)?;
  }

  let mut hooks = Hooks::<TestHost>::new(&mut host).expect("create hooks");
  {
    let (vm_host, realm) = host.vm_host_and_window_realm().expect("split");

    let writable = realm.exec_script_with_host_and_hooks(
      vm_host,
      &mut hooks,
      "Object.getOwnPropertyDescriptor(chrome, 'navigation').writable",
    )?;
    assert_eq!(writable, Value::Bool(false));

    realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.navigation.back()")?;
    realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.navigation.forward()")?;
    realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.navigation.reload()")?;
    realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, "chrome.navigation.stop()")?;
    realm.exec_script_with_host_and_hooks(
      vm_host,
      &mut hooks,
      "chrome.navigation.navigate('https://example.com')",
    )?;
  }

  assert_eq!(
    host.cmds.as_slice(),
    &[
      ChromeCommand::Back,
      ChromeCommand::Forward,
      ChromeCommand::Reload,
      ChromeCommand::Stop,
      ChromeCommand::Navigate {
        url: "https://example.com".to_string(),
      },
    ]
  );
  Ok(())
}

#[test]
fn chrome_navigation_navigate_validates_type_and_size() -> Result<(), VmError> {
  let mut host = TestHost::new(WindowRealmConfig::new("https://example.invalid/"))?;
  {
    let (vm, realm_ref, heap) = host.realm.vm_realm_and_heap_mut();
    install_chrome_api_bindings_vm_js::<TestHost>(vm, heap, realm_ref)?;
  }

  let mut hooks = Hooks::<TestHost>::new(&mut host).expect("create hooks");

  let (err_name_non_string, err_name_too_large) = {
    let (vm_host, realm) = host.vm_host_and_window_realm().expect("split");

    let err_name_non_string = realm.exec_script_with_host_and_hooks(
      vm_host,
      &mut hooks,
      "try { chrome.navigation.navigate(123); 'no-error'; } catch (e) { e.name }",
    )?;

    let err_name_too_large = realm.exec_script_with_host_and_hooks(
      vm_host,
      &mut hooks,
      "try { chrome.navigation.navigate('a'.repeat(9000)); 'no-error'; } catch (e) { e.name }",
    )?;

    (err_name_non_string, err_name_too_large)
  };

  assert_eq!(get_string(host.realm.heap(), err_name_non_string), "TypeError");
  assert_eq!(get_string(host.realm.heap(), err_name_too_large), "TypeError");
  assert!(host.cmds.is_empty());
  Ok(())
}
