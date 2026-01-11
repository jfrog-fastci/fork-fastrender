#![cfg(target_os = "linux")]

use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;

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
    let remove_supported = match runtime_io_uring::is_remove_buffers_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !remove_supported {
        eprintln!("skipping: IORING_OP_REMOVE_BUFFERS not supported by kernel");
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

    // `LeasedBuf` drops submit `IORING_OP_PROVIDE_BUFFERS` to re-provide buffers back to the
    // kernel. Those ops are tracked and must be drained before dropping the driver in debug
    // builds (the driver panics on drop with in-flight ops).
    let slab_drops = pool.debug_slab_drop_counter();
    drop(pool);

    // Drive the ring until the tracked provide CQE(s) are observed and the pool slab is freed.
    let mut pipe_fds = [0; 2];
    let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    assert_eq!(rc, 0);
    // SAFETY: `pipe` returns two valid FDs on success.
    let mut pipe_tx = unsafe { std::fs::File::from_raw_fd(pipe_fds[1]) };
    let pipe_rx = unsafe { std::fs::File::from_raw_fd(pipe_fds[0]) };

    for _ in 0..128 {
        if slab_drops.load(Ordering::SeqCst) != 0 {
            break;
        }
        pipe_tx.write_all(b"z").unwrap();
        let id = driver
            .submit(PreparedOp::read(pipe_rx.as_raw_fd(), vec![0u8; 1]))
            .unwrap();
        match driver.wait().unwrap() {
            Completion::Op { id: got_id, res, .. } => {
                assert_eq!(got_id, id);
                assert_eq!(res, 1);
            }
            other => panic!("unexpected completion: {other:?}"),
        }
    }

    assert_eq!(
        slab_drops.load(Ordering::SeqCst),
        1,
        "expected provide CQE(s) to be drained before driver drop"
    );
}

#[test]
fn drop_driver_with_leased_buf_select_does_not_keep_driver_alive() {
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

    // Use a single buffer so holding on to `buf1` keeps the pool "empty".
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

    let weak_driver = driver.downgrade();
    drop(driver);

    // Regression test: Previously the pool held a strong `Driver`, keeping the driver alive even
    // after the last handle was dropped.
    assert!(weak_driver.upgrade().is_none());

    // Dropping leased buffers after the driver is already gone should not panic and should update
    // pool accounting (but can't re-provide to the kernel).
    let reprovided_before = pool.stats().reprovided;
    drop(buf1);
    assert_eq!(pool.stats().reprovided, reprovided_before);
    assert_eq!(pool.stats().leased, 0);
}

#[test]
fn reprovide_keeps_slab_alive_until_cqe_drained() {
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
    let remove_supported = match runtime_io_uring::is_remove_buffers_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !remove_supported {
        eprintln!("skipping: IORING_OP_REMOVE_BUFFERS not supported by kernel");
        return;
    }

    let pool = ProvidedBufPool::new(&driver, 2, 8, 1).unwrap();
    let slab_drops = pool.debug_slab_drop_counter();

    // Lease one buffer via a buf-select recv.
    let mut fds = [0; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0);
    // SAFETY: `socketpair` returns two valid FDs on success.
    let mut tx = unsafe { UnixStream::from_raw_fd(fds[0]) };
    let rx = unsafe { UnixStream::from_raw_fd(fds[1]) };

    tx.write_all(b"x").unwrap();
    let op = driver
        .submit(PreparedOp::recv_with_buf_select(rx.as_raw_fd(), pool.clone()))
        .unwrap();
    let buf = match driver.wait().unwrap() {
        Completion::ProvidedBuf { id, buf } => {
            assert_eq!(id, op);
            buf
        }
        other => panic!("unexpected completion: {other:?}"),
    };

    // Schedule a tracked re-provide, then drop the pool immediately. The slab must stay alive
    // until the kernel completes the provide op and the driver drains its CQE.
    drop(buf);
    drop(pool);
    assert_eq!(
        slab_drops.load(Ordering::SeqCst),
        0,
        "provided-buffer slab dropped before PROVIDE_BUFFERS CQE was observed"
    );

    // Drive the ring until the PROVIDE_BUFFERS CQE is drained and the pool slab is freed.
    let mut pipe_fds = [0; 2];
    let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    assert_eq!(rc, 0);
    // SAFETY: `pipe` returns two valid FDs on success.
    let mut pipe_tx = unsafe { std::fs::File::from_raw_fd(pipe_fds[1]) };
    let pipe_rx = unsafe { std::fs::File::from_raw_fd(pipe_fds[0]) };

    for _ in 0..128 {
        if slab_drops.load(Ordering::SeqCst) != 0 {
            break;
        }
        pipe_tx.write_all(b"y").unwrap();
        let id = driver
            .submit(PreparedOp::read(pipe_rx.as_raw_fd(), vec![0u8; 1]))
            .unwrap();
        match driver.wait().unwrap() {
            Completion::Op { id: got_id, res, .. } => {
                assert_eq!(got_id, id);
                assert_eq!(res, 1);
            }
            other => panic!("unexpected completion: {other:?}"),
        }
    }

    assert_eq!(
        slab_drops.load(Ordering::SeqCst),
        1,
        "expected slab to be dropped after driver drained provide CQE"
    );
}
