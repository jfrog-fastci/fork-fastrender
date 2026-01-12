use fastrender::dom2;
use fastrender::js::bindings::{
  install_url_bindings_vm_js, install_url_search_params_bindings_vm_js,
};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use vm_js::{PropertyKey, Value};

fn delete_global_prop(host: &mut fastrender::js::WindowHost, name: &str) -> Result<()> {
  let window = host.host_mut().window_mut();
  let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();

  scope
    .push_root(Value::Object(global))
    .map_err(|err| Error::Other(err.to_string()))?;
  let key_s = scope
    .alloc_string(name)
    .map_err(|err| Error::Other(err.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|err| Error::Other(err.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  scope
    .delete_property_or_throw(global, key)
    .map_err(|err| Error::Other(err.to_string()))?;
  Ok(())
}

fn value_to_utf8(host: &mut fastrender::js::WindowHost, value: Value) -> Result<String> {
  let window = host.host_mut().window_mut();
  let (_vm, _realm, heap) = window.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  scope
    .push_root(value)
    .map_err(|err| Error::Other(err.to_string()))?;
  let s = scope
    .heap_mut()
    .to_string(value)
    .map_err(|err| Error::Other(err.to_string()))?;
  Ok(
    scope
      .heap()
      .get_string(s)
      .map_err(|err| Error::Other(err.to_string()))?
      .to_utf8_lossy(),
  )
}

#[test]
fn vm_js_install_only_url_search_params_does_not_clobber_dom() -> Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = fastrender::js::WindowHost::new(dom, "https://example.invalid/")?;

  // Ensure we're starting from the default realm surface (handwritten DOM bindings present).
  let el = host.exec_script("document.createElement('div')")?;
  assert!(
    matches!(el, Value::Object(_)),
    "expected document.createElement to return an object"
  );

  // Replace the handcrafted URLSearchParams binding with the generated WebIDL binding.
  delete_global_prop(&mut host, "URLSearchParams")?;
  {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_url_search_params_bindings_vm_js(vm, heap, realm)
      .map_err(|err| Error::Other(err.to_string()))?;
  }

  let out = host.exec_script("new URLSearchParams('a=1').get('a')")?;
  assert_eq!(value_to_utf8(&mut host, out)?, "1");

  // Existing DOM globals should still work after incrementally installing a single WebIDL binding.
  let out = host.exec_script("document.createElement('span').tagName")?;
  assert_eq!(value_to_utf8(&mut host, out)?, "SPAN");

  Ok(())
}

#[test]
fn vm_js_install_url_and_url_search_params_still_work() -> Result<()> {
  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = fastrender::js::WindowHost::new(dom, "https://example.invalid/")?;

  // Replace the handcrafted URL + URLSearchParams bindings with the generated WebIDL bindings.
  delete_global_prop(&mut host, "URL")?;
  delete_global_prop(&mut host, "URLSearchParams")?;
  {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_url_bindings_vm_js(vm, heap, realm).map_err(|err| Error::Other(err.to_string()))?;
    install_url_search_params_bindings_vm_js(vm, heap, realm)
      .map_err(|err| Error::Other(err.to_string()))?;
  }

  let out = host.exec_script("URL.canParse('https://example.com/')")?;
  assert_eq!(out, Value::Bool(true));

  let origin = host.exec_script("new URL('https://example.com/a/b').origin")?;
  assert_eq!(value_to_utf8(&mut host, origin)?, "https://example.com");

  let out = host.exec_script("new URLSearchParams('a=1&b=2').get('b')")?;
  assert_eq!(value_to_utf8(&mut host, out)?, "2");

  // DOM bindings should remain usable after swapping in the generated WHATWG URL bindings.
  let out = host.exec_script("document.createElement('div').tagName")?;
  assert_eq!(value_to_utf8(&mut host, out)?, "DIV");

  Ok(())
}
