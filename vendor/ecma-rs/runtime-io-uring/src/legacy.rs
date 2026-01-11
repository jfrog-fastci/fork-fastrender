//! Legacy `io_uring` driver used by [`PreparedOp`] helpers and [`ProvidedBufPool`].
//!
//! # Drop safety
//!
//! The kernel may continue to dereference SQE user pointers (buffers, path C-strings, `msghdr`
//! graphs, etc.) and post CQEs after a request is submitted. Dropping an `io_uring` instance or any
//! in-flight operation state too early can therefore cause use-after-free.
//!
//! To make the legacy [`Driver`] memory safe by construction, it follows a *leak-on-drop* policy:
//! when the last `Driver` is dropped, it first drains any already-ready CQEs (non-blocking). If any
//! operations are still in-flight, it intentionally leaks the ring and the remaining in-flight op
//! state so that any kernel-referenced pointers remain valid for the rest of the process lifetime.
//!
//! This matches the safety strategy used by the newer [`crate::IoUringDriver`].

#[cfg(target_os = "linux")]
mod linux {
    use std::any::Any;
    use std::collections::{HashMap, VecDeque};
    use std::ffi::CString;
    use std::io;
    use std::mem::{self, MaybeUninit};
    use std::net::SocketAddr;
    use std::os::unix::io::RawFd;
    use std::path::Path;
    use std::sync::{Arc, Mutex, Weak};
    use std::time::Duration;

    use io_uring::{opcode, squeue, types, IoUring};

    use crate::debug_stability;
    use crate::driver::OpId;
    use crate::multishot::{
        MultiShotHandle, MultiShotId, MultiShotRecvMsgEvent, MultiShotRecvMsgState,
    };
    use crate::op_connect_accept::{AcceptAddr, ConnectAddr};
    use crate::pool::{LeasedBuf, ProvidedBufPool};
    use crate::timeout::duration_to_timespec;

    const INTERNAL_USER_DATA: u64 = 0;
    const IORING_CQE_F_BUFFER: u32 = 1 << 0;
    const IORING_CQE_BUFFER_SHIFT: u32 = 16;
    const IORING_RECV_MULTISHOT: u32 = 1 << 0;

    fn ring_submit(ring: &mut IoUring) -> io::Result<()> {
        loop {
            match ring.submit() {
                Ok(_) => return Ok(()),
                Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn ring_submit_and_wait(ring: &mut IoUring, want: usize) -> io::Result<()> {
        loop {
            match ring.submit_and_wait(want) {
                Ok(_) => return Ok(()),
                Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn cstring_from_path(path: &Path) -> io::Result<CString> {
        use std::os::unix::ffi::OsStrExt;

        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains an interior NUL byte",
            )
        })
    }

    #[derive(Debug)]
    pub struct OpWithTimeout {
        pub op_id: OpId,
        pub timeout_id: OpId,
    }

    #[derive(Debug)]
    pub enum PreparedOp {
        Read {
            fd: RawFd,
            buf: Vec<u8>,
            /// Extra resources to keep alive for the op's lifetime.
            keep_alive: Option<Box<dyn Any + Send + Sync>>,
        },
        Connect {
            fd: RawFd,
            addr: ConnectAddr,
            /// Extra resources to keep alive for the op's lifetime.
            keep_alive: Option<Box<dyn Any + Send + Sync>>,
        },
        Accept {
            listener_fd: RawFd,
            flags: i32,
            addr: AcceptAddr,
            /// Extra resources to keep alive for the op's lifetime.
            keep_alive: Option<Box<dyn Any + Send + Sync>>,
        },
        RecvWithBufSelect {
            fd: RawFd,
            pool: ProvidedBufPool,
        },
        ReadWithBufSelect {
            fd: RawFd,
            offset: u64,
            pool: ProvidedBufPool,
        },
        OpenAt {
            dirfd: RawFd,
            path: CString,
            flags: i32,
            mode: u32,
            /// Extra resources to keep alive for the op's lifetime.
            keep_alive: Option<Box<dyn Any + Send + Sync>>,
        },
        Statx {
            dirfd: RawFd,
            path: CString,
            flags: i32,
            mask: u32,
            out: Box<MaybeUninit<libc::statx>>,
            /// Extra resources to keep alive for the op's lifetime.
            keep_alive: Option<Box<dyn Any + Send + Sync>>,
        },
    }

    impl PreparedOp {
        pub fn read(fd: RawFd, buf: Vec<u8>) -> Self {
            Self::Read {
                fd,
                buf,
                keep_alive: None,
            }
        }

        pub fn read_with_keep_alive(
            fd: RawFd,
            buf: Vec<u8>,
            keep_alive: impl Any + Send + Sync,
        ) -> Self {
            Self::Read {
                fd,
                buf,
                keep_alive: Some(Box::new(keep_alive)),
            }
        }

        pub fn connect(fd: RawFd, addr: SocketAddr) -> Self {
            Self::Connect {
                fd,
                addr: ConnectAddr::new(addr),
                keep_alive: None,
            }
        }

        pub fn connect_with_keep_alive(
            fd: RawFd,
            addr: SocketAddr,
            keep_alive: impl Any + Send + Sync,
        ) -> Self {
            Self::Connect {
                fd,
                addr: ConnectAddr::new(addr),
                keep_alive: Some(Box::new(keep_alive)),
            }
        }

        pub fn accept(listener_fd: RawFd, flags: i32) -> Self {
            Self::Accept {
                listener_fd,
                flags,
                addr: AcceptAddr::new(),
                keep_alive: None,
            }
        }

        pub fn accept_with_keep_alive(
            listener_fd: RawFd,
            flags: i32,
            keep_alive: impl Any + Send + Sync,
        ) -> Self {
            Self::Accept {
                listener_fd,
                flags,
                addr: AcceptAddr::new(),
                keep_alive: Some(Box::new(keep_alive)),
            }
        }

        pub fn accept_peer_addr(&self) -> Option<SocketAddr> {
            match self {
                PreparedOp::Accept { addr, .. } => addr.peer_addr(),
                _ => None,
            }
        }

        pub fn recv_with_buf_select(fd: RawFd, pool: ProvidedBufPool) -> Self {
            Self::RecvWithBufSelect { fd, pool }
        }

        pub fn read_with_buf_select(fd: RawFd, offset: u64, pool: ProvidedBufPool) -> Self {
            Self::ReadWithBufSelect { fd, offset, pool }
        }

        pub fn openat(dirfd: RawFd, path: &Path, flags: i32, mode: u32) -> io::Result<Self> {
            Ok(Self::OpenAt {
                dirfd,
                path: cstring_from_path(path)?,
                flags,
                mode,
                keep_alive: None,
            })
        }

        pub fn openat_with_keep_alive(
            dirfd: RawFd,
            path: &Path,
            flags: i32,
            mode: u32,
            keep_alive: impl Any + Send + Sync,
        ) -> io::Result<Self> {
            Ok(Self::OpenAt {
                dirfd,
                path: cstring_from_path(path)?,
                flags,
                mode,
                keep_alive: Some(Box::new(keep_alive)),
            })
        }

        pub fn statx(dirfd: RawFd, path: &Path, flags: i32, mask: u32) -> io::Result<Self> {
            Ok(Self::Statx {
                dirfd,
                path: cstring_from_path(path)?,
                flags,
                mask,
                out: Box::new(MaybeUninit::uninit()),
                keep_alive: None,
            })
        }

        pub fn statx_with_keep_alive(
            dirfd: RawFd,
            path: &Path,
            flags: i32,
            mask: u32,
            keep_alive: impl Any + Send + Sync,
        ) -> io::Result<Self> {
            Ok(Self::Statx {
                dirfd,
                path: cstring_from_path(path)?,
                flags,
                mask,
                out: Box::new(MaybeUninit::uninit()),
                keep_alive: Some(Box::new(keep_alive)),
            })
        }

        pub fn into_statx_result(self, res: i32) -> io::Result<libc::statx> {
            match self {
                PreparedOp::Statx { out, .. } => {
                    if res == 0 {
                        Ok(unsafe { (*out).assume_init() })
                    } else if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::Other,
                            "statx returned an unexpected positive result",
                        ))
                    }
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a statx operation",
                )),
            }
        }

        fn build_sqe(&mut self) -> squeue::Entry {
            match self {
                PreparedOp::Read { fd, buf, .. } => {
                    opcode::Read::new(types::Fd(*fd), buf.as_mut_ptr(), buf.len() as _).build()
                }
                PreparedOp::Connect { fd, addr, .. } => {
                    opcode::Connect::new(types::Fd(*fd), addr.addr_ptr(), addr.addr_len()).build()
                }
                PreparedOp::Accept {
                    listener_fd,
                    flags,
                    addr,
                    ..
                } => opcode::Accept::new(
                    types::Fd(*listener_fd),
                    addr.addr_ptr(),
                    addr.addr_len_ptr(),
                )
                .flags(*flags)
                .build(),
                PreparedOp::RecvWithBufSelect { fd, pool } => opcode::Recv::new(
                    types::Fd(*fd),
                    std::ptr::null_mut(),
                    pool.buf_size() as u32,
                )
                .buf_group(pool.buf_group())
                .build()
                .flags(squeue::Flags::BUFFER_SELECT),
                PreparedOp::ReadWithBufSelect { fd, offset, pool } => opcode::Read::new(
                    types::Fd(*fd),
                    std::ptr::null_mut(),
                    pool.buf_size() as u32,
                )
                .offset(*offset)
                .buf_group(pool.buf_group())
                .build()
                .flags(squeue::Flags::BUFFER_SELECT),
                PreparedOp::OpenAt {
                    dirfd,
                    path,
                    flags,
                    mode,
                    ..
                } => opcode::OpenAt::new(types::Fd(*dirfd), path.as_ptr())
                    .flags(*flags)
                    .mode(*mode)
                    .build(),
                PreparedOp::Statx {
                    dirfd,
                    path,
                    flags,
                    mask,
                    out,
                    ..
                } => opcode::Statx::new(
                    types::Fd(*dirfd),
                    path.as_ptr(),
                    // io-uring defines its own `types::statx` wrapper type; `libc::statx` is layout
                    // compatible with the kernel ABI, so we cast the output pointer.
                    out.as_mut_ptr() as *mut types::statx,
                )
                    .flags(*flags)
                    .mask(*mask)
                    .build(),
            }
        }

        fn record_stability_pointers(&mut self, rec: &mut debug_stability::Recorder) {
            match self {
                PreparedOp::Read { buf, .. } => {
                    rec.ptr(
                        debug_stability::PtrKind::IoBufData { index: 0 },
                        buf.as_ptr(),
                    );
                }
                PreparedOp::Connect { addr, .. } => {
                    rec.ptr(
                        debug_stability::PtrKind::SockAddr,
                        addr.addr_ptr() as *const u8,
                    );
                }
                PreparedOp::Accept { addr, .. } => {
                    rec.ptr(
                        debug_stability::PtrKind::SockAddr,
                        addr.addr_ptr_const() as *const u8,
                    );
                    rec.ptr(
                        debug_stability::PtrKind::OutParam,
                        addr.addr_len_ptr_const() as *const u8,
                    );
                }
                PreparedOp::RecvWithBufSelect { pool, .. }
                | PreparedOp::ReadWithBufSelect { pool, .. } => {
                    rec.ptr(
                        debug_stability::PtrKind::ProvidedBufPoolBase {
                            group_id: pool.buf_group(),
                        },
                        pool.storage_base_ptr(),
                    );
                }
                PreparedOp::OpenAt { path, .. } => {
                    rec.ptr(debug_stability::PtrKind::Path, path.as_ptr() as *const u8);
                }
                PreparedOp::Statx { path, out, .. } => {
                    rec.ptr(debug_stability::PtrKind::Path, path.as_ptr() as *const u8);
                    rec.ptr(
                        debug_stability::PtrKind::OutParam,
                        out.as_ptr() as *const u8,
                    );
                }
            }
        }
    }

    #[derive(Debug)]
    pub enum Completion {
        Op { id: OpId, res: i32, op: PreparedOp },
        ProvidedBuf { id: OpId, buf: LeasedBuf },
        MultiShotRecvMsg { id: MultiShotId, event: MultiShotRecvMsgEvent },
        Timeout { id: OpId, target: OpId, res: i32 },
        Cancel { id: OpId, target: OpId, res: i32 },
    }

    enum OpState {
        Target {
            op: PreparedOp,
            stability: debug_stability::OpStability,
        },
        /// Internal op: remove a provided-buffer group so the kernel no longer holds user pointers.
        ///
        /// This op is not surfaced to callers via [`Completion`].
        InternalRemoveBuffers { pool: ProvidedBufPool },
        Timeout {
            target: OpId,
            _ts: Box<types::Timespec>,
            stability: debug_stability::OpStability,
        },
        Cancel { target: OpId },
    }

    struct Inner {
        ring: Option<IoUring>,
        next_id: u64,
        /// Number of in-flight internal ops submitted with `user_data == INTERNAL_USER_DATA`.
        ///
        /// These ops are not tracked in `ops`/`multishots`, but they can still cause the kernel to
        /// write CQEs into the ring mappings after submission (e.g. `IORING_OP_PROVIDE_BUFFERS`).
        /// We must therefore keep the ring alive until their CQEs have been observed.
        internal_in_flight: usize,
        /// Keep-alive for pointer-carrying internal ops.
        ///
        /// `IORING_OP_PROVIDE_BUFFERS` stores user pointers in the SQE. Even though these ops are
        /// internal (and their CQEs are not surfaced), the pointed-to pool storage must remain
        /// valid until the CQE is observed.
        internal_keepalive_pools: Vec<ProvidedBufPool>,
        ops: HashMap<OpId, OpState>,
        multishots: HashMap<OpId, MultiShotRecvMsgState>,
        ready: VecDeque<Completion>,
    }

    impl Inner {
        /// Process all currently-ready CQEs without blocking.
        ///
        /// Returns any internal keepalive pools that became eligible for drop as a result of
        /// draining internal CQEs. Callers should drop the returned pools *after* releasing the
        /// driver's mutex, since dropping a pool may submit further internal ops (e.g.
        /// `IORING_OP_REMOVE_BUFFERS`).
        fn poll_completions(&mut self) -> Vec<ProvidedBufPool> {
            let mut keepalive_to_drop = Vec::new();
            let Some(ring) = self.ring.as_mut() else {
                return keepalive_to_drop;
            };

            let cq = ring.completion();
            for cqe in cq {
                let id = OpId::from_u64(cqe.user_data());
                let res = cqe.result();
                let flags = cqe.flags();

                if id.as_u64() == INTERNAL_USER_DATA {
                    #[cfg(debug_assertions)]
                    if res < 0 {
                        eprintln!(
                            "runtime-io-uring: internal CQE error: {}",
                            io::Error::from_raw_os_error(-res)
                        );
                    }
                    self.internal_in_flight = self.internal_in_flight.saturating_sub(1);
                    if self.internal_in_flight == 0 {
                        keepalive_to_drop.append(&mut self.internal_keepalive_pools);
                    }
                    continue;
                }

                if self.multishots.contains_key(&id) {
                    let event = self
                        .multishots
                        .get(&id)
                        .expect("checked contains_key")
                        .handle_cqe(res, flags);
                    let more = event.more();
                    self.ready.push_back(Completion::MultiShotRecvMsg {
                        id: MultiShotId::from_op_id(id),
                        event,
                    });
                    if !more {
                        self.multishots.remove(&id);
                    }
                    continue;
                }

                let state = match self.ops.remove(&id) {
                    Some(state) => state,
                    None => continue,
                };

                match state {
                    OpState::Target { mut op, stability } => {
                        debug_stability::assert_stable(&stability, |rec| {
                            op.record_stability_pointers(rec);
                        });
                        match &mut op {
                            PreparedOp::RecvWithBufSelect { pool, .. }
                            | PreparedOp::ReadWithBufSelect { pool, .. } => {
                                if res < 0 {
                                    self.ready.push_back(Completion::Op { id, res, op });
                                    continue;
                                }

                                // Best-effort: if the CQE doesn't carry a buffer id (or leasing
                                // fails), fall back to treating this as a normal op completion so
                                // we can still drop the op resources safely.
                                if flags & IORING_CQE_F_BUFFER == 0 {
                                    self.ready.push_back(Completion::Op { id, res, op });
                                    continue;
                                }

                                let buf_id = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                                match pool.lease(buf_id, res as usize) {
                                    Ok(lease) => {
                                        self.ready
                                            .push_back(Completion::ProvidedBuf { id, buf: lease });
                                    }
                                    Err(_) => {
                                        self.ready.push_back(Completion::Op { id, res, op });
                                    }
                                }
                            }
                            _ => self.ready.push_back(Completion::Op { id, res, op }),
                        }
                    }
                    OpState::InternalRemoveBuffers { pool } => {
                        if res < 0 {
                            // If we failed to remove provided buffers, the kernel may still hold the
                            // user pointers. Leak the pool slab to preserve memory safety.
                            #[cfg(debug_assertions)]
                            eprintln!(
                                "runtime-io-uring: internal REMOVE_BUFFERS failed: {}; leaking pool slab",
                                io::Error::from_raw_os_error(-res)
                            );
                            mem::forget(pool);
                        }
                        // Success path: drop the pool, freeing the slab.
                    }
                    OpState::Timeout {
                        target,
                        _ts,
                        stability,
                    } => {
                        debug_stability::assert_stable(&stability, |rec| {
                            rec.ptr(
                                debug_stability::PtrKind::Timespec,
                                (&*_ts as *const types::Timespec) as *const u8,
                            );
                        });
                        self.ready
                            .push_back(Completion::Timeout { id, target, res });
                    }
                    OpState::Cancel { target } => {
                        self.ready.push_back(Completion::Cancel { id, target, res });
                    }
                }
            }
            keepalive_to_drop
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            // Best-effort: process any already-ready CQEs so completed ops don't force a leak.
            let _ = self.poll_completions();

            if self.ops.is_empty() && self.multishots.is_empty() && self.internal_in_flight == 0 {
                return;
            }

            // Safety: dropping an io_uring instance or any in-flight op state while the kernel may
            // still be using SQE pointers can cause use-after-free. Leak the ring and the in-flight
            // op resources to preserve memory safety.
            #[cfg(debug_assertions)]
            eprintln!(
                "runtime-io-uring: dropping legacy Driver with {} in-flight ops ({} multishots, {} internal); leaking ring + op state",
                self.ops.len(),
                self.multishots.len(),
                self.internal_in_flight
            );

            if let Some(ring) = self.ring.take() {
                mem::forget(ring);
            }
            mem::forget(mem::take(&mut self.internal_keepalive_pools));
            mem::forget(mem::take(&mut self.ops));
            mem::forget(mem::take(&mut self.multishots));
        }
    }

    #[derive(Clone)]
    /// Legacy `io_uring` driver.
    ///
    /// # Drop safety
    /// Dropping a driver while operations are still in-flight would normally drop the owned buffers
    /// and metadata before the kernel finishes, which is unsound. To preserve memory safety, the
    /// legacy driver drains any already-ready CQEs and then intentionally leaks the ring and any
    /// remaining in-flight op state when the last `Driver` handle is dropped.
    ///
    /// This matches the leak-on-drop strategy used by [`crate::IoUringDriver`].
    pub struct Driver {
        inner: Arc<Mutex<Inner>>,
    }

    /// A non-owning reference to a [`Driver`].
    ///
    /// This is primarily used to avoid reference cycles between:
    /// `Driver -> ops map -> PreparedOp -> ProvidedBufPool -> Driver`.
    #[derive(Clone)]
    pub struct WeakDriver {
        inner: Weak<Mutex<Inner>>,
    }

    impl WeakDriver {
        /// Attempt to upgrade this weak reference to a strong [`Driver`].
        pub fn upgrade(&self) -> Option<Driver> {
            Some(Driver {
                inner: self.inner.upgrade()?,
            })
        }
    }

    impl Driver {
        fn submission_queue_full() -> io::Error {
            io::Error::other("io_uring submission queue is full")
        }

        pub fn new(entries: u32) -> io::Result<Self> {
            Ok(Self {
                inner: Arc::new(Mutex::new(Inner {
                    ring: Some(IoUring::new(entries)?),
                    next_id: 1,
                    internal_in_flight: 0,
                    internal_keepalive_pools: Vec::new(),
                    ops: HashMap::new(),
                    multishots: HashMap::new(),
                    ready: VecDeque::new(),
                })),
            })
        }

        /// Create a weak reference to this driver.
        pub fn downgrade(&self) -> WeakDriver {
            WeakDriver {
                inner: Arc::downgrade(&self.inner),
            }
        }

        fn alloc_id(inner: &mut Inner) -> OpId {
            loop {
                let id = OpId::from_u64(inner.next_id);
                inner.next_id = inner.next_id.wrapping_add(1);
                if id.as_u64() != INTERNAL_USER_DATA {
                    return id;
                }
            }
        }

        pub(crate) fn submit_provide_buffers(
            &self,
            addr: *mut u8,
            len: usize,
            nbufs: u16,
            buf_group: u16,
            buf_id: u16,
            keepalive_pool: ProvidedBufPool,
        ) -> io::Result<()> {
            let len_i32: i32 = len.try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buffer length must fit into i32 for IORING_OP_PROVIDE_BUFFERS",
                )
            })?;

            let entry = opcode::ProvideBuffers::new(addr, len_i32, nbufs, buf_group, buf_id)
                .build()
                .user_data(INTERNAL_USER_DATA);

            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;
            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                unsafe {
                    sq.push(&entry).unwrap();
                }
                inner.internal_in_flight = inner.internal_in_flight.saturating_add(1);
                inner.internal_keepalive_pools.push(keepalive_pool);
            }

            ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            )?;
            Ok(())
        }

        pub(crate) fn submit_remove_buffers(
            &self,
            nbufs: u16,
            buf_group: u16,
            keepalive_pool: ProvidedBufPool,
        ) -> io::Result<()> {
            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;
            let op_id = Self::alloc_id(inner);

            let entry = opcode::RemoveBuffers::new(nbufs, buf_group)
                .build()
                .user_data(op_id.as_u64());

            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let ops = &mut inner.ops;

                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                ops.insert(op_id, OpState::InternalRemoveBuffers { pool: keepalive_pool });

                unsafe {
                    sq.push(&entry).unwrap();
                }
            }

            inner
                .ring
                .as_mut()
                .expect("io_uring used after drop (ring already taken)")
                .submit()?;
            Ok(())
        }

        pub(crate) fn submit_async_cancel_internal(&self, target: OpId) -> io::Result<()> {
            let entry = opcode::AsyncCancel::new(target.as_u64())
                .build()
                .user_data(INTERNAL_USER_DATA);

            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;

            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                unsafe {
                    sq.push(&entry).unwrap();
                }
                inner.internal_in_flight = inner.internal_in_flight.saturating_add(1);
            }

            ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            )?;
            Ok(())
        }

        pub fn submit(&mut self, mut op: PreparedOp) -> io::Result<OpId> {
            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;
            let op_id = Self::alloc_id(inner);

            let stability = debug_stability::record(op_id, |rec| op.record_stability_pointers(rec));
            let entry = op.build_sqe().user_data(op_id.as_u64());
            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let ops = &mut inner.ops;

                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                ops.insert(op_id, OpState::Target { op, stability });

                unsafe {
                    sq.push(&entry).unwrap();
                }
            }
            ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            )?;

            Ok(op_id)
        }

        pub fn submit_openat(
            &mut self,
            dirfd: RawFd,
            path: &Path,
            flags: i32,
            mode: u32,
        ) -> io::Result<OpId> {
            self.submit(PreparedOp::openat(dirfd, path, flags, mode)?)
        }

        pub fn submit_statx(
            &mut self,
            dirfd: RawFd,
            path: &Path,
            flags: i32,
            mask: u32,
        ) -> io::Result<OpId> {
            self.submit(PreparedOp::statx(dirfd, path, flags, mask)?)
        }

        pub fn submit_connect(&mut self, fd: RawFd, addr: SocketAddr) -> io::Result<OpId> {
            self.submit(PreparedOp::connect(fd, addr))
        }

        pub fn submit_accept(&mut self, listener_fd: RawFd, flags: i32) -> io::Result<OpId> {
            self.submit(PreparedOp::accept(listener_fd, flags))
        }

        pub fn submit_recvmsg_multishot(
            &mut self,
            fd: RawFd,
            pool: &ProvidedBufPool,
            recv_flags: u32,
        ) -> io::Result<MultiShotHandle> {
            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;

            let id = Self::alloc_id(inner);
            let keepalive = Arc::new(());
            let state = MultiShotRecvMsgState::new(pool.clone(), keepalive.clone())?;
            let msg_ptr = state.msghdr_ptr();

            let entry = opcode::RecvMsg::new(types::Fd(fd), msg_ptr)
                .buf_group(pool.buf_group())
                .flags(recv_flags | IORING_RECV_MULTISHOT)
                .build()
                .flags(squeue::Flags::BUFFER_SELECT)
                .user_data(id.as_u64());

            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let multishots = &mut inner.multishots;

                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                multishots.insert(id, state);
                unsafe {
                    sq.push(&entry).unwrap();
                }
            }

            if let Err(e) = ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            ) {
                inner.multishots.remove(&id);
                return Err(e);
            }

            Ok(MultiShotHandle::new(self.clone(), id, keepalive))
        }

        pub fn stop_multishot(&self, handle: &MultiShotHandle) -> io::Result<()> {
            handle.stop()
        }

        pub fn is_multishot_active(&self, id: MultiShotId) -> io::Result<bool> {
            let inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            Ok(inner_guard.multishots.contains_key(&id.op_id()))
        }

        /// Submit `op` with a per-operation timeout.
        ///
        /// This uses `IOSQE_IO_LINK` + `IORING_OP_LINK_TIMEOUT`:
        /// - If the target op completes before the timeout, the target CQE returns its normal result
        ///   (>= 0 for success, `-errno` for failure) and the timeout CQE returns `-ECANCELED`.
        /// - If the timeout expires first, the timeout CQE returns `-ETIME` and the target op CQE
        ///   returns `-ECANCELED`.
        ///
        /// Resource lifetime rule: even if the timeout CQE is observed first, resources owned by the
        /// target op (buffers, pins, roots) are not released until the target op CQE is processed.
        pub fn submit_with_timeout(
            &mut self,
            mut op: PreparedOp,
            timeout: Duration,
        ) -> io::Result<OpWithTimeout> {
            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;

            let op_id = Self::alloc_id(inner);
            let timeout_id = Self::alloc_id(inner);

            let op_stability = debug_stability::record(op_id, |rec| op.record_stability_pointers(rec));
            let entry = op
                .build_sqe()
                .flags(squeue::Flags::IO_LINK)
                .user_data(op_id.as_u64());

            // `IORING_OP_LINK_TIMEOUT` stores a pointer to the passed `Timespec` in the SQE; keep
            // that memory alive until the timeout CQE completes.
            let ts = Box::new(duration_to_timespec(timeout));
            let ts_ptr: *const types::Timespec = &*ts;
            let timeout_entry = opcode::LinkTimeout::new(&*ts)
                .build()
                .user_data(timeout_id.as_u64());
            let timeout_stability = debug_stability::record(timeout_id, |rec| {
                rec.ptr(debug_stability::PtrKind::Timespec, ts_ptr as *const u8);
            });

            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let ops = &mut inner.ops;

                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 2 {
                    return Err(Self::submission_queue_full());
                }

                ops.insert(
                    op_id,
                    OpState::Target {
                        op,
                        stability: op_stability,
                    },
                );
                ops.insert(
                    timeout_id,
                    OpState::Timeout {
                        target: op_id,
                        _ts: ts,
                        stability: timeout_stability,
                    },
                );

                unsafe {
                    sq.push(&entry).unwrap();
                    sq.push(&timeout_entry).unwrap();
                }
            }
            ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            )?;

            Ok(OpWithTimeout { op_id, timeout_id })
        }

        /// Submit an async cancellation request for `target`.
        ///
        /// The returned [`OpId`] refers to the cancel request itself. The cancel CQE result is:
        /// - `0` if one request was successfully canceled
        /// - `-ENOENT` if the target request could not be found (already completed/canceled)
        ///
        /// The target op still delivers its own CQE, typically `-ECANCELED` if the cancellation won.
        pub fn cancel(&mut self, target: OpId) -> io::Result<OpId> {
            let mut inner_guard = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
            let inner = &mut *inner_guard;
            let cancel_id = Self::alloc_id(inner);

            let entry = opcode::AsyncCancel::new(target.as_u64())
                .build()
                .user_data(cancel_id.as_u64());

            {
                let ring = inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)");
                let ops = &mut inner.ops;

                let mut sq = ring.submission();
                let available = sq.capacity() - sq.len();
                if available < 1 {
                    return Err(Self::submission_queue_full());
                }

                ops.insert(cancel_id, OpState::Cancel { target });

                unsafe {
                    sq.push(&entry).unwrap();
                }
            }
            ring_submit(
                inner
                    .ring
                    .as_mut()
                    .expect("io_uring used after drop (ring already taken)"),
            )?;

            Ok(cancel_id)
        }

        pub fn wait(&mut self) -> io::Result<Completion> {
            // Dropping a pool may submit additional internal ops. Ensure we don't drop any internal
            // keepalive pools while holding the driver's mutex.
            let mut keepalive_to_drop: Vec<ProvidedBufPool> = Vec::new();

            let res = {
                let mut inner_guard = self
                    .inner
                    .lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
                let inner = &mut *inner_guard;

                loop {
                    if let Some(c) = inner.ready.pop_front() {
                        break Ok(c);
                    }

                    ring_submit_and_wait(
                        inner
                            .ring
                            .as_mut()
                            .expect("io_uring used after drop (ring already taken)"),
                        1,
                    )?;

                    let cq = inner
                        .ring
                        .as_mut()
                        .expect("io_uring used after drop (ring already taken)")
                        .completion();
                    for cqe in cq {
                        let id = OpId::from_u64(cqe.user_data());
                        let res = cqe.result();
                        let flags = cqe.flags();

                        if id.as_u64() == INTERNAL_USER_DATA {
                            #[cfg(debug_assertions)]
                            if res < 0 {
                                eprintln!(
                                    "runtime-io-uring: internal CQE error: {}",
                                    io::Error::from_raw_os_error(-res)
                                );
                            }
                            inner.internal_in_flight = inner.internal_in_flight.saturating_sub(1);
                            if inner.internal_in_flight == 0 {
                                keepalive_to_drop.append(&mut inner.internal_keepalive_pools);
                            }
                            continue;
                        }

                        if inner.multishots.contains_key(&id) {
                            let event = inner
                                .multishots
                                .get(&id)
                                .expect("checked contains_key")
                                .handle_cqe(res, flags);
                            let more = event.more();
                            inner.ready.push_back(Completion::MultiShotRecvMsg {
                                id: MultiShotId::from_op_id(id),
                                event,
                            });
                            if !more {
                                inner.multishots.remove(&id);
                            }
                            continue;
                        }

                        let state = match inner.ops.remove(&id) {
                            Some(state) => state,
                            None => continue,
                        };

                        match state {
                            OpState::Target { mut op, stability } => {
                                debug_stability::assert_stable(&stability, |rec| {
                                    op.record_stability_pointers(rec);
                                });
                                match &mut op {
                                    PreparedOp::RecvWithBufSelect { pool, .. }
                                    | PreparedOp::ReadWithBufSelect { pool, .. } => {
                                        if res < 0 {
                                            inner.ready.push_back(Completion::Op { id, res, op });
                                            continue;
                                        }

                                        if flags & IORING_CQE_F_BUFFER == 0 {
                                            return Err(io::Error::new(
                                                io::ErrorKind::Other,
                                                "missing buffer id in CQE for buf-select op",
                                            ));
                                        }
                                        let buf_id = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                                        let lease = pool.lease(buf_id, res as usize)?;
                                        inner.ready.push_back(Completion::ProvidedBuf { id, buf: lease });
                                    }
                                    _ => inner.ready.push_back(Completion::Op { id, res, op }),
                                }
                            }
                            OpState::InternalRemoveBuffers { pool } => {
                                if res < 0 {
                                    #[cfg(debug_assertions)]
                                    eprintln!(
                                        "runtime-io-uring: internal REMOVE_BUFFERS failed: {}; leaking pool slab",
                                        io::Error::from_raw_os_error(-res)
                                    );
                                    mem::forget(pool);
                                }
                            }
                            OpState::Timeout {
                                target,
                                _ts,
                                stability,
                            } => {
                                debug_stability::assert_stable(&stability, |rec| {
                                    rec.ptr(
                                        debug_stability::PtrKind::Timespec,
                                        (&*_ts as *const types::Timespec) as *const u8,
                                    );
                                });
                                inner.ready.push_back(Completion::Timeout { id, target, res });
                            }
                            OpState::Cancel { target } => {
                                inner.ready.push_back(Completion::Cancel { id, target, res });
                            }
                        }
                    }
                }
            };
            drop(keepalive_to_drop);
            res
        }
    }

    impl Drop for Driver {
        fn drop(&mut self) {
            // Only the final `Driver` handle can tear down the ring.
            if Arc::strong_count(&self.inner) != 1 {
                return;
            }

            // Best-effort: process any already-ready CQEs so completed ops don't force a leak/panic.
            let (ops_len, multishots_len, internal_in_flight, keepalive_to_drop) = {
                let mut guard = match self.inner.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                let keepalive_to_drop = guard.poll_completions();
                (
                    guard.ops.len(),
                    guard.multishots.len(),
                    guard.internal_in_flight,
                    keepalive_to_drop,
                )
            };
            drop(keepalive_to_drop);

            if ops_len == 0 && multishots_len == 0 && internal_in_flight == 0 {
                return;
            }

            // Leak before panicking: panicking in `Drop` does not guarantee field drops are skipped.
            mem::forget(self.inner.clone());

            if (cfg!(debug_assertions) || cfg!(feature = "debug_stability"))
                && !std::thread::panicking()
            {
                let total = ops_len + multishots_len + internal_in_flight;
                panic!(
                    "runtime-io-uring: dropping legacy Driver with {total} in-flight ops \
                     ({ops_len} single-shot, {multishots_len} multishot, {internal_in_flight} internal); \
                     drive to completion (and/or cancel) before drop"
                );
            }
        }
    }

    pub fn is_link_timeout_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::LinkTimeout::CODE))
    }

    pub fn is_async_cancel_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::AsyncCancel::CODE))
    }

    pub fn is_accept_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::Accept::CODE))
    }

    pub fn is_connect_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::Connect::CODE))
    }

    pub fn is_provide_buffers_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::ProvideBuffers::CODE))
    }

    pub fn is_remove_buffers_supported(driver: &Driver) -> io::Result<bool> {
        let mut probe = io_uring::Probe::new();
        let inner = driver
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "io_uring mutex poisoned"))?;
        inner
            .ring
            .as_ref()
            .expect("io_uring used after drop (ring already taken)")
            .submitter()
            .register_probe(&mut probe)?;
        Ok(probe.is_supported(opcode::RemoveBuffers::CODE))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn is_uring_unavailable(err: &io::Error) -> bool {
            matches!(
                err.raw_os_error(),
                Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
            )
        }

        #[test]
        fn drop_counts_internal_in_flight_ops() {
            let driver = match Driver::new(8) {
                Ok(d) => d,
                Err(e) if is_uring_unavailable(&e) => return,
                Err(e) => return Err(e).unwrap(),
            };

            {
                let mut guard = driver.inner.lock().unwrap_or_else(|e| e.into_inner());
                guard.internal_in_flight = 1;
            }

            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(driver)));
            if cfg!(debug_assertions) || cfg!(feature = "debug_stability") {
                assert!(
                    res.is_err(),
                    "dropping a legacy Driver with in-flight internal ops should panic in debug builds"
                );
            } else {
                assert!(
                    res.is_ok(),
                    "dropping a legacy Driver with in-flight internal ops should not panic in release builds"
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use std::io;
    use std::time::Duration;

    use crate::driver::OpId;

    #[derive(Debug)]
    pub struct OpWithTimeout {
        pub op_id: OpId,
        pub timeout_id: OpId,
    }

    #[derive(Debug)]
    pub enum PreparedOp {}

    #[derive(Debug)]
    pub enum Completion {}

    pub struct Driver;

    #[derive(Clone, Debug)]
    pub struct WeakDriver;

    impl WeakDriver {
        pub fn upgrade(&self) -> Option<Driver> {
            None
        }
    }

    impl Driver {
        pub fn new(_entries: u32) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }

        pub fn downgrade(&self) -> WeakDriver {
            WeakDriver
        }

        pub fn submit(&mut self, _op: PreparedOp) -> io::Result<OpId> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }

        pub fn submit_with_timeout(
            &mut self,
            _op: PreparedOp,
            _timeout: Duration,
        ) -> io::Result<OpWithTimeout> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }

        pub fn cancel(&mut self, _target: OpId) -> io::Result<OpId> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }

        pub fn wait(&mut self) -> io::Result<Completion> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }
    }

    pub fn is_link_timeout_supported(_driver: &Driver) -> io::Result<bool> {
        Ok(false)
    }

    pub fn is_async_cancel_supported(_driver: &Driver) -> io::Result<bool> {
        Ok(false)
    }

    pub fn is_accept_supported(_driver: &Driver) -> io::Result<bool> {
        Ok(false)
    }

    pub fn is_connect_supported(_driver: &Driver) -> io::Result<bool> {
        Ok(false)
    }

    pub fn is_provide_buffers_supported(_driver: &Driver) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
