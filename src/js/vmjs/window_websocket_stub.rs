//! Stub WebSocket bindings for the `vm-js` Window realm.
//!
//! When the `direct_websocket` Cargo feature is disabled, FastRender must be able to build
//! renderer-side binaries without linking any in-process network stacks (including `tungstenite`).
//!
//! This module keeps the public API surface stable by providing no-op install functions and RAII
//! guards. The JS global `WebSocket` constructor is intentionally *not* installed.

use crate::js::window_realm::WindowRealmHost;
use std::sync::atomic::{AtomicU64, Ordering};
use vm_js::{Heap, Realm, Vm, VmError};

static NEXT_ENV_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct WindowWebSocketEnv {
  pub document_url: Option<String>,
}

impl WindowWebSocketEnv {
  pub fn for_document(document_url: Option<String>) -> Self {
    Self { document_url }
  }
}

pub fn unregister_window_websocket_env(_env_id: u64) {
  // No-op: direct WebSocket support is disabled.
}

#[derive(Debug)]
#[must_use = "websocket bindings are only valid while the returned WindowWebSocketBindings is kept alive"]
pub struct WindowWebSocketBindings {
  env_id: u64,
  active: bool,
}

impl WindowWebSocketBindings {
  fn new(env_id: u64) -> Self {
    Self { env_id, active: true }
  }

  pub fn env_id(&self) -> u64 {
    self.env_id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }
}

impl Drop for WindowWebSocketBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_window_websocket_env(self.env_id);
    }
  }
}

pub fn install_window_websocket_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_websocket_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

pub fn install_window_websocket_bindings_with_guard<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  _realm: &Realm,
  _heap: &mut Heap,
  _env: WindowWebSocketEnv,
) -> Result<WindowWebSocketBindings, VmError> {
  // Allocate an env id for debug parity (even though it isn't used for lookups).
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  Ok(WindowWebSocketBindings::new(env_id))
}

