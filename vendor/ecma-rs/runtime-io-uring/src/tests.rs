#[cfg(not(target_os = "linux"))]
#[test]
fn io_uring_tests_skipped_non_linux() {
    // The driver is Linux-only; compilation is still expected to succeed elsewhere.
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::os::fd::RawFd;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Duration;

    use crate::buf::{GcIoBuf, OwnedIoBuf};
    use crate::gc::GcHooks;
    use crate::mock_gc::MockGc;
    use crate::IoUringDriver;

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

    fn try_driver() -> io::Result<Option<IoUringDriver>> {
        match IoUringDriver::new(64) {
            Ok(d) => Ok(Some(d)),
            Err(e) => {
                eprintln!("skipping io_uring tests: {e}");
                Ok(None)
            }
        }
    }

    #[test]
    fn public_types_are_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send::<IoUringDriver>();
        assert_send_sync::<crate::Driver>();
        assert_send_sync::<crate::ProvidedBufPool>();
    }

    #[test]
    fn basic_read_write() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let (a, mut b) = UnixStream::pair()?;

        // WRITE via io_uring, READ via std.
        let write_buf = OwnedIoBuf::from_vec(b"hello".to_vec());
        let write_op = driver.submit_write(a.as_raw_fd(), write_buf, 0)?;
        let write_c = write_op.wait(&mut driver)?;
        assert_eq!(write_c.result, 5);

        let mut got = [0u8; 5];
        b.read_exact(&mut got)?;
        assert_eq!(&got, b"hello");

        // WRITE via std, READ via io_uring.
        b.write_all(b"world")?;
        let read_buf = OwnedIoBuf::new_zeroed(5);
        let read_op = driver.submit_read(a.as_raw_fd(), read_buf, 0)?;
        let read_c = read_op.wait(&mut driver)?;
        assert_eq!(read_c.result, 5);
        assert_eq!(read_c.resource.as_slice(), b"world");

        Ok(())
    }

    #[test]
    fn cancellation_holds_pins_until_target_cqe() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let (read_fd, write_fd) = pipe()?;

        let gc = MockGc::new();
        let handle = gc.alloc_zeroed(8);

        // Blocked read.
        let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
        let read_op = driver.submit_read(read_fd.0, buf, 0)?;

        // Best-effort cancel.
        let cancel_op = driver.cancel(read_op.id())?;

        // Safety net: if cancellation doesn't work, write some data so the read unblocks.
        let write_fd_dup = unsafe { libc::dup(write_fd.0) };
        assert!(write_fd_dup >= 0);
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = unsafe { libc::write(write_fd_dup, b"x".as_ptr() as *const _, 1) };
            unsafe {
                libc::close(write_fd_dup);
            }
        });

        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        let mut saw_cancel_before_read = false;
        while !(cancel_op.is_completed() && read_op.is_completed()) {
            driver.wait_for_cqe()?;

            if cancel_op.is_completed() && !read_op.is_completed() {
                saw_cancel_before_read = true;
                assert_eq!(gc.root_drops(handle), 0);
                assert_eq!(gc.pin_drops(handle), 0);
                assert_eq!(gc.root_count(handle), 1);
                assert_eq!(gc.pin_count(handle), 1);
            }
        }

        // Join the safety-net writer (no-op if cancellation worked).
        writer.join().expect("writer thread panicked");

        let cancel_res = cancel_op
            .try_take_completion()
            .expect("cancel completed")
            .result;
        let read_c = read_op
            .try_take_completion()
            .expect("read completed");

        // The pinned/rooted guards are stored in the returned buffer, so they must not have dropped
        // yet (regardless of cancellation outcome).
        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        let read_canceled =
            read_c.result == -(libc::ECANCELED as i32) || read_c.result == -(libc::EINTR as i32);
        if !read_canceled {
            let cancel_unsupported = cancel_res == -(libc::EINVAL as i32)
                || cancel_res == -(libc::EOPNOTSUPP as i32)
                || cancel_res == -(libc::ENOSYS as i32);

            if cancel_unsupported {
                eprintln!(
                    "skipping cancellation semantics (kernel returned {cancel_res}); \
                     read result was {}",
                    read_c.result
                );
                return Ok(());
            }

            // Raced with completion; cancellation is best-effort.
            if cancel_res == -(libc::ENOENT as i32) {
                eprintln!(
                    "skipping cancellation semantics due to race (cancel -ENOENT); \
                     read result was {}",
                    read_c.result
                );
                return Ok(());
            }

            panic!(
                "expected read to be canceled (-ECANCELED/-EINTR), got {}; cancel CQE result={}",
                read_c.result, cancel_res
            );
        }

        // Dropping the read completion buffer should drop the GC root+pin guards.
        drop(read_c);
        assert_eq!(gc.root_drops(handle), 1);
        assert_eq!(gc.pin_drops(handle), 1);
        assert_eq!(gc.root_count(handle), 0);
        assert_eq!(gc.pin_count(handle), 0);

        // Usually the cancel CQE arrives before the read CQE, but don't hard-require it.
        if !saw_cancel_before_read {
            eprintln!("note: cancel CQE arrived after read CQE on this kernel");
        }

        Ok(())
    }

    #[test]
    fn timeout_holds_pins_until_target_cqe() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let link_timeout_supported = match driver.is_link_timeout_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!("skipping timeout test: failed to probe io_uring ops: {err}");
                return Ok(());
            }
        };
        if !link_timeout_supported {
            eprintln!("skipping timeout test: IORING_OP_LINK_TIMEOUT not supported by kernel");
            return Ok(());
        }

        let (read_fd, write_fd) = pipe()?;

        let gc = MockGc::new();
        let handle = gc.alloc_zeroed(8);

        let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
        let (read_op, timeout_op) =
            driver.submit_read_with_timeout(read_fd.0, buf, 0, Duration::from_millis(100))?;

        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        let mut saw_timeout_before_read = false;
        while !(timeout_op.is_completed() && read_op.is_completed()) {
            driver.wait_for_cqe()?;

            if timeout_op.is_completed() && !read_op.is_completed() {
                saw_timeout_before_read = true;
                assert_eq!(gc.root_drops(handle), 0);
                assert_eq!(gc.pin_drops(handle), 0);
                assert_eq!(gc.root_count(handle), 1);
                assert_eq!(gc.pin_count(handle), 1);
            }
        }

        // Keep the pipe write end alive (otherwise reads can complete with EOF instead of blocking).
        drop(write_fd);

        let timeout_c = timeout_op
            .try_take_completion()
            .expect("timeout completed");
        let read_c = read_op.try_take_completion().expect("read completed");

        assert_eq!(timeout_c.result, -(libc::ETIME as i32));
        assert_eq!(timeout_c.resource, read_c.id);
        assert_eq!(read_c.result, -(libc::ECANCELED as i32));

        // The pinned/rooted guards are stored in the returned buffer, so they must not have dropped
        // yet even though the timeout CQE was processed.
        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        drop(read_c);
        assert_eq!(gc.root_drops(handle), 1);
        assert_eq!(gc.pin_drops(handle), 1);
        assert_eq!(gc.root_count(handle), 0);
        assert_eq!(gc.pin_count(handle), 0);

        if !saw_timeout_before_read {
            eprintln!("note: timeout CQE arrived after read CQE on this kernel");
        }

        Ok(())
    }

    #[test]
    fn timeout_race_complete_just_before_deadline() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let link_timeout_supported = match driver.is_link_timeout_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!("skipping timeout race test: failed to probe io_uring ops: {err}");
                return Ok(());
            }
        };
        if !link_timeout_supported {
            eprintln!("skipping timeout race test: IORING_OP_LINK_TIMEOUT not supported by kernel");
            return Ok(());
        }

        let (reader, mut writer) = UnixStream::pair()?;
        let read_buf = OwnedIoBuf::new_zeroed(1);
        let (read_op, timeout_op) = driver.submit_read_with_timeout(
            reader.as_raw_fd(),
            read_buf,
            0,
            Duration::from_millis(1000),
        )?;

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(700));
            writer.write_all(b"x").unwrap();
        });

        while !(timeout_op.is_completed() && read_op.is_completed()) {
            driver.wait_for_cqe()?;
        }

        let read_c = read_op.try_take_completion().expect("read completed");
        let timeout_c = timeout_op
            .try_take_completion()
            .expect("timeout completed");

        assert_eq!(read_c.result, 1);
        assert_eq!(read_c.resource.as_slice(), b"x");
        assert_eq!(timeout_c.result, -(libc::ECANCELED as i32));
        assert_eq!(timeout_c.resource, read_c.id);

        Ok(())
    }

    #[test]
    fn explicit_cancel_vs_timeout_race() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let link_timeout_supported = match driver.is_link_timeout_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!(
                    "skipping cancel-vs-timeout test: failed to probe io_uring ops: {err}"
                );
                return Ok(());
            }
        };
        if !link_timeout_supported {
            eprintln!(
                "skipping cancel-vs-timeout test: IORING_OP_LINK_TIMEOUT not supported by kernel"
            );
            return Ok(());
        }

        let async_cancel_supported = match driver.is_async_cancel_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!(
                    "skipping cancel-vs-timeout test: failed to probe io_uring ops: {err}"
                );
                return Ok(());
            }
        };
        if !async_cancel_supported {
            eprintln!(
                "skipping cancel-vs-timeout test: IORING_OP_ASYNC_CANCEL not supported by kernel"
            );
            return Ok(());
        }

        let (read_fd, write_fd) = pipe()?;

        let gc = MockGc::new();
        let handle = gc.alloc_zeroed(8);

        let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
        let (read_op, timeout_op) =
            driver.submit_read_with_timeout(read_fd.0, buf, 0, Duration::from_secs(5))?;
        let cancel_op = driver.cancel(read_op.id())?;

        // Safety net: if cancellation doesn't work, write some data so the read unblocks.
        let write_fd_dup = unsafe { libc::dup(write_fd.0) };
        assert!(write_fd_dup >= 0);
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = unsafe { libc::write(write_fd_dup, b"x".as_ptr() as *const _, 1) };
            unsafe {
                libc::close(write_fd_dup);
            }
        });

        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        while !(cancel_op.is_completed() && timeout_op.is_completed() && read_op.is_completed()) {
            driver.wait_for_cqe()?;

            if (cancel_op.is_completed() || timeout_op.is_completed()) && !read_op.is_completed() {
                assert_eq!(gc.root_drops(handle), 0);
                assert_eq!(gc.pin_drops(handle), 0);
                assert_eq!(gc.root_count(handle), 1);
                assert_eq!(gc.pin_count(handle), 1);
            }
        }

        writer.join().expect("writer thread panicked");
        drop(write_fd);

        let cancel_res = cancel_op.try_take_completion().expect("cancel completed").result;
        let timeout_res = timeout_op
            .try_take_completion()
            .expect("timeout completed")
            .result;
        let read_c = read_op.try_take_completion().expect("read completed");

        let read_canceled =
            read_c.result == -(libc::ECANCELED as i32) || read_c.result == -(libc::EINTR as i32);
        if !read_canceled {
            eprintln!(
                "skipping cancel-vs-timeout semantics due to race: read result was {}",
                read_c.result
            );
            return Ok(());
        }

        assert!(
            timeout_res == -(libc::ECANCELED as i32) || timeout_res == -(libc::ETIME as i32),
            "unexpected timeout CQE result: {timeout_res}"
        );
        assert!(
            cancel_res == 0 || cancel_res == -(libc::ENOENT as i32),
            "unexpected cancel CQE result: {cancel_res}"
        );

        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        drop(read_c);
        assert_eq!(gc.root_drops(handle), 1);
        assert_eq!(gc.pin_drops(handle), 1);
        assert_eq!(gc.root_count(handle), 0);
        assert_eq!(gc.pin_count(handle), 0);

        Ok(())
    }

    #[test]
    fn accept_timeout_cancels_target() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let link_timeout_supported = match driver.is_link_timeout_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!("skipping accept timeout test: failed to probe io_uring ops: {err}");
                return Ok(());
            }
        };
        if !link_timeout_supported {
            eprintln!("skipping accept timeout test: IORING_OP_LINK_TIMEOUT not supported by kernel");
            return Ok(());
        }

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let (accept_op, timeout_op) =
            driver.submit_accept_with_timeout(listener.as_raw_fd(), 0, Duration::from_millis(100))?;

        while !(accept_op.is_completed() && timeout_op.is_completed()) {
            driver.wait_for_cqe()?;
        }

        let accept_c = accept_op.try_take_completion().expect("accept completed");
        let timeout_c = timeout_op.try_take_completion().expect("timeout completed");

        // Old kernels may not support certain opcodes; treat as a skip.
        if accept_c.result == -(libc::EINVAL as i32) || accept_c.result == -(libc::EOPNOTSUPP as i32) {
            eprintln!("skipping accept timeout test: accept not supported (res={})", accept_c.result);
            return Ok(());
        }

        assert_eq!(timeout_c.resource, accept_c.id);
        assert_eq!(timeout_c.result, -(libc::ETIME as i32));
        assert_eq!(accept_c.result, -(libc::ECANCELED as i32));
        assert_eq!(accept_c.resource, None);

        Ok(())
    }

    #[test]
    fn connect_completes_before_timeout() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let link_timeout_supported = match driver.is_link_timeout_supported() {
            Ok(v) => v,
            Err(err) => {
                eprintln!("skipping connect timeout test: failed to probe io_uring ops: {err}");
                return Ok(());
            }
        };
        if !link_timeout_supported {
            eprintln!("skipping connect timeout test: IORING_OP_LINK_TIMEOUT not supported by kernel");
            return Ok(());
        }

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;

        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let _fd_guard = Fd(fd);

        let (connect_op, timeout_op) =
            driver.submit_connect_with_timeout(fd, addr, Duration::from_secs(2))?;

        while !(connect_op.is_completed() && timeout_op.is_completed()) {
            driver.wait_for_cqe()?;
        }

        let connect_c = connect_op.try_take_completion().expect("connect completed");
        let timeout_c = timeout_op.try_take_completion().expect("timeout completed");

        if connect_c.result == -(libc::EINVAL as i32) || connect_c.result == -(libc::EOPNOTSUPP as i32) {
            eprintln!(
                "skipping connect timeout test: connect not supported (res={})",
                connect_c.result
            );
            return Ok(());
        }

        assert_eq!(timeout_c.resource, connect_c.id);
        assert_eq!(connect_c.result, 0);
        assert_eq!(timeout_c.result, -(libc::ECANCELED as i32));

        Ok(())
    }

    #[test]
    fn moving_gc_simulation_pin_prevents_relocation() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        // Demonstrate relocation for a rooted but *unpinned* object.
        let gc = MockGc::new();
        let h1 = gc.alloc_zeroed(16);
        let root1 = <MockGc as GcHooks>::root(&gc, h1);
        let p1_before = gc.ptr(h1).unwrap();
        gc.collect();
        let p1_after = gc.ptr(h1).unwrap();
        assert_ne!(p1_before, p1_after);
        drop(root1);

        // Now ensure a pinned in-flight op prevents relocation.
        let (read_fd, write_fd) = pipe()?;
        let h2 = gc.alloc_zeroed(16);
        let p2_before = gc.ptr(h2).unwrap();

        let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, h2);
        let read_op = driver.submit_read(read_fd.0, buf, 0)?;

        gc.collect();
        let p2_after = gc.ptr(h2).unwrap();
        assert_eq!(p2_before, p2_after, "pinned buffer relocated during GC");

        // Complete the read.
        unsafe {
            libc::write(write_fd.0, b"y".as_ptr() as *const _, 1);
        }
        while !read_op.is_completed() {
            driver.wait_for_cqe()?;
        }
        let _read_c = read_op.try_take_completion().expect("read completed");

        Ok(())
    }

    #[test]
    fn dropping_ioop_handle_still_holds_pins_until_cqe() -> io::Result<()> {
        let Some(mut driver) = try_driver()? else {
            return Ok(());
        };

        let (read_fd, write_fd) = pipe()?;

        let gc = MockGc::new();
        let handle = gc.alloc_zeroed(8);

        let buf: GcIoBuf<_> = GcIoBuf::from_gc(&gc, handle);
        let read_op = driver.submit_read(read_fd.0, buf, 0)?;
        drop(read_op);

        assert_eq!(gc.root_count(handle), 1);
        assert_eq!(gc.pin_count(handle), 1);
        assert_eq!(gc.root_drops(handle), 0);
        assert_eq!(gc.pin_drops(handle), 0);

        unsafe {
            libc::write(write_fd.0, b"z".as_ptr() as *const _, 1);
        }

        // Drive CQEs until the read's closure runs, dropping the buffer and therefore the GC
        // root+pin guards.
        while gc.root_count(handle) != 0 {
            driver.wait_for_cqe()?;
        }

        assert_eq!(gc.root_count(handle), 0);
        assert_eq!(gc.pin_count(handle), 0);
        assert_eq!(gc.root_drops(handle), 1);
        assert_eq!(gc.pin_drops(handle), 1);

        Ok(())
    }
}
