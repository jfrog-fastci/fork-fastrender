use super::{
  install_url_bindings_vm_js, install_url_search_params_bindings_vm_js, install_window_bindings_vm_js,
};
use crate::js::{JsExecutionOptions, RunLimits, RunUntilIdleOutcome, WindowHost};
use crate::{Error, Result};
use selectors::context::QuirksMode;
use std::time::Duration;
use vm_js::{PropertyKey, Value};

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time. The library default is intentionally aggressive,
  // but under parallel `cargo test` the OS can deschedule a test thread long enough for the VM to
  // observe a false-positive deadline exceed. Use a generous limit to keep unit tests deterministic
  // while still bounding infinite loops.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
  opts
}

fn delete_global_prop(host: &mut WindowHost, name: &str) -> Result<()> {
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

fn value_to_utf8(host: &mut WindowHost, value: Value) -> Result<String> {
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

fn install_generated_window_bindings(host: &mut WindowHost) -> Result<()> {
  // WindowRealm installs handcrafted bindings by default (`src/js/vmjs/window_url.rs`,
  // `src/js/vmjs/window_timers.rs`). The generated bindings are idempotent and intentionally do not
  // clobber existing globals, so delete the existing globals first to ensure the executed script
  // hits `webidl_vm_js::host_from_hooks()`.
  {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .map_err(|err| Error::Other(err.to_string()))?;

    for name in ["URL", "URLSearchParams", "setTimeout", "clearTimeout"] {
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
    }
  }
  {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_bindings_vm_js(vm, heap, realm).map_err(|err| Error::Other(err.to_string()))?;
  }
  Ok(())
}

#[test]
fn vm_js_webidl_generated_url_and_search_params_work_in_window_host() -> Result<()> {
  let dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_opts_for_test())?;
  install_generated_window_bindings(&mut host)?;

  let got = host.exec_script(
    r#"
    (() => {
      const u = new URL("https://example.com/a?b=c");
      return u.href === "https://example.com/a?b=c"
        && u.origin === "https://example.com"
        && u.searchParams.get("b") === "c"
        && new URLSearchParams("a=b").get("a") === "b";
    })()
    "#,
  )?;
  assert_eq!(got, Value::Bool(true));
  Ok(())
}

#[test]
fn vm_js_webidl_generated_set_timeout_and_clear_timeout_work_in_window_host() -> Result<()> {
  let dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_js_execution_options(dom, "https://example.invalid/", js_opts_for_test())?;
  install_generated_window_bindings(&mut host)?;

  let _ = host.exec_script(
    r#"
    globalThis.__ran1 = 0;
    globalThis.__ran2 = 0;
    const id = setTimeout(() => { __ran1++; }, 0);
    clearTimeout(id);
    setTimeout(() => { __ran2++; }, 0);
    "#,
  )?;

  // Drive the event loop until the 0ms timer is delivered.
  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let got = host.exec_script("globalThis.__ran1 === 0 && globalThis.__ran2 === 1")?;
  assert_eq!(got, Value::Bool(true));
  Ok(())
}

#[test]
fn vm_js_install_only_url_search_params_does_not_clobber_dom() -> Result<()> {
  let dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.invalid/")?;

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
  let dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.invalid/")?;

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

