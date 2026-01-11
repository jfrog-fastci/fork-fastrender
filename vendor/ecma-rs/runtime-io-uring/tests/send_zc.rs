#![cfg(all(target_os = "linux", feature = "send_zc"))]

use std::io::Read;
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::time::Duration;
use std::time::Instant;

use runtime_io_uring::GcIoBuf;
use runtime_io_uring::IoUringDriver;
use runtime_io_uring::mock_gc::MockGc;
use runtime_io_uring::SendZcFlags;

const IORING_CQE_F_MORE: u32 = 1 << 1;

fn set_sockopt_int(fd: i32, level: i32, opt: i32, value: i32) -> std::io::Result<()> {
  let rc = unsafe {
    libc::setsockopt(
      fd,
      level,
      opt,
      (&value as *const i32).cast(),
      std::mem::size_of::<i32>() as _,
    )
  };
  if rc == 0 {
    return Ok(());
  }
  Err(std::io::Error::last_os_error())
}

fn tcp_pair() -> std::io::Result<(TcpStream, TcpStream)> {
  let listener = TcpListener::bind(("127.0.0.1", 0))?;
  let addr = listener.local_addr()?;

  let client = TcpStream::connect(addr)?;
  let (server, _) = listener.accept()?;

  Ok((client, server))
}

#[test]
fn send_zc_observes_notif_and_keeps_buffer_pinned_until_release() {
  let mut driver = match IoUringDriver::new(8) {
    Ok(d) => d,
    Err(err) => {
      eprintln!("skipping: failed to create io_uring instance: {err}");
      return;
    }
  };

  let (send_sock, mut recv_sock) = tcp_pair().unwrap();
  send_sock.set_nodelay(true).unwrap();
  recv_sock.set_nodelay(true).unwrap();

  // `SEND_ZC` relies on `SO_ZEROCOPY`. Not all kernels/configs support it, so
  // treat it as a skip.
  match set_sockopt_int(send_sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ZEROCOPY, 1) {
    Ok(()) => {}
    Err(err) => {
      eprintln!("skipping SEND_ZC test: SO_ZEROCOPY unsupported ({err})");
      return;
    }
  }

  // Help avoid backpressure-induced delays (notification CQE can be delayed until
  // skb/page lifetimes end).
  let _ = set_sockopt_int(
    send_sock.as_raw_fd(),
    libc::SOL_SOCKET,
    libc::SO_SNDBUF,
    4 * 1024 * 1024,
  );
  let _ = set_sockopt_int(
    recv_sock.as_raw_fd(),
    libc::SOL_SOCKET,
    libc::SO_RCVBUF,
    4 * 1024 * 1024,
  );

  recv_sock
    .set_read_timeout(Some(Duration::from_secs(5)))
    .unwrap();

  let len = 256 * 1024;
  let mut bytes = vec![0u8; len];
  for (i, b) in bytes.iter_mut().enumerate() {
    *b = (i % 251) as u8;
  }

  let gc = MockGc::new();
  let handle = gc.alloc(bytes);
  let ptr_before = gc.ptr(handle).expect("missing object before send");

  let buf = GcIoBuf::from_gc(&gc, handle);
  assert_eq!(gc.pin_count(handle), 1);

  let op = driver
    .submit_send_zc(
      send_sock.as_raw_fd(),
      buf,
      SendZcFlags {
        msg_flags: libc::MSG_ZEROCOPY | libc::MSG_NOSIGNAL,
        zc_flags: 0,
      },
    )
    .unwrap();

  let deadline = Instant::now() + Duration::from_secs(5);
  let mut completion = None;
  let mut did_collect = false;
  let mut wakeups = 0usize;
  let mut first_wait_n = 0usize;

  while completion.is_none() {
    assert!(Instant::now() < deadline, "timed out waiting for CQEs");
    wakeups += 1;
    let n = driver.wait_for_cqe().unwrap();
    if wakeups == 1 {
      first_wait_n = n;
    }

    completion = op.try_take_completion();
    if completion.is_none() {
      assert_eq!(
        gc.pin_drops(handle),
        0,
        "pin guard dropped before SEND_ZC op completed"
      );
      assert_eq!(
        gc.root_drops(handle),
        0,
        "root dropped before SEND_ZC op completed"
      );

      // Simulate a moving GC cycle while the kernel may still hold pinned pages. The pin guard held
      // by the in-flight op must keep the backing store from relocating.
      if !did_collect {
        gc.collect();
        let ptr_after = gc.ptr(handle).expect("object collected while op is in-flight");
        assert_eq!(ptr_before, ptr_after, "buffer relocated while op is in-flight");
        did_collect = true;
      }
    }
  }

  let completion = completion.unwrap();

  // Common "not supported" error codes.
  if completion.result == -libc::EINVAL
    || completion.result == -libc::EOPNOTSUPP
    || completion.result == -libc::ENOSYS
  {
    eprintln!(
      "skipping SEND_ZC test: IORING_OP_SEND_ZC unsupported (res={})",
      completion.result
    );
    return;
  }

  assert!(
    completion.result >= 0,
    "send_zc failed: {}",
    completion.result
  );

  let flags = completion.resource.send_flags;

  // Receiver must see the correct bytes.
  let mut received = vec![0u8; completion.result as usize];
  recv_sock.read_exact(&mut received).unwrap();
  for (i, b) in received.iter().enumerate() {
    assert_eq!(*b, (i % 251) as u8);
  }

  // If the kernel won't send a notification CQE, we can't validate the extended buffer lifetime
  // invariant.
  if (flags & IORING_CQE_F_MORE) == 0 {
    eprintln!(
      "skipping SEND_ZC test: no notification CQE expected (flags={flags})"
    );
    return;
  }

  assert!(
    completion.resource.notif.is_some(),
    "SEND_ZC expected a notification CQE (IORING_CQE_F_MORE) but none was recorded"
  );

  if wakeups == 1 && first_wait_n == 1 {
    // If we only observed a single CQE wake-up but the op indicated it would emit a notification,
    // then the driver must have released the buffer early.
    //
    // Note: if a future kernel ever collapses send+notif into a single CQE, this assertion should
    // be revisited. Current `SEND_ZC` semantics are two CQEs (send + notif).
    panic!(
      "SEND_ZC op reported notification pending (IORING_CQE_F_MORE) but completed after processing a single CQE"
    );
  }

  // Pins should still be held until the completion is dropped (resource owns the buffer).
  assert_eq!(gc.pin_drops(handle), 0);
  assert_eq!(gc.root_drops(handle), 0);
  drop(completion);

  assert_eq!(gc.pin_drops(handle), 1);
  assert_eq!(gc.root_drops(handle), 1);
}
