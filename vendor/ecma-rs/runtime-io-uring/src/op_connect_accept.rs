use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

struct ConnectMeta {
    addr: libc::sockaddr_storage,
    addr_len: libc::socklen_t,
}

struct AcceptMeta {
    addr: libc::sockaddr_storage,
    addr_len: libc::socklen_t,
}

/// Heap-owned address storage for `IORING_OP_CONNECT`.
///
/// The kernel may read from the passed `sockaddr*` until the connect CQE is produced, so this
/// buffer must live in non-moving memory owned by the op state.
pub struct ConnectAddr {
    meta: Box<ConnectMeta>,
}

/// Heap-owned address storage for `IORING_OP_ACCEPT`.
///
/// The kernel may write to both the `sockaddr*` and `socklen_t*` pointers until the accept CQE is
/// produced, so these buffers must live in non-moving memory owned by the op state.
pub struct AcceptAddr {
    meta: Box<AcceptMeta>,
}

impl ConnectAddr {
    pub fn new(addr: SocketAddr) -> Self {
        let (addr_storage, addr_len) = socket_addr_to_storage(addr);
        Self {
            meta: Box::new(ConnectMeta {
                addr: addr_storage,
                addr_len,
            }),
        }
    }

    pub fn addr_ptr(&self) -> *const libc::sockaddr {
        (&self.meta.addr as *const libc::sockaddr_storage).cast()
    }

    pub fn addr_len(&self) -> libc::socklen_t {
        self.meta.addr_len
    }
}

impl AcceptAddr {
    pub fn new() -> Self {
        Self {
            meta: Box::new(AcceptMeta {
                addr: unsafe { std::mem::zeroed() },
                addr_len: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            }),
        }
    }

    pub fn addr_ptr_const(&self) -> *const libc::sockaddr {
        (&self.meta.addr as *const libc::sockaddr_storage).cast()
    }

    pub fn addr_len_ptr_const(&self) -> *const libc::socklen_t {
        &self.meta.addr_len
    }

    pub fn addr_ptr(&mut self) -> *mut libc::sockaddr {
        (&mut self.meta.addr as *mut libc::sockaddr_storage).cast()
    }

    pub fn addr_len_ptr(&mut self) -> *mut libc::socklen_t {
        &mut self.meta.addr_len
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        storage_to_socket_addr(&self.meta.addr, self.meta.addr_len)
    }
}

impl fmt::Debug for ConnectAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectAddr")
            .field("ss_family", &self.meta.addr.ss_family)
            .field("addr_len", &self.meta.addr_len)
            .finish()
    }
}

impl fmt::Debug for AcceptAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptAddr")
            .field("ss_family", &self.meta.addr.ss_family)
            .field("addr_len", &self.meta.addr_len)
            .finish()
    }
}

pub(crate) fn socket_addr_to_storage(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };

    match addr {
        SocketAddr::V4(v4) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                // `sockaddr_in::sin_addr::s_addr` is stored in network byte order.
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                std::ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>(),
                    sin,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                // `sockaddr_in6::sin6_flowinfo` is stored in network byte order.
                sin6_flowinfo: v6.flowinfo().to_be(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            unsafe {
                std::ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>(),
                    sin6,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

pub(crate) fn storage_to_socket_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<SocketAddr> {
    match storage.ss_family as i32 {
        libc::AF_INET => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in>() {
                return None;
            }
            let sin = unsafe {
                &*(storage as *const libc::sockaddr_storage as *const libc::sockaddr_in)
            };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in6>() {
                return None;
            }
            let sin6 = unsafe {
                &*(storage as *const libc::sockaddr_storage as *const libc::sockaddr_in6)
            };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            let flowinfo = u32::from_be(sin6.sin6_flowinfo);
            Some(SocketAddr::V6(SocketAddrV6::new(
                ip,
                port,
                flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}
