#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use runtime_io_uring::{Driver, PreparedOp};

fn is_uring_unavailable(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
}

struct Fd(RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn pipe() -> io::Result<(Fd, Fd)> {
    let mut fds = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((Fd(fds[0]), Fd(fds[1])))
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn drop_driver_with_inflight_op_does_not_drop_keep_alive() {
    let mut driver = match Driver::new(8) {
        Ok(d) => d,
        Err(e) if is_uring_unavailable(&e) => {
            eprintln!("skipping: io_uring unavailable: {e}");
            return;
        }
        Err(e) => return Err(e).unwrap(),
    };

    // Submit a read that will stay in-flight (no data is ever written and the write end stays open).
    let (read_fd, _write_fd) = pipe().unwrap();

    let drops = Arc::new(AtomicUsize::new(0));
    driver
        .submit(PreparedOp::read_with_keep_alive(
            read_fd.0,
            vec![0u8; 8],
            DropCounter(Arc::clone(&drops)),
        ))
        .unwrap();

    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // If the legacy driver dropped its op table (and therefore op-owned buffers/metadata) while the
    // kernel still held pointers, this would be a UAF. The driver is expected to leak the ring and
    // in-flight ops instead.
    drop(driver);

    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "keep_alive resources were dropped while the op was still in-flight"
    );
}

