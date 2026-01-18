#![cfg(test)]

use std::io;
use std::net::TcpListener;
use std::sync::{Mutex, MutexGuard, OnceLock};

// Many tests spin up local TCP servers and run HTTP clients in parallel. When the test runner uses
// a very high thread count, localhost networking can get flaky (spurious connection failures).
// Serialize network-heavy tests behind a single global lock to keep CI deterministic.
//
// Note: integration tests cannot use this helper because the library crate is compiled without
// `cfg(test)` for integration-test targets (so `crate::testing` is not available). Those tests use
// `tests/common/net.rs` instead.
static NET_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn net_test_lock() -> MutexGuard<'static, ()> {
  match NET_TEST_LOCK.get_or_init(|| Mutex::new(())).lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

#[track_caller]
pub(crate) fn try_bind_localhost(context: &str) -> Option<TcpListener> {
  match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => Some(listener),
    Err(err)
      if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrNotAvailable
      ) =>
    {
      let loc = std::panic::Location::caller();
      eprintln!(
        "skipping {context} ({}:{}): cannot bind localhost in this environment: {err}",
        loc.file(),
        loc.line()
      );
      None
    }
    Err(err) => {
      let loc = std::panic::Location::caller();
      assert!(
        false,
        "bind {context} ({}:{}): {err}",
        loc.file(),
        loc.line()
      );
      None
    }
  }
}
