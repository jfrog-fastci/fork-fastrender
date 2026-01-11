#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Weak};
 
    use crate::driver::OpId;
    use crate::pool::{LeasedBuf, ProvidedBufPool};
    use crate::Driver;
 
    const IORING_CQE_F_BUFFER: u32 = 1 << 0;
    const IORING_CQE_F_MORE: u32 = 1 << 1;
    const IORING_CQE_BUFFER_SHIFT: u32 = 16;
 
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct IoUringRecvMsgOut {
        namelen: u32,
        controllen: u32,
        payloadlen: u32,
        flags: u32,
    }
 
    const RECVMSG_OUT_SIZE: usize = std::mem::size_of::<IoUringRecvMsgOut>();
 
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct MultiShotId(OpId);
 
    impl MultiShotId {
        pub fn as_u64(self) -> u64 {
            self.0.as_u64()
        }
 
        pub(crate) fn from_op_id(id: OpId) -> Self {
            Self(id)
        }
 
        pub(crate) fn op_id(self) -> OpId {
            self.0
        }
    }
 
    pub struct MultiShotHandle {
        driver: Driver,
        id: MultiShotId,
        keepalive: Arc<()>,
        stop_submitted: AtomicBool,
    }
 
    impl MultiShotHandle {
        pub(crate) fn new(driver: Driver, id: OpId, keepalive: Arc<()>) -> Self {
            Self {
                driver,
                id: MultiShotId::from_op_id(id),
                keepalive,
                stop_submitted: AtomicBool::new(false),
            }
        }
 
        pub fn id(&self) -> MultiShotId {
            self.id
        }
 
        pub fn stop(&self) -> io::Result<()> {
            if self.stop_submitted.load(Ordering::Relaxed) {
                return Ok(());
            }
 
            if self
                .stop_submitted
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return Ok(());
            }
 
            match self.driver.submit_async_cancel_internal(self.id.op_id()) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Allow retries on transient submission failures (e.g. SQ full).
                    self.stop_submitted.store(false, Ordering::Relaxed);
                    Err(e)
                }
            }
        }
 
        pub fn downgrade_keepalive(&self) -> Weak<()> {
            Arc::downgrade(&self.keepalive)
        }
    }
 
    impl Drop for MultiShotHandle {
        fn drop(&mut self) {
            if self.stop_submitted.swap(true, Ordering::Relaxed) {
                return;
            }
 
            let _ = self.driver.submit_async_cancel_internal(self.id.op_id());
        }
    }
 
    #[derive(Debug)]
    pub enum MultiShotRecvMsgEvent {
        Shot(MultiShotRecvMsgShot),
        Err(MultiShotRecvMsgErr),
        End(MultiShotEnd),
    }
 
    impl MultiShotRecvMsgEvent {
        pub fn more(&self) -> bool {
            match self {
                Self::Shot(s) => s.more,
                Self::Err(e) => e.more,
                Self::End(e) => e.more,
            }
        }
    }
 
    #[derive(Debug)]
    pub struct MultiShotRecvMsgShot {
        buf: LeasedBuf,
        payload_offset: usize,
        payload_len: usize,
        msg_flags: u32,
        more: bool,
    }
 
    impl MultiShotRecvMsgShot {
        pub fn payload(&self) -> &[u8] {
            &self.buf.as_slice()[self.payload_offset..self.payload_offset + self.payload_len]
        }
 
        pub fn msg_flags(&self) -> u32 {
            self.msg_flags
        }
 
        pub fn more(&self) -> bool {
            self.more
        }
    }
 
    #[derive(Debug)]
    pub struct MultiShotRecvMsgErr {
        pub res: i32,
        pub more: bool,
        // Held only to avoid dropping the buffer while the legacy driver's internal mutex is held.
        // Dropping happens after `Driver::wait()` returns the completion to the caller.
        _cleanup_buf: Option<LeasedBuf>,
    }
 
    #[derive(Debug)]
    pub struct MultiShotEnd {
        pub res: i32,
        pub more: bool,
        // Held only to avoid dropping the buffer while the legacy driver's internal mutex is held.
        _cleanup_buf: Option<LeasedBuf>,
    }
 
    pub(crate) struct MultiShotRecvMsgState {
        pool: ProvidedBufPool,
        // Represents all kernel-referenced resources for the request. Kept alive until the final
        // CQE (when `IORING_CQE_F_MORE` is no longer set).
        _keepalive: Arc<()>,
        // `RecvMsg` stores a `msghdr *` in the SQE; the pointed-to memory must remain valid for the
        // lifetime of the multishot request.
        msghdr: Box<libc::msghdr>,
        _iov: Box<[libc::iovec; 1]>,
    }
 
    impl MultiShotRecvMsgState {
        pub(crate) fn new(pool: ProvidedBufPool, keepalive: Arc<()>) -> io::Result<Self> {
            if pool.buf_size() < RECVMSG_OUT_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provided buffer is too small for io_uring_recvmsg_out header",
                ));
            }
 
            let mut iov = Box::new([libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: pool.buf_size(),
            }]);
 
            let msghdr = Box::new(libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: iov.as_mut_ptr(),
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            });
 
            Ok(Self {
                pool,
                _keepalive: keepalive,
                msghdr,
                _iov: iov,
            })
        }
 
        pub(crate) fn msghdr_ptr(&self) -> *mut libc::msghdr {
            (&*self.msghdr) as *const libc::msghdr as *mut libc::msghdr
        }
 
        pub(crate) fn handle_cqe(&self, res: i32, flags: u32) -> MultiShotRecvMsgEvent {
            let more = (flags & IORING_CQE_F_MORE) != 0;
 
            if res < 0 {
                if more {
                    return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                        res,
                        more,
                        _cleanup_buf: None,
                    });
                }
                return MultiShotRecvMsgEvent::End(MultiShotEnd {
                    res,
                    more: false,
                    _cleanup_buf: None,
                });
            }
 
            if (flags & IORING_CQE_F_BUFFER) == 0 {
                let err_res = -libc::EINVAL;
                if more {
                    return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                        res: err_res,
                        more,
                        _cleanup_buf: None,
                    });
                }
                return MultiShotRecvMsgEvent::End(MultiShotEnd {
                    res: err_res,
                    more: false,
                    _cleanup_buf: None,
                });
            }
 
            let buf_id = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
            let lease = match self.pool.lease(buf_id, self.pool.buf_size()) {
                Ok(b) => b,
                Err(e) => {
                    let err_res = e
                        .raw_os_error()
                        .map(|errno| -errno)
                        .unwrap_or(-libc::EINVAL);
                    if more {
                        return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                            res: err_res,
                            more,
                            _cleanup_buf: None,
                        });
                    }
                    return MultiShotRecvMsgEvent::End(MultiShotEnd {
                        res: err_res,
                        more: false,
                        _cleanup_buf: None,
                    });
                }
            };
 
            let bytes = lease.as_slice();
            if bytes.len() < RECVMSG_OUT_SIZE {
                let err_res = -libc::EINVAL;
                if more {
                    return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                        res: err_res,
                        more,
                        _cleanup_buf: Some(lease),
                    });
                }
                return MultiShotRecvMsgEvent::End(MultiShotEnd {
                    res: err_res,
                    more: false,
                    _cleanup_buf: Some(lease),
                });
            }
 
            // SAFETY: The kernel writes `struct io_uring_recvmsg_out` at the start of the selected
            // provided buffer.
            let out = unsafe { (bytes.as_ptr() as *const IoUringRecvMsgOut).read_unaligned() };
 
            let payload_offset =
                RECVMSG_OUT_SIZE + out.namelen as usize + out.controllen as usize;
            let payload_len = if out.payloadlen != 0 {
                out.payloadlen as usize
            } else {
                res as usize
            };
 
            let payload_end = match payload_offset.checked_add(payload_len) {
                Some(end) => end,
                None => {
                    let err_res = -libc::EINVAL;
                    if more {
                        return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                            res: err_res,
                            more,
                            _cleanup_buf: Some(lease),
                        });
                    }
                    return MultiShotRecvMsgEvent::End(MultiShotEnd {
                        res: err_res,
                        more: false,
                        _cleanup_buf: Some(lease),
                    });
                }
            };
 
            if payload_end > bytes.len() {
                let err_res = -libc::EINVAL;
                if more {
                    return MultiShotRecvMsgEvent::Err(MultiShotRecvMsgErr {
                        res: err_res,
                        more,
                        _cleanup_buf: Some(lease),
                    });
                }
                return MultiShotRecvMsgEvent::End(MultiShotEnd {
                    res: err_res,
                    more: false,
                    _cleanup_buf: Some(lease),
                });
            }
 
            MultiShotRecvMsgEvent::Shot(MultiShotRecvMsgShot {
                buf: lease,
                payload_offset,
                payload_len,
                msg_flags: out.flags,
                more,
            })
        }
    }
}
 
#[cfg(target_os = "linux")]
pub use linux::*;
 
#[cfg(not(target_os = "linux"))]
mod non_linux {
    use std::io;
    use std::sync::Weak;
 
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct MultiShotId;
 
    #[derive(Debug)]
    pub struct MultiShotHandle;
 
    #[derive(Debug)]
    pub enum MultiShotRecvMsgEvent {}
 
    #[derive(Debug)]
    pub struct MultiShotRecvMsgShot;
 
    #[derive(Debug)]
    pub struct MultiShotRecvMsgErr;
 
    #[derive(Debug)]
    pub struct MultiShotEnd;
 
    impl MultiShotHandle {
        pub fn id(&self) -> MultiShotId {
            MultiShotId
        }
 
        pub fn stop(&self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring is only supported on Linux",
            ))
        }
 
        pub fn downgrade_keepalive(&self) -> Weak<()> {
            Weak::new()
        }
    }
}
 
#[cfg(not(target_os = "linux"))]
pub use non_linux::*;
