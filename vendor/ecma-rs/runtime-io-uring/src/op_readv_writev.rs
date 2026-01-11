use crate::buf::{IoBuf, IoBufMut};

pub(crate) fn build_readv_iovecs<B: IoBufMut>(bufs: &mut [B]) -> Box<[libc::iovec]> {
    bufs.iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.stable_mut_ptr().as_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

pub(crate) fn build_writev_iovecs<B: IoBuf>(bufs: &[B]) -> Box<[libc::iovec]> {
    bufs.iter()
        .map(|b| libc::iovec {
            iov_base: b.stable_ptr().as_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

