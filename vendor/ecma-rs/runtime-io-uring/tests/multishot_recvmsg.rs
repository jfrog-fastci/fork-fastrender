#![cfg(target_os = "linux")]

use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use runtime_io_uring::{Completion, Driver, MultiShotRecvMsgEvent, ProvidedBufPool};

fn is_uring_unavailable(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
    )
}

fn is_multishot_unsupported_res(res: i32) -> bool {
    if res >= 0 {
        return false;
    }
    matches!(-res, libc::EINVAL | libc::EOPNOTSUPP | libc::ENOSYS)
}

#[test]
fn recvmsg_multishot_yields_multiple_messages_and_stops() {
    let mut driver = match Driver::new(64) {
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

    let cancel_supported = match runtime_io_uring::is_async_cancel_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !cancel_supported {
        eprintln!("skipping: IORING_OP_ASYNC_CANCEL not supported by kernel");
        return;
    }

    let pool = ProvidedBufPool::new(&driver, 1, 2048, 16).unwrap();
    let (rx, tx) = UnixDatagram::pair().unwrap();

    let handle = match driver.submit_recvmsg_multishot(rx.as_raw_fd(), &pool, 0) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping: failed to submit recvmsg multishot: {e}");
            return;
        }
    };

    let id = handle.id();
    assert!(driver.is_multishot_active(id).unwrap());

    let msgs = [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()];
    for msg in msgs {
        tx.send(msg).unwrap();

        loop {
            match driver.wait().unwrap() {
                Completion::MultiShotRecvMsg { id: got, event } if got == id => match event {
                    MultiShotRecvMsgEvent::Shot(shot) => {
                        assert_eq!(shot.payload(), msg);
                        assert!(shot.more(), "multishot CQE should have MORE set while active");
                        break;
                    }
                    MultiShotRecvMsgEvent::Err(err) => {
                        if is_multishot_unsupported_res(err.res) {
                            eprintln!(
                                "skipping: recvmsg multishot appears unsupported (res={})",
                                err.res
                            );
                            return;
                        }
                        panic!("unexpected multishot error: {err:?}");
                    }
                    MultiShotRecvMsgEvent::End(end) => {
                        if is_multishot_unsupported_res(end.res) {
                            eprintln!(
                                "skipping: recvmsg multishot appears unsupported (res={})",
                                end.res
                            );
                            return;
                        }
                        panic!("unexpected multishot End event while still sending: {end:?}");
                    }
                },
                // Ignore unrelated completions.
                _ => continue,
            }
        }
    }

    handle.stop().unwrap();
    assert!(driver.is_multishot_active(id).unwrap());

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match driver.wait().unwrap() {
            Completion::MultiShotRecvMsg { id: got, event } if got == id => match event {
                MultiShotRecvMsgEvent::End(end) => {
                    assert!(!end.more, "final CQE must not have MORE set");
                    break;
                }
                // Ignore any remaining non-terminal CQEs.
                _ => {}
            },
            _ => {}
        }

        if Instant::now() > deadline {
            panic!("timed out waiting for multishot termination");
        }
    }

    assert!(!driver.is_multishot_active(id).unwrap());
}

#[test]
fn dropping_handle_early_keeps_resources_until_final_cqe() {
    let mut driver = match Driver::new(64) {
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

    let cancel_supported = match runtime_io_uring::is_async_cancel_supported(&driver) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("skipping: failed to probe io_uring ops: {err}");
            return;
        }
    };
    if !cancel_supported {
        eprintln!("skipping: IORING_OP_ASYNC_CANCEL not supported by kernel");
        return;
    }

    let pool = ProvidedBufPool::new(&driver, 2, 2048, 8).unwrap();
    let (rx, _tx) = UnixDatagram::pair().unwrap();

    let handle = match driver.submit_recvmsg_multishot(rx.as_raw_fd(), &pool, 0) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping: failed to submit recvmsg multishot: {e}");
            return;
        }
    };
    let id = handle.id();
    let weak = handle.downgrade_keepalive();

    drop(handle);

    assert!(
        weak.upgrade().is_some(),
        "dropping the handle must not free kernel-referenced resources early"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while driver.is_multishot_active(id).unwrap() {
        match driver.wait().unwrap() {
            Completion::MultiShotRecvMsg { id: got, event } if got == id => {
                if let MultiShotRecvMsgEvent::End(end) = &event {
                    if is_multishot_unsupported_res(end.res) {
                        eprintln!(
                            "skipping: recvmsg multishot appears unsupported (res={})",
                            end.res
                        );
                        return;
                    }
                }
            }
            _ => {}
        }

        if Instant::now() > deadline {
            panic!("timed out waiting for multishot cancellation after drop");
        }
    }

    assert!(
        weak.upgrade().is_none(),
        "resources should be released after the final CQE"
    );
}
