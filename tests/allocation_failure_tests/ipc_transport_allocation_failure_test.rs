use super::{fail_next_allocation, failed_allocs, lock_allocator};
use fastrender::ipc::transport::Transport;
use fastrender::ipc::IpcError;

#[test]
fn ipc_transport_recv_returns_error_on_payload_allocation_failure() {
  let _guard = lock_allocator();

  // Use a payload length that's large enough to avoid colliding with small internal allocations
  // (like formatting error messages), but still well below the IPC framing cap.
  let payload_len = 4096usize;
  let mut bytes = Vec::new();
  bytes.extend_from_slice(&(payload_len as u32).to_le_bytes());

  let mut transport = Transport::new(std::io::Cursor::new(bytes), std::io::sink());

  let start_failures = failed_allocs();
  fail_next_allocation(payload_len, 1);

  let err = transport
    .recv::<u32>()
    .expect_err("expected IPC payload allocation failure to surface as an error");
  assert!(
    matches!(err, IpcError::Io(_)),
    "expected IpcError::Io on allocation failure, got {err:?}"
  );
  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger exactly one allocation failure"
  );
}

#[cfg(unix)]
#[test]
fn ipc_transport_recv_with_timeout_returns_error_on_payload_allocation_failure() {
  use std::io::Write;
  use std::os::unix::net::UnixStream;
  use std::time::Duration;

  let _guard = lock_allocator();

  let payload_len = 4096usize;
  let (a, mut b) = UnixStream::pair().expect("socketpair");
  b.write_all(&(payload_len as u32).to_le_bytes())
    .expect("write length prefix");

  let reader = a.try_clone().expect("try_clone");
  let mut transport = Transport::new(reader, a);

  let start_failures = failed_allocs();
  fail_next_allocation(payload_len, 1);

  let err = transport
    .recv_with_timeout::<u32>(Duration::from_millis(50))
    .expect_err("expected IPC payload allocation failure to surface as an error");
  assert!(
    matches!(err, IpcError::Io(_)),
    "expected IpcError::Io on allocation failure, got {err:?}"
  );
  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger exactly one allocation failure"
  );
}

