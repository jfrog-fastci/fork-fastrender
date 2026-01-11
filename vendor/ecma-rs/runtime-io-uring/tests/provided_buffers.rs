#![cfg(target_os = "linux")]

use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;

use runtime_io_uring::{Completion, Driver, PreparedOp, ProvidedBufPool};

fn is_uring_unavailable(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
}

#[test]
fn recv_with_buf_select_recycles_buffers() {
    let mut driver = match Driver::new(8) {
        Ok(d) => d,
        Err(e) if is_uring_unavailable(&e) => {
            eprintln!("skipping: io_uring unavailable: {e}");
            return;
        }
        Err(e) => return Err(e).unwrap(),
    };

    let provide_supported = match runtime_io_uring::is_provide_buffers_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !provide_supported {
        eprintln!("skipping: IORING_OP_PROVIDE_BUFFERS not supported by kernel");
        return;
    }

    let pool = ProvidedBufPool::new(&driver, 1, 8, 1).unwrap();

    let mut fds = [0; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0);

    // SAFETY: `socketpair` returns two valid FDs on success.
    let mut tx = unsafe { UnixStream::from_raw_fd(fds[0]) };
    let rx = unsafe { UnixStream::from_raw_fd(fds[1]) };

    tx.write_all(b"hello").unwrap();
    let op1 = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();
    let buf1 = match driver.wait().unwrap() {
        Completion::ProvidedBuf { id, buf } => {
            assert_eq!(id, op1);
            buf
        }
        other => panic!("unexpected completion: {other:?}"),
    };
    assert_eq!(buf1.buf_id(), 0);
    assert_eq!(buf1.as_slice(), b"hello");
    assert_eq!(pool.stats().leased, 1);
    assert_eq!(pool.stats().in_kernel, 0);

    // With only one provided buffer, a second recv should fail until `buf1` is dropped and
    // the buffer is re-provided to the kernel.
    tx.write_all(b"world").unwrap();
    let op2 = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();
    match driver.wait().unwrap() {
        Completion::Op { id, res, op } => {
            assert_eq!(id, op2);
            assert_eq!(res, -libc::ENOBUFS);
            // Ensure we didn't get a normal read op completion by accident.
            match op {
                PreparedOp::RecvWithBufSelect { .. } => {}
                _ => panic!("unexpected op in completion: {op:?}"),
            }
        }
        other => panic!("unexpected completion: {other:?}"),
    }

    let reprovided_before = pool.stats().reprovided;
    drop(buf1);
    assert_eq!(pool.stats().reprovided, reprovided_before + 1);

    let op3 = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();
    let buf2 = match driver.wait().unwrap() {
        Completion::ProvidedBuf { id, buf } => {
            assert_eq!(id, op3);
            buf
        }
        other => panic!("unexpected completion: {other:?}"),
    };
    assert_eq!(buf2.buf_id(), 0);
    assert_eq!(buf2.as_slice(), b"world");
    drop(buf2);

    let stats = pool.stats();
    assert_eq!(stats.leased, 0);
    assert_eq!(stats.in_kernel, 1);
}

#[test]
fn drop_driver_with_inflight_buf_select_does_not_cycle() {
    let mut driver = match Driver::new(8) {
        Ok(d) => d,
        Err(e) if is_uring_unavailable(&e) => {
            eprintln!("skipping: io_uring unavailable: {e}");
            return;
        }
        Err(e) => return Err(e).unwrap(),
    };

    let provide_supported = match runtime_io_uring::is_provide_buffers_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !provide_supported {
        eprintln!("skipping: IORING_OP_PROVIDE_BUFFERS not supported by kernel");
        return;
    }

    // Use a single buffer so holding on to `buf1` keeps the pool "empty" and `op2` stays in the
    // driver's `ops` map (even if it completes immediately with `-ENOBUFS`).
    let pool = ProvidedBufPool::new(&driver, 1, 8, 1).unwrap();

    let mut fds = [0; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0);

    // SAFETY: `socketpair` returns two valid FDs on success.
    let mut tx = unsafe { UnixStream::from_raw_fd(fds[0]) };
    let rx = unsafe { UnixStream::from_raw_fd(fds[1]) };

    tx.write_all(b"hello").unwrap();
    let op1 = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();
    let buf1 = match driver.wait().unwrap() {
        Completion::ProvidedBuf { id, buf } => {
            assert_eq!(id, op1);
            buf
        }
        other => panic!("unexpected completion: {other:?}"),
    };

    // Submit a second op and intentionally never call `wait()` again.
    tx.write_all(b"world").unwrap();
    let _op2 = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();

    let weak_driver = driver.downgrade();
    drop(driver);

    // Regression test: Previously the pool held a strong `Driver`, creating a reference cycle:
    // `Driver -> ops -> PreparedOp -> ProvidedBufPool -> Driver`.
    assert!(weak_driver.upgrade().is_none());

    // Dropping leased buffers after the driver is already gone should not panic and should update
    // pool accounting (but can't re-provide to the kernel).
    let reprovided_before = pool.stats().reprovided;
    drop(buf1);
    assert_eq!(pool.stats().reprovided, reprovided_before);
    assert_eq!(pool.stats().leased, 0);
}
