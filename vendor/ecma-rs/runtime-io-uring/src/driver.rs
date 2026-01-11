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
    use std::os::fd::RawFd;
    use std::sync::Weak;

    use io_uring::{opcode, types, IoUring};

    pub struct IoUringDriver {
        ring: Option<IoUring>,
        next_id: u64,
        in_flight: Option<HashMap<u64, Box<dyn FnOnce(i32) + Send + 'static>>>,
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

        fn in_flight(
            &mut self,
        ) -> &mut HashMap<u64, Box<dyn FnOnce(i32) + Send + 'static>> {
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
            let len = buf.len();
            let entry = opcode::Read::new(types::Fd(fd), ptr, len as _)
                .offset(offset as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                Box::new(move |result| {
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, buf);
                    }
                }),
            );

            // Flush SQEs into the kernel.
            self.ring().submit()?;

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
            let len = buf.len();
            let entry = opcode::Write::new(types::Fd(fd), ptr, len as _)
                .offset(offset as _)
                .build()
                .user_data(id.0);

            self.push_entry(&entry)?;

            self.in_flight().insert(
                id.0,
                Box::new(move |result| {
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, buf);
                    }
                }),
            );

            self.ring().submit()?;

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
                Box::new(move |result| {
                    if let Some(shared) = weak.upgrade() {
                        shared.complete(result, ());
                    }
                }),
            );

            self.ring().submit()?;

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
                if let Some(complete) = in_flight.remove(&id) {
                    complete(result);
                }
                n += 1;
            }
            Ok(n)
        }

        /// Block until at least one CQE is available, then process all CQEs.
        pub fn wait_for_cqe(&mut self) -> io::Result<usize> {
            let n = self.poll_completions()?;
            if n != 0 {
                return Ok(n);
            }
            self.ring().submit_and_wait(1)?;
            self.poll_completions()
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

    pub fn cancel(&mut self, _target: OpId) -> io::Result<IoOp<()>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "io_uring is only supported on Linux",
        ))
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
