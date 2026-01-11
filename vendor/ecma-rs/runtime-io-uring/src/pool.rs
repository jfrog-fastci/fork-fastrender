#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::{Driver, WeakDriver};

    /// Statistics for a [`ProvidedBufPool`].
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct PoolStats {
        pub nbufs: usize,
        pub buf_size: usize,
        /// Buffers currently available for selection by the kernel.
        pub in_kernel: usize,
        /// Buffers currently held by userspace as `LeasedBuf`s.
        pub leased: usize,
        /// Number of times a `LeasedBuf` was dropped and its buffer was successfully
        /// re-provided back to the kernel.
        pub reprovided: usize,
    }

    struct Inner {
        driver: WeakDriver,
        buf_group: u16,
        buf_size: usize,
        nbufs: u16,
        storage: Box<[u8]>,
        leased_map: Mutex<Vec<bool>>,
        in_kernel: AtomicUsize,
        leased: AtomicUsize,
        reprovided: AtomicUsize,
        slab_drop_counter: Arc<AtomicUsize>,
        remove_submitted: AtomicBool,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            self.slab_drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A pool of stable buffers "provided" to the kernel for buffer selection.
    ///
    /// Reads/recvs can be submitted without passing a per-operation pointer. The kernel selects an
    /// available buffer from the pool, writes into it, and returns the chosen `buffer_id` in the
    /// CQE flags.
    ///
    /// The buffers are allocated in stable memory (`Box<[u8]>`); the backing allocation address
    /// never changes for the lifetime of the pool.
    #[derive(Clone)]
    pub struct ProvidedBufPool {
        inner: Arc<Inner>,
    }

    impl std::fmt::Debug for ProvidedBufPool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ProvidedBufPool")
                .field("buf_group", &self.buf_group())
                .field("buf_size", &self.buf_size())
                .field("nbufs", &self.nbufs())
                .field("stats", &self.stats())
                .finish()
        }
    }

    impl ProvidedBufPool {
        pub fn new(driver: &Driver, buf_group: u16, buf_size: usize, nbufs: u16) -> io::Result<Self> {
            if nbufs == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "nbufs must be > 0"));
            }
            if buf_size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buf_size must be > 0",
                ));
            }
            if buf_size > i32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "buf_size must fit into i32 for IORING_OP_PROVIDE_BUFFERS",
                ));
            }

            let total_len = buf_size.checked_mul(nbufs as usize).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "buffer pool too large")
            })?;

            let storage = vec![0u8; total_len].into_boxed_slice();
            let leased_map = vec![false; nbufs as usize];

            let pool = Self {
                inner: Arc::new(Inner {
                    driver: driver.downgrade(),
                    buf_group,
                    buf_size,
                    nbufs,
                    storage,
                    leased_map: Mutex::new(leased_map),
                    in_kernel: AtomicUsize::new(0),
                    leased: AtomicUsize::new(0),
                    reprovided: AtomicUsize::new(0),
                    slab_drop_counter: Arc::new(AtomicUsize::new(0)),
                    remove_submitted: AtomicBool::new(false),
                }),
            };

            // Provide the full, contiguous buffer slab in one operation.
            driver.submit_provide_buffers(
                pool.inner.storage.as_ptr() as *mut u8,
                buf_size,
                nbufs,
                buf_group,
                0,
                pool.clone(),
            )?;
            pool.inner.in_kernel.store(nbufs as usize, Ordering::Relaxed);

            Ok(pool)
        }

        pub fn buf_group(&self) -> u16 {
            self.inner.buf_group
        }

        pub fn buf_size(&self) -> usize {
            self.inner.buf_size
        }

        pub fn nbufs(&self) -> u16 {
            self.inner.nbufs
        }

        pub(crate) fn storage_base_ptr(&self) -> *const u8 {
            self.inner.storage.as_ptr()
        }

        pub fn stats(&self) -> PoolStats {
            PoolStats {
                nbufs: self.inner.nbufs as usize,
                buf_size: self.inner.buf_size,
                in_kernel: self.inner.in_kernel.load(Ordering::Relaxed),
                leased: self.inner.leased.load(Ordering::Relaxed),
                reprovided: self.inner.reprovided.load(Ordering::Relaxed),
            }
        }

        /// Returns a counter that increments when this pool's backing buffer slab is freed.
        ///
        /// This is intended for tests/debugging.
        #[doc(hidden)]
        pub fn debug_slab_drop_counter(&self) -> Arc<AtomicUsize> {
            self.inner.slab_drop_counter.clone()
        }

        pub(crate) fn lease(&self, buf_id: u16, filled_len: usize) -> io::Result<LeasedBuf> {
            if buf_id >= self.inner.nbufs {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "kernel returned out-of-range buffer id",
                ));
            }
            if filled_len > self.inner.buf_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "kernel wrote more than buf_size",
                ));
            }

            {
                let mut leased_map = self
                    .inner
                    .leased_map
                    .lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "leased_map mutex poisoned"))?;
                if leased_map[buf_id as usize] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "kernel reused a buffer id that is still leased",
                    ));
                }
                leased_map[buf_id as usize] = true;
            }

            self.inner.in_kernel.fetch_sub(1, Ordering::Relaxed);
            self.inner.leased.fetch_add(1, Ordering::Relaxed);

            Ok(LeasedBuf {
                pool: self.clone(),
                buf_id,
                len: filled_len,
            })
        }

        fn buf_ptr(&self, buf_id: u16) -> *mut u8 {
            // Safe: buf_id is range-checked by callers; storage is stable for the lifetime of the pool.
            let offset = buf_id as usize * self.inner.buf_size;
            unsafe { self.inner.storage.as_ptr().add(offset) as *mut u8 }
        }

        fn buf_slice(&self, buf_id: u16, len: usize) -> &[u8] {
            let offset = buf_id as usize * self.inner.buf_size;
            &self.inner.storage[offset..offset + len]
        }

        fn return_to_kernel(&self, buf_id: u16) -> io::Result<()> {
            let (res, did_submit) = match self.inner.driver.upgrade() {
                Some(driver) => (
                    driver.submit_provide_buffers(
                        self.buf_ptr(buf_id),
                        self.inner.buf_size,
                        1,
                        self.inner.buf_group,
                        buf_id,
                        self.clone(),
                    ),
                    true,
                ),
                // Driver already dropped; treat as best-effort success, but don't update
                // `in_kernel`/`reprovided` since nothing was submitted to the kernel.
                None => (Ok(()), false),
            };

            {
                let mut leased_map = self
                    .inner
                    .leased_map
                    .lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "leased_map mutex poisoned"))?;
                leased_map[buf_id as usize] = false;
            }

            self.inner.leased.fetch_sub(1, Ordering::Relaxed);
            if did_submit && res.is_ok() {
                self.inner.in_kernel.fetch_add(1, Ordering::Relaxed);
                self.inner.reprovided.fetch_add(1, Ordering::Relaxed);
            }
            res
        }
    }

    impl Drop for ProvidedBufPool {
        fn drop(&mut self) {
            // Only the last handle can initiate buffer removal. This avoids redundant submissions
            // and ensures the pool's slab remains alive until the kernel acknowledges removal.
            if Arc::strong_count(&self.inner) != 1 {
                return;
            }
            if self
                .inner
                .remove_submitted
                .swap(true, Ordering::AcqRel)
            {
                return;
            }

            let Some(driver) = self.inner.driver.upgrade() else {
                // Driver already dropped (or leaked and not reachable via WeakDriver); the ring is
                // gone, so the kernel cannot reference the provided buffers anymore.
                return;
            };

            // `IORING_OP_PROVIDE_BUFFERS` registers raw user pointers inside the kernel. If we were
            // to free the backing slab while the ring is still alive, later buf-select ops could
            // cause the kernel to write into freed memory. Submit `IORING_OP_REMOVE_BUFFERS` so the
            // kernel forgets the pointers, keeping the slab alive until the internal CQE is drained.
            let keepalive = self.clone();
            if let Err(_err) = driver.submit_remove_buffers(self.inner.nbufs, self.inner.buf_group, keepalive) {
                // Best-effort fallback: if we cannot submit the removal request (unsupported kernel,
                // SQ full, etc), leak the pool slab to preserve memory safety.
                #[cfg(debug_assertions)]
                eprintln!(
                    "runtime-io-uring: failed to submit REMOVE_BUFFERS for group {}; leaking pool slab",
                    self.inner.buf_group
                );
                // Leak a clone so the underlying slab remains valid for the rest of the process.
                std::mem::forget(self.clone());
            }
        }
    }

    /// A buffer selected by the kernel from a [`ProvidedBufPool`].
    ///
    /// When dropped, the buffer is automatically re-provided back to the kernel so it can be
    /// reused for future reads.
    pub struct LeasedBuf {
        pool: ProvidedBufPool,
        buf_id: u16,
        len: usize,
    }

    impl LeasedBuf {
        pub fn buf_id(&self) -> u16 {
            self.buf_id
        }

        pub fn len(&self) -> usize {
            self.len
        }

        pub fn as_slice(&self) -> &[u8] {
            self.pool.buf_slice(self.buf_id, self.len)
        }

        /// Pointer to the filled prefix of the underlying stable buffer.
        ///
        /// Intended for future "external buffer" (zero-copy) GC integration.
        pub fn as_ptr(&self) -> *const u8 {
            self.as_slice().as_ptr()
        }
    }

    impl std::fmt::Debug for LeasedBuf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LeasedBuf")
                .field("buf_id", &self.buf_id)
                .field("len", &self.len)
                .finish()
        }
    }

    impl Drop for LeasedBuf {
        fn drop(&mut self) {
            // Best-effort in Drop. If this fails, future reads may return ENOBUFS due to pool
            // exhaustion.
            let _ = self.pool.return_to_kernel(self.buf_id);
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use std::io;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct PoolStats;

    #[derive(Clone, Debug)]
    pub struct ProvidedBufPool;

    #[derive(Debug)]
    pub struct LeasedBuf;

    impl ProvidedBufPool {
        pub fn new(
            _driver: &crate::Driver,
            _buf_group: u16,
            _buf_size: usize,
            _nbufs: u16,
        ) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }

        pub fn stats(&self) -> PoolStats {
            PoolStats
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
