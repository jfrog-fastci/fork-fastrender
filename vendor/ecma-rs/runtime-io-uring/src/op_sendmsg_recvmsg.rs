use std::io;

use crate::buf::{IoBuf, IoBufMut};

pub struct SendMsg<'a, B> {
    pub(crate) bufs: Vec<B>,
    pub(crate) flags: libc::c_int,
    pub(crate) name: Option<&'a [u8]>,
    pub(crate) control: Option<&'a [u8]>,
}

impl<'a, B> SendMsg<'a, B> {
    pub fn new(bufs: Vec<B>) -> Self {
        Self {
            bufs,
            flags: 0,
            name: None,
            control: None,
        }
    }

    pub fn flags(mut self, flags: libc::c_int) -> Self {
        self.flags = flags;
        self
    }

    pub fn name(mut self, name: &'a [u8]) -> Self {
        self.name = Some(name);
        self
    }

    pub fn control(mut self, control: &'a [u8]) -> Self {
        self.control = Some(control);
        self
    }
}

pub struct RecvMsg<B> {
    pub(crate) bufs: Vec<B>,
    pub(crate) flags: libc::c_int,
    pub(crate) want_name: bool,
    pub(crate) control_len: Option<usize>,
}

impl<B> RecvMsg<B> {
    pub fn new(bufs: Vec<B>) -> Self {
        Self {
            bufs,
            flags: 0,
            want_name: false,
            control_len: None,
        }
    }

    pub fn flags(mut self, flags: libc::c_int) -> Self {
        self.flags = flags;
        self
    }

    pub fn name(mut self) -> Self {
        self.want_name = true;
        self
    }

    pub fn control_len(mut self, len: usize) -> Self {
        self.control_len = Some(len);
        self
    }
}

#[derive(Debug)]
pub struct RecvMsgResource<B> {
    pub bufs: Vec<B>,
    name: Option<Box<libc::sockaddr_storage>>,
    name_len: usize,
    control: Option<Box<[u8]>>,
    control_len: usize,
    msg_flags: libc::c_int,
}

impl<B> RecvMsgResource<B> {
    pub(crate) fn new(
        bufs: Vec<B>,
        name: Option<Box<libc::sockaddr_storage>>,
        name_len: usize,
        control: Option<Box<[u8]>>,
        control_len: usize,
        msg_flags: libc::c_int,
    ) -> Self {
        Self {
            bufs,
            name,
            name_len,
            control,
            control_len,
            msg_flags,
        }
    }

    pub fn msg_flags(&self) -> libc::c_int {
        self.msg_flags
    }

    pub fn name(&self) -> Option<&[u8]> {
        self.name.as_ref().map(|s| unsafe {
            std::slice::from_raw_parts(
                (s.as_ref() as *const libc::sockaddr_storage).cast::<u8>(),
                self.name_len,
            )
        })
    }

    pub fn control(&self) -> Option<&[u8]> {
        self.control
            .as_ref()
            .map(|c| &c[..self.control_len])
    }
}

pub(crate) fn copy_sockaddr_storage(name_bytes: &[u8]) -> io::Result<Box<libc::sockaddr_storage>> {
    if name_bytes.len() > std::mem::size_of::<libc::sockaddr_storage>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sockaddr larger than sockaddr_storage",
        ));
    }

    let mut storage: Box<libc::sockaddr_storage> = Box::new(unsafe { std::mem::zeroed() });
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            (storage.as_mut() as *mut libc::sockaddr_storage).cast::<u8>(),
            name_bytes.len(),
        );
    }
    Ok(storage)
}

pub(crate) fn build_sendmsg_iovecs<B: IoBuf>(bufs: &[B]) -> Box<[libc::iovec]> {
    bufs.iter()
        .map(|b| libc::iovec {
            iov_base: b.stable_ptr().as_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

pub(crate) fn build_recvmsg_iovecs<B: IoBufMut>(bufs: &mut [B]) -> Box<[libc::iovec]> {
    bufs.iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.stable_mut_ptr().as_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}
