use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::buf::{IoBuf, IoBufMut};

/// Stable ID routed through `io_uring` `user_data`.
///
/// The driver never stores raw pointers (GC or otherwise) in `user_data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(u64);

impl OpId {
    pub(crate) fn from_u64(id: u64) -> Self {
        Self(id)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Completion of a submitted operation.
#[derive(Debug)]
pub struct OpCompletion<R> {
    pub id: OpId,
    /// `io_uring` CQE result:
    /// - `>= 0`: success (e.g. number of bytes read/written)
    /// - `< 0`: negated errno (e.g. `-ECANCELED`)
    pub result: i32,
    /// Resources returned to the caller (e.g. the owned buffer).
    pub resource: R,
}

#[derive(Debug)]
struct OpShared<R> {
    id: OpId,
    completion: Mutex<Option<OpCompletion<R>>>,
}

impl<R> OpShared<R> {
    fn new(id: OpId) -> Self {
        Self {
            id,
            completion: Mutex::new(None),
        }
    }

    fn complete(&self, result: i32, resource: R) {
        let mut guard = self.completion.lock().expect("poisoned mutex");
        *guard = Some(OpCompletion {
            id: self.id,
            result,
            resource,
        });
    }

    fn take_completion(&self) -> Option<OpCompletion<R>> {
        self.completion
            .lock()
            .expect("poisoned mutex")
            .take()
    }

    fn is_completed(&self) -> bool {
        self.completion
            .lock()
            .expect("poisoned mutex")
            .is_some()
    }
}

/// Handle for an in-flight operation.
///
/// The driver owns and pins all kernel-referenced resources until the CQE for the operation
/// arrives. Dropping the handle does **not** cancel the operation (use [`IoUringDriver::cancel`]).
#[derive(Debug)]
pub struct IoOp<R> {
    id: OpId,
    shared: Arc<OpShared<R>>,
}

impl<R> IoOp<R> {
    pub fn id(&self) -> OpId {
        self.id
    }

    pub fn is_completed(&self) -> bool {
        self.shared.is_completed()
    }

    pub fn try_take_completion(&self) -> Option<OpCompletion<R>> {
        self.shared.take_completion()
    }

    /// Block the current thread until this op completes, driving the ring via `driver`.
    pub fn wait(self, driver: &mut IoUringDriver) -> io::Result<OpCompletion<R>> {
        loop {
            if let Some(c) = self.shared.take_completion() {
                return Ok(c);
            }
            driver.wait_for_cqe()?;
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::mem;
    use std::net::SocketAddr;
    use std::os::fd::RawFd;
    use std::sync::Weak;
    use std::time::Duration;

    use io_uring::{opcode, squeue, types, IoUring};

    use crate::timeout::duration_to_timespec;

    use crate::op_connect_accept::{AcceptAddr, ConnectAddr};
    use crate::op_readv_writev::{build_readv_iovecs, build_writev_iovecs};
    use crate::op_sendmsg_recvmsg::{
        build_recvmsg_iovecs, build_sendmsg_iovecs, copy_sockaddr_storage, RecvMsg, RecvMsgResource,
        SendMsg,
    };

    /// `libc`'s `iovec`/`msghdr` contain raw pointers, so they are not `Send` by default.
    ///
    /// We only use these allocations as stable kernel metadata and never dereference the pointers
    /// while the op is in-flight, so moving the owning boxes across threads is safe.
    struct SendIovecs(Box<[libc::iovec]>);
    unsafe impl Send for SendIovecs {}

    impl SendIovecs {
        fn as_ptr(&self) -> *const libc::iovec {
            self.0.as_ptr()
        }

        fn len(&self) -> usize {
            self.0.len()
        }
    }

    struct SendMsgHdr(Box<libc::msghdr>);
    unsafe impl Send for SendMsgHdr {}

    impl SendMsgHdr {
        fn as_ptr(&self) -> *const libc::msghdr {
            self.0.as_ref() as *const _
        }

        fn as_mut_ptr(&mut self) -> *mut libc::msghdr {
            self.0.as_mut() as *mut _
        }

        fn as_mut(&mut self) -> &mut libc::msghdr {
            self.0.as_mut()
        }

        fn as_ref(&self) -> &libc::msghdr {
            self.0.as_ref()
        }
    }

    enum InFlightOp {
        Once(Box<dyn FnOnce(i32) + Send + 'static>),
        #[cfg(feature = "send_zc")]
        SendZc(Box<dyn SendZcInFlight>),
    }

    #[cfg(feature = "send_zc")]
    trait SendZcInFlight: Send + 'static {
        /// Handle a single CQE.
        ///
        /// Returns `true` if the op has completed and should be removed from the in-flight map.
        fn on_cqe(&mut self, result: i32, flags: u32) -> bool;
    }

    #[cfg(feature = "send_zc")]
    struct InFlightSendZc<B: IoBuf> {
        weak: Weak<OpShared<crate::send_zc::SendZcResource<B>>>,
        buf: Option<B>,
        send_result: Option<i32>,
        send_flags: Option<u32>,
        notif: Option<crate::send_zc::SendZcNotif>,
        stability: crate::debug_stability::OpStability,
    }

    #[cfg(feature = "send_zc")]
    impl<B: IoBuf> InFlightSendZc<B> {
        fn try_complete(&mut self) -> bool {
            let Some(send_result) = self.send_result else {
                return false;
            };
            let Some(send_flags) = self.send_flags else {
                return false;
            };

            let notif_expected = (send_flags & crate::send_zc::IORING_CQE_F_MORE) != 0;
            if notif_expected && self.notif.is_none() {
                return false;
            }

            let buf = self
                .buf
                .take()
                .expect("SEND_ZC completed twice (buffer already taken)");
            let resource = crate::send_zc::SendZcResource {
                buf,
                notif: self.notif.take(),
                send_flags,
            };

            if let Some(shared) = self.weak.upgrade() {
                shared.complete(send_result, resource);
            }
            true
        }
    }

    #[cfg(feature = "send_zc")]
    impl<B: IoBuf> SendZcInFlight for InFlightSendZc<B> {
        fn on_cqe(&mut self, result: i32, flags: u32) -> bool {
            crate::debug_stability::assert_stable(&self.stability, |rec| {
                // The buffer pointer must remain stable until the final notification CQE.
                if let Some(ref buf) = self.buf {
                    rec.ptr(
                        crate::debug_stability::PtrKind::IoBufData { index: 0 },
                        buf.stable_ptr().as_ptr() as *const u8,
                    );
                }
            });

            if (flags & crate::send_zc::IORING_CQE_F_NOTIF) != 0 {
                self.notif = Some(crate::send_zc::SendZcNotif { result, flags });
            } else {
                self.send_result = Some(result);
                self.send_flags = Some(flags);
            }
            self.try_complete()
        }
    }

    /// Low-level `io_uring` driver.
    ///
    /// # Drop safety
    /// Dropping a driver while operations are still in-flight would normally drop the owned
    /// buffers/pin guards before the kernel finishes. To preserve memory safety, this driver
    /// intentionally leaks the ring and all in-flight op resources if dropped with pending ops.
    pub struct IoUringDriver {
        ring: Option<IoUring>,
        next_id: u64,
        in_flight: Option<HashMap<u64, InFlightOp>>,
    }

    impl IoUringDriver {
        /// Create a new `io_uring` instance.
        pub fn new(entries: u32) -> io::Result<Self> {
            Ok(Self {
                ring: Some(IoUring::new(entries)?),
                next_id: 1,
                in_flight: Some(HashMap::new()),
            })
        }

        fn ring(&mut self) -> &mut IoUring {
            self.ring
                .as_mut()
                .expect("IoUringDriver used after drop (ring already taken)")
        }

        fn in_flight(&mut self) -> &mut HashMap<u64, InFlightOp> {
            self.in_flight
                .as_mut()
                .expect("IoUringDriver used after drop (in_flight already taken)")
        }

        fn alloc_id(&mut self) -> OpId {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            OpId(id)
        }

        fn push_entry(&mut self, entry: &io_uring::squeue::Entry) -> io::Result<()> {
            unsafe {
                self.ring()
                    .submission()
                    .push(entry)
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring SQ is full"))?;
            }
            Ok(())
        }

        fn push_entries(&mut self, entries: &[io_uring::squeue::Entry]) -> io::Result<()> {
            let mut sq = self.ring().submission();
            let available = sq.capacity() - sq.len();
            if available < entries.len() {
                return Err(io::Error::new(io::ErrorKind::Other, "io_uring SQ is full"));
            }

            unsafe {
                for entry in entries {
                    sq.push(entry).expect("capacity checked");
                }
            }
            Ok(())
        }

        fn submit_sqes(&mut self) -> io::Result<()> {
            loop {
                match self.ring().submit() {
                    Ok(_) => return Ok(()),
                    Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
                    Err(err) => return Err(err),
                }
            }
        }

        fn submit_and_wait(&mut self, want: usize) -> io::Result<()> {
            loop {
                match self.ring().submit_and_wait(want) {
                    Ok(_) => return Ok(()),
                    Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
                    Err(err) => return Err(err),
                }
            }
        }

        /// Submit a read with a linked timeout.
        ///
        /// This uses `IOSQE_IO_LINK` + `IORING_OP_LINK_TIMEOUT`:
        /// - If the read completes before the timeout, the read CQE returns its normal result and
        ///   the timeout CQE returns `-ECANCELED`.
        /// - If the timeout expires first, the timeout CQE returns `-ETIME` and the read CQE
        ///   returns `-ECANCELED`.
        ///
        /// Resource lifetime rule: even if the timeout CQE is observed first, the read buffer is
        /// kept alive by the driver until the read CQE is processed.
        pub fn submit_read_with_timeout<B: IoBufMut>(
            &mut self,
            fd: RawFd,
            mut buf: B,
            offset: u64,
            timeout: Duration,
        ) -> io::Result<(IoOp<B>, IoOp<OpId>)> {
            let read_id = self.alloc_id();
            let timeout_id = self.alloc_id();

            let read_shared = Arc::new(OpShared::new(read_id));
            let read_weak: Weak<OpShared<B>> = Arc::downgrade(&read_shared);

            let timeout_shared = Arc::new(OpShared::new(timeout_id));
            let timeout_weak: Weak<OpShared<OpId>> = Arc::downgrade(&timeout_shared);

            let ptr = buf.stable_mut_ptr().as_ptr();
            let len = u32::try_from(buf.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer length exceeds u32::MAX",
                )
            })?;
            let stability = crate::debug_stability::record(read_id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IoBufData { index: 0 },
                    ptr as *const u8,
                );
            });
            let read_entry = opcode::Read::new(types::Fd(fd), ptr, len as _)
                .offset(offset as _)
                .build()
                .flags(squeue::Flags::IO_LINK)
                .user_data(read_id.0);

            // `IORING_OP_LINK_TIMEOUT` takes a pointer to a `__kernel_timespec`. That memory must
            // stay valid until the timeout request completes, so it cannot live on the stack here.
            let ts = Box::new(duration_to_timespec(timeout));
            let timeout_entry = opcode::LinkTimeout::new(&*ts)
                .build()
                .user_data(timeout_id.0);

            let entries = [read_entry, timeout_entry];
            self.push_entries(&entries)?;

            self.in_flight().insert(
                read_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IoBufData { index: 0 },
                            buf.stable_mut_ptr().as_ptr() as *const u8,
                        );
                    });
                    if let Some(shared) = read_weak.upgrade() {
                        shared.complete(result, buf);
                    }
                })),
            );

            self.in_flight().insert(
                timeout_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let _ts = ts;
                    if let Some(shared) = timeout_weak.upgrade() {
                        shared.complete(result, read_id);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok((
                IoOp {
                    id: read_id,
                    shared: read_shared,
                },
                IoOp {
                    id: timeout_id,
                    shared: timeout_shared,
                },
            ))
        }

        /// Submit a write with a linked timeout.
        ///
        /// See [`Self::submit_read_with_timeout`] for CQE result semantics.
        pub fn submit_write_with_timeout<B: IoBuf>(
            &mut self,
            fd: RawFd,
            buf: B,
            offset: u64,
            timeout: Duration,
        ) -> io::Result<(IoOp<B>, IoOp<OpId>)> {
            let write_id = self.alloc_id();
            let timeout_id = self.alloc_id();

            let write_shared = Arc::new(OpShared::new(write_id));
            let write_weak: Weak<OpShared<B>> = Arc::downgrade(&write_shared);

            let timeout_shared = Arc::new(OpShared::new(timeout_id));
            let timeout_weak: Weak<OpShared<OpId>> = Arc::downgrade(&timeout_shared);

            let ptr = buf.stable_ptr().as_ptr() as *const u8;
            let len = u32::try_from(buf.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer length exceeds u32::MAX",
                )
            })?;
            let stability = crate::debug_stability::record(write_id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IoBufData { index: 0 },
                    ptr as *const u8,
                );
            });
            let write_entry = opcode::Write::new(types::Fd(fd), ptr, len as _)
                .offset(offset as _)
                .build()
                .flags(squeue::Flags::IO_LINK)
                .user_data(write_id.0);

            let ts = Box::new(duration_to_timespec(timeout));
            let timeout_entry = opcode::LinkTimeout::new(&*ts)
                .build()
                .user_data(timeout_id.0);

            let entries = [write_entry, timeout_entry];
            self.push_entries(&entries)?;

            self.in_flight().insert(
                write_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IoBufData { index: 0 },
                            buf.stable_ptr().as_ptr() as *const u8,
                        );
                    });
                    if let Some(shared) = write_weak.upgrade() {
                        shared.complete(result, buf);
                    }
                })),
            );

            self.in_flight().insert(
                timeout_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let _ts = ts;
                    if let Some(shared) = timeout_weak.upgrade() {
                        shared.complete(result, write_id);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok((
                IoOp {
                    id: write_id,
                    shared: write_shared,
                },
                IoOp {
                    id: timeout_id,
                    shared: timeout_shared,
                },
            ))
        }

        /// Submit a connect with a linked timeout.
        ///
        /// CQE semantics match [`Self::submit_read_with_timeout`].
        pub fn submit_connect_with_timeout(
            &mut self,
            fd: RawFd,
            addr: SocketAddr,
            timeout: Duration,
        ) -> io::Result<(IoOp<()>, IoOp<OpId>)> {
            let connect_id = self.alloc_id();
            let timeout_id = self.alloc_id();

            let connect_shared = Arc::new(OpShared::new(connect_id));
            let connect_weak: Weak<OpShared<()>> = Arc::downgrade(&connect_shared);

            let timeout_shared = Arc::new(OpShared::new(timeout_id));
            let timeout_weak: Weak<OpShared<OpId>> = Arc::downgrade(&timeout_shared);

            let addr = ConnectAddr::new(addr);
            let connect_entry = opcode::Connect::new(types::Fd(fd), addr.addr_ptr(), addr.addr_len())
                .build()
                .flags(squeue::Flags::IO_LINK)
                .user_data(connect_id.0);

            let ts = Box::new(duration_to_timespec(timeout));
            let timeout_entry = opcode::LinkTimeout::new(&*ts)
                .build()
                .user_data(timeout_id.0);

            let entries = [connect_entry, timeout_entry];
            self.push_entries(&entries)?;

            self.in_flight().insert(
                connect_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let _addr = addr;
                    if let Some(shared) = connect_weak.upgrade() {
                        shared.complete(result, ());
                    }
                })),
            );

            self.in_flight().insert(
                timeout_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let _ts = ts;
                    if let Some(shared) = timeout_weak.upgrade() {
                        shared.complete(result, connect_id);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok((
                IoOp {
                    id: connect_id,
                    shared: connect_shared,
                },
                IoOp {
                    id: timeout_id,
                    shared: timeout_shared,
                },
            ))
        }

        /// Submit an accept with a linked timeout.
        ///
        /// CQE semantics match [`Self::submit_read_with_timeout`].
        pub fn submit_accept_with_timeout(
            &mut self,
            listener_fd: RawFd,
            flags: i32,
            timeout: Duration,
        ) -> io::Result<(IoOp<Option<SocketAddr>>, IoOp<OpId>)> {
            let accept_id = self.alloc_id();
            let timeout_id = self.alloc_id();

            let accept_shared = Arc::new(OpShared::new(accept_id));
            let accept_weak: Weak<OpShared<Option<SocketAddr>>> = Arc::downgrade(&accept_shared);

            let timeout_shared = Arc::new(OpShared::new(timeout_id));
            let timeout_weak: Weak<OpShared<OpId>> = Arc::downgrade(&timeout_shared);

            let mut addr = AcceptAddr::new();
            let accept_entry = opcode::Accept::new(
                types::Fd(listener_fd),
                addr.addr_ptr(),
                addr.addr_len_ptr(),
            )
            .flags(flags)
            .build()
            .flags(squeue::Flags::IO_LINK)
            .user_data(accept_id.0);

            let ts = Box::new(duration_to_timespec(timeout));
            let timeout_entry = opcode::LinkTimeout::new(&*ts)
                .build()
                .user_data(timeout_id.0);

            let entries = [accept_entry, timeout_entry];
            self.push_entries(&entries)?;

            self.in_flight().insert(
                accept_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let peer = if result < 0 { None } else { addr.peer_addr() };
                    if let Some(shared) = accept_weak.upgrade() {
                        shared.complete(result, peer);
                    }
                })),
            );

            self.in_flight().insert(
                timeout_id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let _ts = ts;
                    if let Some(shared) = timeout_weak.upgrade() {
                        shared.complete(result, accept_id);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok((
                IoOp {
                    id: accept_id,
                    shared: accept_shared,
                },
                IoOp {
                    id: timeout_id,
                    shared: timeout_shared,
                },
            ))
        }

        pub fn is_link_timeout_supported(&self) -> io::Result<bool> {
            let ring = self
                .ring
                .as_ref()
                .expect("IoUringDriver used after drop (ring already taken)");
            let mut probe = io_uring::Probe::new();
            ring.submitter().register_probe(&mut probe)?;
            Ok(probe.is_supported(opcode::LinkTimeout::CODE))
        }

        pub fn is_async_cancel_supported(&self) -> io::Result<bool> {
            let ring = self
                .ring
                .as_ref()
                .expect("IoUringDriver used after drop (ring already taken)");
            let mut probe = io_uring::Probe::new();
            ring.submitter().register_probe(&mut probe)?;
            Ok(probe.is_supported(opcode::AsyncCancel::CODE))
        }

        pub fn submit_read<B: IoBufMut>(
            &mut self,
            fd: RawFd,
            mut buf: B,
            offset: u64,
        ) -> io::Result<IoOp<B>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<B>> = Arc::downgrade(&shared);

            let ptr = buf.stable_mut_ptr().as_ptr();
            let len = u32::try_from(buf.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer length exceeds u32::MAX",
                )
            })?;
            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IoBufData { index: 0 },
                    ptr as *const u8,
                );
            });
            let entry = opcode::Read::new(types::Fd(fd), ptr, len)
                .offset(offset as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IoBufData { index: 0 },
                            buf.stable_mut_ptr().as_ptr() as *const u8,
                        );
                    });
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, buf);
                    }
                })),
            );

            // Flush SQEs into the kernel.
            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_write<B: IoBuf>(
            &mut self,
            fd: RawFd,
            buf: B,
            offset: u64,
        ) -> io::Result<IoOp<B>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<B>> = Arc::downgrade(&shared);

            let ptr = buf.stable_ptr().as_ptr() as *const u8;
            let len = u32::try_from(buf.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer length exceeds u32::MAX",
                )
            })?;
            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IoBufData { index: 0 },
                    ptr as *const u8,
                );
            });
            let entry = opcode::Write::new(types::Fd(fd), ptr, len)
                .offset(offset as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IoBufData { index: 0 },
                            buf.stable_ptr().as_ptr() as *const u8,
                        );
                    });
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, buf);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_readv<B: IoBufMut>(
            &mut self,
            fd: RawFd,
            mut bufs: Vec<B>,
            offset: Option<u64>,
        ) -> io::Result<IoOp<Vec<B>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<Vec<B>>> = Arc::downgrade(&shared);

            let iovecs = SendIovecs(build_readv_iovecs(&mut bufs));
            let iovecs_len: u32 = iovecs
                .len()
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many iovecs"))?;

            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IovecArray,
                    iovecs.as_ptr().cast::<u8>(),
                );
                for (i, b) in bufs.iter_mut().enumerate() {
                    rec.ptr(
                        crate::debug_stability::PtrKind::IoBufData { index: i },
                        b.stable_mut_ptr().as_ptr() as *const u8,
                    );
                }
            });

            let off = offset.unwrap_or(u64::MAX);
            let entry = opcode::Readv::new(types::Fd(fd), iovecs.as_ptr(), iovecs_len)
                .offset(off as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IovecArray,
                            iovecs.as_ptr().cast::<u8>(),
                        );
                        for (i, b) in bufs.iter_mut().enumerate() {
                            rec.ptr(
                                crate::debug_stability::PtrKind::IoBufData { index: i },
                                b.stable_mut_ptr().as_ptr() as *const u8,
                            );
                        }
                    });
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, bufs);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_writev<B: IoBuf>(
            &mut self,
            fd: RawFd,
            bufs: Vec<B>,
            offset: Option<u64>,
        ) -> io::Result<IoOp<Vec<B>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<Vec<B>>> = Arc::downgrade(&shared);

            let iovecs = SendIovecs(build_writev_iovecs(&bufs));
            let iovecs_len: u32 = iovecs
                .len()
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many iovecs"))?;

            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::IovecArray,
                    iovecs.as_ptr().cast::<u8>(),
                );
                for (i, b) in bufs.iter().enumerate() {
                    rec.ptr(
                        crate::debug_stability::PtrKind::IoBufData { index: i },
                        b.stable_ptr().as_ptr() as *const u8,
                    );
                }
            });

            let off = offset.unwrap_or(u64::MAX);
            let entry = opcode::Writev::new(types::Fd(fd), iovecs.as_ptr(), iovecs_len)
                .offset(off as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::IovecArray,
                            iovecs.as_ptr().cast::<u8>(),
                        );
                        for (i, b) in bufs.iter().enumerate() {
                            rec.ptr(
                                crate::debug_stability::PtrKind::IoBufData { index: i },
                                b.stable_ptr().as_ptr() as *const u8,
                            );
                        }
                    });
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, bufs);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_sendmsg<'a, B: IoBuf>(
            &mut self,
            fd: RawFd,
            msg: SendMsg<'a, B>,
        ) -> io::Result<IoOp<Vec<B>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<Vec<B>>> = Arc::downgrade(&shared);

            if msg.bufs.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sendmsg requires at least one buffer",
                ));
            }

            let bufs = msg.bufs;
            let iovecs = SendIovecs(build_sendmsg_iovecs(&bufs));
            let name = match msg.name {
                None => None,
                Some(bytes) => Some(copy_sockaddr_storage(bytes)?),
            };
            let control = msg.control.map(|c| c.to_vec().into_boxed_slice());

            let mut hdr: SendMsgHdr = SendMsgHdr(Box::new(unsafe { mem::zeroed() }));
            hdr.as_mut().msg_iov = iovecs.as_ptr().cast_mut();
            hdr.as_mut().msg_iovlen = iovecs.len();

            if let Some(ref name) = name {
                hdr.as_mut().msg_name =
                    (name.as_ref() as *const libc::sockaddr_storage).cast_mut().cast();
                hdr.as_mut().msg_namelen = msg.name.unwrap().len() as libc::socklen_t;
            }

            if let Some(ref control) = control {
                hdr.as_mut().msg_control = control.as_ptr().cast_mut().cast();
                hdr.as_mut().msg_controllen = control.len();
            }

            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::MsgHdr,
                    hdr.as_ptr().cast::<u8>(),
                );
                rec.ptr(
                    crate::debug_stability::PtrKind::IovecArray,
                    iovecs.as_ptr().cast::<u8>(),
                );
                if let Some(ref name) = name {
                    rec.ptr(
                        crate::debug_stability::PtrKind::SockAddr,
                        (name.as_ref() as *const libc::sockaddr_storage).cast::<u8>(),
                    );
                }
                if let Some(ref control) = control {
                    rec.ptr(
                        crate::debug_stability::PtrKind::MsgControl,
                        control.as_ptr(),
                    );
                }
                for (i, b) in bufs.iter().enumerate() {
                    rec.ptr(
                        crate::debug_stability::PtrKind::IoBufData { index: i },
                        b.stable_ptr().as_ptr() as *const u8,
                    );
                }
            });

            let entry = opcode::SendMsg::new(types::Fd(fd), hdr.as_ptr())
                .flags(msg.flags as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::MsgHdr,
                            hdr.as_ptr().cast::<u8>(),
                        );
                        rec.ptr(
                            crate::debug_stability::PtrKind::IovecArray,
                            iovecs.as_ptr().cast::<u8>(),
                        );
                        if let Some(ref name) = name {
                            rec.ptr(
                                crate::debug_stability::PtrKind::SockAddr,
                                (name.as_ref() as *const libc::sockaddr_storage).cast::<u8>(),
                            );
                        }
                        if let Some(ref control) = control {
                            rec.ptr(
                                crate::debug_stability::PtrKind::MsgControl,
                                control.as_ptr(),
                            );
                        }
                        for (i, b) in bufs.iter().enumerate() {
                            rec.ptr(
                                crate::debug_stability::PtrKind::IoBufData { index: i },
                                b.stable_ptr().as_ptr() as *const u8,
                            );
                        }
                    });
                    let _keep = (iovecs, hdr, name, control);
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, bufs);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_recvmsg<B: IoBufMut>(
            &mut self,
            fd: RawFd,
            msg: RecvMsg<B>,
        ) -> io::Result<IoOp<RecvMsgResource<B>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<RecvMsgResource<B>>> = Arc::downgrade(&shared);

            if msg.bufs.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recvmsg requires at least one buffer",
                ));
            }

            let mut bufs = msg.bufs;
            let iovecs = SendIovecs(build_recvmsg_iovecs(&mut bufs));

            let name = if msg.want_name {
                Some(Box::new(unsafe { mem::zeroed::<libc::sockaddr_storage>() }))
            } else {
                None
            };
            let control = msg
                .control_len
                .map(|len| vec![0u8; len].into_boxed_slice());

            let mut hdr: SendMsgHdr = SendMsgHdr(Box::new(unsafe { mem::zeroed() }));
            hdr.as_mut().msg_iov = iovecs.as_ptr().cast_mut();
            hdr.as_mut().msg_iovlen = iovecs.len();

            if let Some(ref name) = name {
                hdr.as_mut().msg_name =
                    (name.as_ref() as *const libc::sockaddr_storage).cast_mut().cast();
                hdr.as_mut().msg_namelen =
                    std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            }

            if let Some(ref control) = control {
                hdr.as_mut().msg_control = control.as_ptr().cast_mut().cast();
                hdr.as_mut().msg_controllen = control.len();
            }

            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(
                    crate::debug_stability::PtrKind::MsgHdr,
                    hdr.as_ptr().cast::<u8>(),
                );
                rec.ptr(
                    crate::debug_stability::PtrKind::IovecArray,
                    iovecs.as_ptr().cast::<u8>(),
                );
                if let Some(ref name) = name {
                    rec.ptr(
                        crate::debug_stability::PtrKind::SockAddr,
                        (name.as_ref() as *const libc::sockaddr_storage).cast::<u8>(),
                    );
                }
                if let Some(ref control) = control {
                    rec.ptr(
                        crate::debug_stability::PtrKind::MsgControl,
                        control.as_ptr(),
                    );
                }
                for (i, b) in bufs.iter_mut().enumerate() {
                    rec.ptr(
                        crate::debug_stability::PtrKind::IoBufData { index: i },
                        b.stable_mut_ptr().as_ptr() as *const u8,
                    );
                }
            });

            let entry = opcode::RecvMsg::new(types::Fd(fd), hdr.as_mut_ptr())
                .flags(msg.flags as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    crate::debug_stability::assert_stable(&stability, |rec| {
                        rec.ptr(
                            crate::debug_stability::PtrKind::MsgHdr,
                            hdr.as_ptr().cast::<u8>(),
                        );
                        rec.ptr(
                            crate::debug_stability::PtrKind::IovecArray,
                            iovecs.as_ptr().cast::<u8>(),
                        );
                        if let Some(ref name) = name {
                            rec.ptr(
                                crate::debug_stability::PtrKind::SockAddr,
                                (name.as_ref() as *const libc::sockaddr_storage).cast::<u8>(),
                            );
                        }
                        if let Some(ref control) = control {
                            rec.ptr(
                                crate::debug_stability::PtrKind::MsgControl,
                                control.as_ptr(),
                            );
                        }
                        for (i, b) in bufs.iter_mut().enumerate() {
                            rec.ptr(
                                crate::debug_stability::PtrKind::IoBufData { index: i },
                                b.stable_mut_ptr().as_ptr() as *const u8,
                            );
                        }
                    });
                    let hdr_ref = hdr.as_ref();
                    let msg_flags = if result < 0 { 0 } else { hdr_ref.msg_flags };

                    let (name_len, control_len) = if result < 0 {
                        (0usize, 0usize)
                    } else {
                        (
                            hdr_ref.msg_namelen as usize,
                            hdr_ref.msg_controllen as usize,
                        )
                    };

                    let name_len = name
                        .as_ref()
                        .map(|_| name_len.min(std::mem::size_of::<libc::sockaddr_storage>()))
                        .unwrap_or(0);
                    let control_len = control
                        .as_ref()
                        .map(|c| control_len.min(c.len()))
                        .unwrap_or(0);

                    let resource = RecvMsgResource::new(
                        bufs,
                        name,
                        name_len,
                        control,
                        control_len,
                        msg_flags,
                    );

                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, resource);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_connect(&mut self, fd: RawFd, addr: SocketAddr) -> io::Result<IoOp<()>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<()>> = Arc::downgrade(&shared);

            // Heap-owned sockaddr buffer; must remain valid until the connect CQE.
            let addr = ConnectAddr::new(addr);
            let entry = opcode::Connect::new(types::Fd(fd), addr.addr_ptr(), addr.addr_len())
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    // Keep the address buffer alive until the target CQE.
                    let _addr = addr;
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, ());
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        pub fn submit_accept(
            &mut self,
            listener_fd: RawFd,
            flags: i32,
        ) -> io::Result<IoOp<Option<SocketAddr>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<Option<SocketAddr>>> = Arc::downgrade(&shared);

            // Heap-owned sockaddr + socklen buffers; must remain valid until the accept CQE.
            let mut addr = AcceptAddr::new();
            let entry = opcode::Accept::new(types::Fd(listener_fd), addr.addr_ptr(), addr.addr_len_ptr())
                .flags(flags)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    let peer = addr.peer_addr();
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, peer);
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        /// Best-effort cancellation of an in-flight op by `user_data`/[`OpId`].
        ///
        /// Correctness: cancelling never drives resource release for the target op; resources are
        /// held until the target op's own CQE arrives.
        pub fn cancel(&mut self, target: OpId) -> io::Result<IoOp<()>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<()>> = Arc::downgrade(&shared);

            let entry = opcode::AsyncCancel::new(target.0)
                .build()
                .user_data(id.0);
            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::Once(Box::new(move |result| {
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, ());
                    }
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        /// Submit a zero-copy send (`IORING_OP_SEND_ZC`).
        ///
        /// The buffer is held until the final notification CQE (if any) indicates the kernel has
        /// released pinned pages. This prevents moving/compacting GC hazards.
        #[cfg(feature = "send_zc")]
        pub fn submit_send_zc<B: IoBuf>(
            &mut self,
            fd: RawFd,
            buf: B,
            flags: crate::send_zc::SendZcFlags,
        ) -> io::Result<IoOp<crate::send_zc::SendZcResource<B>>> {
            let id = self.alloc_id();
            let shared = Arc::new(OpShared::new(id));
            let weak: Weak<OpShared<crate::send_zc::SendZcResource<B>>> = Arc::downgrade(&shared);

            let ptr = buf.stable_ptr().as_ptr() as *const u8;
            let stability = crate::debug_stability::record(id, |rec| {
                rec.ptr(crate::debug_stability::PtrKind::IoBufData { index: 0 }, ptr);
            });

            let entry = crate::send_zc::build_sqe(fd, &buf, flags).user_data(id.0);
            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                InFlightOp::SendZc(Box::new(InFlightSendZc {
                    weak,
                    buf: Some(buf),
                    send_result: None,
                    send_flags: None,
                    notif: None,
                    stability,
                })),
            );

            self.submit_sqes()?;

            Ok(IoOp { id, shared })
        }

        /// Process all currently-available CQEs without blocking.
        pub fn poll_completions(&mut self) -> io::Result<usize> {
            let ring = self
                .ring
                .as_mut()
                .expect("IoUringDriver used after drop (ring already taken)");
            let in_flight = self
                .in_flight
                .as_mut()
                .expect("IoUringDriver used after drop (in_flight already taken)");

            let mut n = 0usize;
            let mut cq = ring.completion();
            while let Some(cqe) = cq.next() {
                let id = cqe.user_data();
                let result = cqe.result();
                #[cfg(feature = "send_zc")]
                let flags = cqe.flags();
                if let Some(op) = in_flight.remove(&id) {
                    match op {
                        InFlightOp::Once(complete) => {
                            complete(result);
                        }
                        #[cfg(feature = "send_zc")]
                        InFlightOp::SendZc(mut zc) => {
                            if !zc.on_cqe(result, flags) {
                                in_flight.insert(id, InFlightOp::SendZc(zc));
                            }
                        }
                    }
                }
                n += 1;
            }
            Ok(n)
        }

        /// Block until at least one CQE is available, then process all CQEs.
        pub fn wait_for_cqe(&mut self) -> io::Result<usize> {
            loop {
                let n = self.poll_completions()?;
                if n != 0 {
                    return Ok(n);
                }
                self.submit_and_wait(1)?;
            }
        }
    }

    impl Drop for IoUringDriver {
        fn drop(&mut self) {
            // Best-effort: process any already-ready CQEs so completed ops don't force a leak.
            let _ = self.poll_completions();

            let in_flight_non_empty = self
                .in_flight
                .as_ref()
                .is_some_and(|m| !m.is_empty());
            if in_flight_non_empty {
                // Safety: dropping an io_uring instance while the kernel still has in-flight ops
                // can lead to use-after-free on both:
                // - the ring's shared memory mappings (CQE writes), and
                // - user-provided buffer pointers.
                //
                // Leak the ring and the in-flight op resources to preserve memory safety. This
                // should only happen if the driver is dropped without driving it to completion.
                #[cfg(debug_assertions)]
                eprintln!(
                    "runtime-io-uring: dropping IoUringDriver with {} in-flight ops; leaking ring + buffers",
                    self.in_flight.as_ref().map_or(0, |m| m.len())
                );
                if let Some(ring) = self.ring.take() {
                    mem::forget(ring);
                }
                if let Some(in_flight) = self.in_flight.take() {
                    mem::forget(in_flight);
                }
                return;
            }

            // No in-flight ops left; drop normally.
            let _ = self.in_flight.take();
            let _ = self.ring.take();
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::IoUringDriver;

#[cfg(not(target_os = "linux"))]
pub struct IoUringDriver {
    _priv: (),
}

#[cfg(not(target_os = "linux"))]
impl IoUringDriver {
    pub fn new(_entries: u32) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_read<B: IoBufMut>(
        &mut self,
        _fd: i32,
        _buf: B,
        _offset: u64,
    ) -> io::Result<IoOp<B>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_write<B: IoBuf>(
        &mut self,
        _fd: i32,
        _buf: B,
        _offset: u64,
    ) -> io::Result<IoOp<B>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_connect(
        &mut self,
        _fd: i32,
        _addr: std::net::SocketAddr,
    ) -> io::Result<IoOp<()>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_accept(
        &mut self,
        _listener_fd: i32,
        _flags: i32,
    ) -> io::Result<IoOp<Option<std::net::SocketAddr>>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn cancel(&mut self, _target: OpId) -> io::Result<IoOp<()>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_read_with_timeout<B: IoBufMut>(
        &mut self,
        _fd: i32,
        _buf: B,
        _offset: u64,
        _timeout: std::time::Duration,
    ) -> io::Result<(IoOp<B>, IoOp<OpId>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_write_with_timeout<B: IoBuf>(
        &mut self,
        _fd: i32,
        _buf: B,
        _offset: u64,
        _timeout: std::time::Duration,
    ) -> io::Result<(IoOp<B>, IoOp<OpId>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_connect_with_timeout(
        &mut self,
        _fd: i32,
        _addr: std::net::SocketAddr,
        _timeout: std::time::Duration,
    ) -> io::Result<(IoOp<()>, IoOp<OpId>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn submit_accept_with_timeout(
        &mut self,
        _listener_fd: i32,
        _flags: i32,
        _timeout: std::time::Duration,
    ) -> io::Result<(IoOp<Option<std::net::SocketAddr>>, IoOp<OpId>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn is_link_timeout_supported(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn is_async_cancel_supported(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn poll_completions(&mut self) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }

    pub fn wait_for_cqe(&mut self) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
    }
}
