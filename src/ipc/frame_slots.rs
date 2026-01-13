#![cfg(target_os = "linux")]

use crate::Error;
use crate::ipc::sync;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

const TAG_INIT: u32 = u32::from_le_bytes(*b"FSIN");
const TAG_FRAME_READY: u32 = u32::from_le_bytes(*b"FSRD");

const INIT_HEADER_LEN: usize = 16; // tag(u32) + slot_count(u32) + slot_size(u64)
const FRAME_READY_LEN: usize = 28; // tag(u32) + slot_id(u32) + w/h/stride(u32) + seq(u64)

fn cmsg_align(len: usize) -> usize {
  let align = mem::size_of::<usize>();
  (len + (align - 1)) & !(align - 1)
}

fn cmsg_space(data_len: usize) -> usize {
  cmsg_align(mem::size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

fn cmsg_len(data_len: usize) -> usize {
  cmsg_align(mem::size_of::<libc::cmsghdr>()) + data_len
}

fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
  let bytes = buf.get(offset..offset.checked_add(4)?)?;
  Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64_le(buf: &[u8], offset: usize) -> Option<u64> {
  let bytes = buf.get(offset..offset.checked_add(8)?)?;
  Some(u64::from_le_bytes([
    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
  ]))
}

fn write_u32_le(buf: &mut [u8], offset: usize, value: u32) -> Option<()> {
  let out = buf.get_mut(offset..offset.checked_add(4)?)?;
  out.copy_from_slice(&value.to_le_bytes());
  Some(())
}

fn write_u64_le(buf: &mut [u8], offset: usize, value: u64) -> Option<()> {
  let out = buf.get_mut(offset..offset.checked_add(8)?)?;
  out.copy_from_slice(&value.to_le_bytes());
  Some(())
}

/// A Linux `SOCK_SEQPACKET` Unix socket.
///
/// This is a very small wrapper intended for tests and multiprocess experiments.
pub struct UnixSeqpacket {
  fd: OwnedFd,
}

impl UnixSeqpacket {
  pub fn pair() -> Result<(Self, Self), Error> {
    let mut fds = [0; 2];
    // SAFETY: `socketpair` writes two fds into `fds` on success.
    let rc = unsafe {
      libc::socketpair(
        libc::AF_UNIX,
        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
        0,
        fds.as_mut_ptr(),
      )
    };
    if rc != 0 {
      return Err(Error::Io(io::Error::last_os_error()));
    }

    // SAFETY: `socketpair` returns valid fds on success.
    let left = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let right = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((Self { fd: left }, Self { fd: right }))
  }

  pub fn send(&self, data: &[u8]) -> Result<(), Error> {
    self.send_with_fds(data, &[])
  }

  pub fn send_with_fds(&self, data: &[u8], fds: &[RawFd]) -> Result<(), Error> {
    // Disallow empty payload messages. This ensures:
    // - `recvmsg` returning 0 bytes is unambiguous EOF, and
    // - `SCM_RIGHTS` is never sent without a byte payload (see `unix(7)`).
    if data.is_empty() {
      return Err(Error::Other(
        "seqpacket messages must contain at least one byte of payload data".to_string(),
      ));
    }

    let mut iov = libc::iovec {
      iov_base: data.as_ptr() as *mut libc::c_void,
      iov_len: data.len(),
    };

    let mut control_storage;
    let (control_ptr, control_len) = if fds.is_empty() {
      (ptr::null_mut(), 0)
    } else {
      let fds_bytes = fds
        .len()
        .checked_mul(mem::size_of::<RawFd>())
        .ok_or_else(|| Error::Other("too many fds".to_string()))?;
      control_storage = vec![0u8; cmsg_space(fds_bytes)];

      // SAFETY: The buffer is large enough for one cmsghdr + `fds_bytes` data.
      unsafe {
        let cmsg = control_storage.as_mut_ptr() as *mut libc::cmsghdr;
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = cmsg_len(fds_bytes) as _;

        let data_ptr = (cmsg as *mut u8).add(cmsg_align(mem::size_of::<libc::cmsghdr>()));
        ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, data_ptr, fds_bytes);
      }

      (
        control_storage.as_mut_ptr() as *mut libc::c_void,
        control_storage.len(),
      )
    };

    let mut hdr = libc::msghdr {
      msg_name: ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: &mut iov as *mut libc::iovec,
      msg_iovlen: 1,
      msg_control: control_ptr,
      msg_controllen: control_len,
      msg_flags: 0,
    };

    loop {
      // SAFETY: `hdr` points to valid data and control buffers for the duration of the call.
      let rc = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
      if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(Error::Io(err));
      }
      if rc as usize != data.len() {
        return Err(Error::Io(io::Error::new(
          io::ErrorKind::WriteZero,
          "short sendmsg on seqpacket socket",
        )));
      }
      return Ok(());
    }
  }

  pub fn recv_with_fds(&self, buf: &mut [u8], max_fds: usize) -> Result<(usize, Vec<OwnedFd>), Error> {
    if buf.is_empty() {
      return Err(Error::Other("recv buffer must be non-empty".to_string()));
    }

    let mut iov = libc::iovec {
      iov_base: buf.as_mut_ptr() as *mut libc::c_void,
      iov_len: buf.len(),
    };

    let mut control_storage;
    let (control_ptr, control_len) = if max_fds == 0 {
      (ptr::null_mut(), 0)
    } else {
      let max_fds_bytes = max_fds
        .checked_mul(mem::size_of::<RawFd>())
        .ok_or_else(|| Error::Other("max_fds overflow".to_string()))?;
      control_storage = vec![0u8; cmsg_space(max_fds_bytes)];
      (
        control_storage.as_mut_ptr() as *mut libc::c_void,
        control_storage.len(),
      )
    };

    let mut hdr = libc::msghdr {
      msg_name: ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: &mut iov as *mut libc::iovec,
      msg_iovlen: 1,
      msg_control: control_ptr,
      msg_controllen: control_len,
      msg_flags: 0,
    };

    let mut need_manual_cloexec = false;
    let rc = loop {
      // `recvmsg` mutates `msg_controllen` on success; reset it for retries.
      hdr.msg_controllen = control_len;
      hdr.msg_flags = 0;

      // SAFETY: `hdr` points to valid buffers.
      let rc = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
      if rc >= 0 {
        break rc;
      }

      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }

      // Some environments reject MSG_CMSG_CLOEXEC with EINVAL; retry without and set FD_CLOEXEC
      // manually on any received fds.
      if err.raw_os_error() == Some(libc::EINVAL) {
        need_manual_cloexec = true;
        let rc2 = loop {
          hdr.msg_controllen = control_len;
          hdr.msg_flags = 0;
          let rc2 = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut hdr, 0) };
          if rc2 >= 0 {
            break rc2;
          }
          let err2 = io::Error::last_os_error();
          if err2.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(Error::Io(err2));
        };
        break rc2;
      }

      return Err(Error::Io(err));
    };

    let mut fds_out: Vec<OwnedFd> = Vec::new();

    if hdr.msg_controllen > 0 {
      let start = hdr.msg_control as usize;
      let end = start
        .checked_add(hdr.msg_controllen)
        .ok_or_else(|| Error::Other("control buffer overflow".to_string()))?;
      let mut cmsg_ptr = hdr.msg_control as *const libc::cmsghdr;

      while (cmsg_ptr as usize)
        .checked_add(mem::size_of::<libc::cmsghdr>())
        .is_some_and(|next| next <= end)
      {
        // SAFETY: Bounds checked above.
        let cmsg = unsafe { &*cmsg_ptr };
        let cmsg_len_raw = cmsg.cmsg_len as usize;
        let header_aligned = cmsg_align(mem::size_of::<libc::cmsghdr>());
        if cmsg_len_raw < header_aligned {
          break;
        }

        let cmsg_end = (cmsg_ptr as usize).checked_add(cmsg_len_raw);
        let Some(cmsg_end) = cmsg_end else { break };
        if cmsg_end > end {
          break;
        }

        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
          let data_len = cmsg_len_raw - header_aligned;
          if data_len % mem::size_of::<RawFd>() != 0 {
            // Close any FDs we already pulled out before returning.
            drop(fds_out);
            return Err(Error::Other("misaligned SCM_RIGHTS payload".to_string()));
          }

          let fd_count = data_len / mem::size_of::<RawFd>();
          let data_ptr = (cmsg_ptr as *const u8).wrapping_add(header_aligned) as *const RawFd;
          // SAFETY: `data_ptr` is inside the received control buffer and `fd_count` is bounds-checked.
          let fd_slice = unsafe { std::slice::from_raw_parts(data_ptr, fd_count) };
          for &fd in fd_slice {
            // SAFETY: Received fds are owned by the receiver.
            fds_out.push(unsafe { OwnedFd::from_raw_fd(fd) });
          }
        }

        let next = (cmsg_ptr as usize).checked_add(cmsg_align(cmsg_len_raw));
        let Some(next) = next else { break };
        if next <= cmsg_ptr as usize {
          break;
        }
        cmsg_ptr = next as *const libc::cmsghdr;
      }
    }

    // After parsing (so any received fds are wrapped/closed on error), validate the recvmsg flags.
    if (hdr.msg_flags & libc::MSG_TRUNC) != 0 {
      drop(fds_out);
      return Err(Error::Other("truncated seqpacket message".to_string()));
    }
    if (hdr.msg_flags & libc::MSG_CTRUNC) != 0 {
      drop(fds_out);
      return Err(Error::Other("truncated control message".to_string()));
    }
    if rc == 0 {
      drop(fds_out);
      return Err(Error::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "peer closed seqpacket socket",
      )));
    }

    if need_manual_cloexec {
      for fd in &fds_out {
        let flags = loop {
          // SAFETY: `fcntl` called with valid fd.
          let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
          if flags >= 0 {
            break flags;
          }
          let err = io::Error::last_os_error();
          if err.kind() == io::ErrorKind::Interrupted {
            continue;
          }
          return Err(Error::Io(err));
        };
        if (flags & libc::FD_CLOEXEC) == 0 {
          loop {
            // SAFETY: `fcntl` called with valid fd.
            let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            if rc >= 0 {
              break;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
              continue;
            }
            return Err(Error::Io(err));
          }
        }
      }
    }

    Ok((rc as usize, fds_out))
  }
}

/// A single shared-memory slot backed by a Linux `memfd`.
pub struct SharedMemory {
  fd: OwnedFd,
  size: usize,
}

impl SharedMemory {
  pub fn new(size: u64) -> Result<Self, Error> {
    if size == 0 {
      return Err(Error::Other("shared memory size must be non-zero".to_string()));
    }
    let size_usize = usize::try_from(size)
      .map_err(|_| Error::Other("shared memory too large for this platform".to_string()))?;

    // SAFETY: Name is NUL-terminated.
    let mut fd = unsafe {
      libc::memfd_create(
        b"fastrender-frame-slot\0".as_ptr() as *const libc::c_char,
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
      )
    };
    if fd < 0 {
      let err = io::Error::last_os_error();
      // Older kernels may not support `MFD_ALLOW_SEALING`. Fall back so the seqpacket prototype can
      // still run (but note that size-sealing will be best-effort below).
      if err.raw_os_error() == Some(libc::EINVAL) {
        fd = unsafe {
          libc::memfd_create(
            b"fastrender-frame-slot\0".as_ptr() as *const libc::c_char,
            libc::MFD_CLOEXEC,
          )
        };
      }
      if fd < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
      }
    }

    // SAFETY: `fd` is valid on success.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: ftruncate is safe with a valid fd.
    let rc = unsafe { libc::ftruncate(owned.as_raw_fd(), size as libc::off_t) };
    if rc != 0 {
      return Err(Error::Io(io::Error::last_os_error()));
    }

    let shm = Self { fd: owned, size: size_usize };

    // Best-effort: prevent the renderer from shrinking/growing the slot (SIGBUS footgun), and then
    // lock the seal set so the renderer cannot persistently add `F_SEAL_WRITE` (breaking future
    // reuse of pooled slots). See `docs/ipc_linux_fd_passing.md` (seals checklist).
    //
    // Apply required size-stability seals first; treat `F_SEAL_SEAL` as optional so a kernel that
    // can't lock the seal set still gets the SIGBUS protection.
    let required_seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    let required_ok = loop {
      let rc = unsafe { libc::fcntl(shm.fd.as_raw_fd(), libc::F_ADD_SEALS, required_seals) };
      if rc == 0 {
        break true;
      }
      let err = io::Error::last_os_error();
      if err.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      match err.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => break false,
        _ => return Err(Error::Io(err)),
      }
    };
    if required_ok {
      loop {
        let rc = unsafe { libc::fcntl(shm.fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SEAL) };
        if rc == 0 {
          break;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        match err.raw_os_error() {
          Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => break,
          _ => return Err(Error::Io(err)),
        }
      }
    }

    // Security: make it explicit that fresh shared-memory slots start zeroed before any untrusted
    // renderer maps them. Even if the kernel typically provides zeroed pages, an explicit clear
    // avoids leaking stale bytes if an fd were ever reused accidentally.
    let mut mapping = shm.map_mut()?;
    mapping.as_slice_mut().fill(0);

    Ok(shm)
  }

  fn from_fd(fd: OwnedFd, size: u64) -> Result<Self, Error> {
    if size == 0 {
      return Err(Error::Other("shared memory size must be non-zero".to_string()));
    }
    let size_usize = usize::try_from(size)
      .map_err(|_| Error::Other("shared memory too large for this platform".to_string()))?;

    // Validate that the backing fd is at least `size` bytes so mapping doesn't SIGBUS on access.
    let mut stat: libc::stat = unsafe { mem::zeroed() };
    // SAFETY: fstat is safe with a valid fd.
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc != 0 {
      return Err(Error::Io(io::Error::last_os_error()));
    }
    if stat.st_size < size as libc::off_t {
      return Err(Error::Other("received shm fd is smaller than expected".to_string()));
    }

    Ok(Self { fd, size: size_usize })
  }

  pub fn size(&self) -> usize {
    self.size
  }

  pub fn as_raw_fd(&self) -> RawFd {
    self.fd.as_raw_fd()
  }

  pub fn map_mut(&self) -> Result<MappedSharedMemory, Error> {
    let len = self.size;
    if len == 0 {
      return Err(Error::Other("cannot map zero-sized shared memory".to_string()));
    }

    // SAFETY: We map `len` bytes from the memfd starting at offset 0. The fd is validated to be
    // large enough in `new`/`from_fd`.
    let ptr = unsafe {
      libc::mmap(
        ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        self.fd.as_raw_fd(),
        0,
      )
    };
    if ptr == libc::MAP_FAILED {
      return Err(Error::Io(io::Error::last_os_error()));
    }

    Ok(MappedSharedMemory { ptr: ptr as *mut u8, len })
  }
}

pub struct MappedSharedMemory {
  ptr: *mut u8,
  len: usize,
}

impl MappedSharedMemory {
  pub fn as_slice(&self) -> &[u8] {
    // SAFETY: The mapping is valid for `len` bytes for the lifetime of `self`.
    unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
  }

  pub fn as_slice_mut(&mut self) -> &mut [u8] {
    // SAFETY: The mapping is valid for `len` bytes for the lifetime of `self`.
    unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
  }
}

impl Drop for MappedSharedMemory {
  fn drop(&mut self) {
    // SAFETY: munmap is safe with a valid mapping; ignore errors on drop.
    unsafe {
      libc::munmap(self.ptr as *mut libc::c_void, self.len);
    }
  }
}

pub struct FrameSlots {
  slot_count: usize,
  slot_size: u64,
  slots: Vec<(u32, SharedMemory)>,
}

impl FrameSlots {
  pub fn new(slot_count: usize, slot_size: u64) -> Result<FrameSlots, Error> {
    if slot_count > (u32::MAX as usize) {
      return Err(Error::Other("slot_count does not fit u32".to_string()));
    }
    if slot_size == 0 {
      return Err(Error::Other("slot_size must be non-zero".to_string()));
    }

    let mut slots = Vec::with_capacity(slot_count);
    for idx in 0..slot_count {
      let id = u32::try_from(idx)
        .map_err(|_| Error::Other("slot id overflow".to_string()))?;
      let shm = SharedMemory::new(slot_size)?;
      slots.push((id, shm));
    }

    Ok(Self { slot_count, slot_size, slots })
  }

  pub fn send_init(&self, sock: &UnixSeqpacket) -> Result<(), Error> {
    let slot_count_u32 =
      u32::try_from(self.slot_count).map_err(|_| Error::Other("slot_count overflow".to_string()))?;

    let msg_len = INIT_HEADER_LEN
      .checked_add(self.slot_count.checked_mul(4).ok_or_else(|| Error::Other("slot_count overflow".to_string()))?)
      .ok_or_else(|| Error::Other("init message too large".to_string()))?;

    let mut msg = vec![0u8; msg_len];
    write_u32_le(&mut msg, 0, TAG_INIT).ok_or_else(|| Error::Other("init message encode failed".to_string()))?;
    write_u32_le(&mut msg, 4, slot_count_u32).ok_or_else(|| Error::Other("init message encode failed".to_string()))?;
    write_u64_le(&mut msg, 8, self.slot_size).ok_or_else(|| Error::Other("init message encode failed".to_string()))?;

    for (i, (slot_id, _)) in self.slots.iter().enumerate() {
      let off = INIT_HEADER_LEN
        .checked_add(i.checked_mul(4).ok_or_else(|| Error::Other("slot id offset overflow".to_string()))?)
        .ok_or_else(|| Error::Other("slot id offset overflow".to_string()))?;
      write_u32_le(&mut msg, off, *slot_id)
        .ok_or_else(|| Error::Other("init message encode failed".to_string()))?;
    }

    let fds: Vec<RawFd> = self.slots.iter().map(|(_, shm)| shm.as_raw_fd()).collect();
    sock.send_with_fds(&msg, &fds)?;
    Ok(())
  }
}

pub fn recv_init(
  sock: &UnixSeqpacket,
  max_slots: usize,
  max_slot_size: u64,
) -> Result<Vec<(u32, SharedMemory)>, Error> {
  if max_slots > (u32::MAX as usize) {
    return Err(Error::Other("max_slots does not fit u32".to_string()));
  }

  let max_msg_len = INIT_HEADER_LEN
    .checked_add(max_slots.checked_mul(4).ok_or_else(|| Error::Other("max_slots overflow".to_string()))?)
    .ok_or_else(|| Error::Other("max message length overflow".to_string()))?;

  let mut buf = vec![0u8; max_msg_len];
  let (n, fds) = sock.recv_with_fds(&mut buf, max_slots)?;

  let close_all_fds = |fds: Vec<OwnedFd>| drop(fds);

  if n < INIT_HEADER_LEN {
    close_all_fds(fds);
    return Err(Error::Other("init message too short".to_string()));
  }

  let tag = read_u32_le(&buf, 0).unwrap_or(0);
  if tag != TAG_INIT {
    close_all_fds(fds);
    return Err(Error::Other("unexpected init tag".to_string()));
  }

  let slot_count = read_u32_le(&buf, 4).ok_or_else(|| Error::Other("missing slot_count".to_string()))? as usize;
  if slot_count > max_slots {
    close_all_fds(fds);
    return Err(Error::Other("slot_count exceeds max_slots".to_string()));
  }

  let slot_size = read_u64_le(&buf, 8).ok_or_else(|| Error::Other("missing slot_size".to_string()))?;
  if slot_size == 0 {
    close_all_fds(fds);
    return Err(Error::Other("slot_size must be non-zero".to_string()));
  }
  if slot_size > max_slot_size {
    close_all_fds(fds);
    return Err(Error::Other("slot_size exceeds max_slot_size".to_string()));
  }

  let expected_len = INIT_HEADER_LEN
    .checked_add(slot_count.checked_mul(4).ok_or_else(|| Error::Other("slot_count overflow".to_string()))?)
    .ok_or_else(|| Error::Other("expected_len overflow".to_string()))?;
  if n != expected_len {
    close_all_fds(fds);
    return Err(Error::Other("init message has unexpected length".to_string()));
  }

  if fds.len() != slot_count {
    close_all_fds(fds);
    return Err(Error::Other("init fd count mismatch".to_string()));
  }

  let mut slot_ids: Vec<u32> = Vec::with_capacity(slot_count);
  for i in 0..slot_count {
    let off = INIT_HEADER_LEN
      .checked_add(i.checked_mul(4).ok_or_else(|| Error::Other("slot id offset overflow".to_string()))?)
      .ok_or_else(|| Error::Other("slot id offset overflow".to_string()))?;
    let slot_id = read_u32_le(&buf, off).ok_or_else(|| Error::Other("missing slot id".to_string()))?;
    if slot_ids.contains(&slot_id) {
      close_all_fds(fds);
      return Err(Error::Other("duplicate slot id".to_string()));
    }
    slot_ids.push(slot_id);
  }

  let mut out = Vec::with_capacity(slot_count);
  let mut fds_iter = fds.into_iter();
  for slot_id in slot_ids {
    let Some(fd) = fds_iter.next() else {
      return Err(Error::Other("missing shm fd".to_string()));
    };
    let shm = match SharedMemory::from_fd(fd, slot_size) {
      Ok(shm) => shm,
      Err(e) => {
        // Remaining fds (if any) need to be closed.
        for fd in fds_iter {
          drop(fd);
        }
        return Err(e);
      }
    };
    out.push((slot_id, shm));
  }

  Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameReady {
  pub slot_id: u32,
  pub width: u32,
  pub height: u32,
  pub stride: u32,
  pub seq: u64,
}

pub fn send_frame_ready(sock: &UnixSeqpacket, msg: FrameReady) -> Result<(), Error> {
  // Publish shared-memory writes (frame pixels) before notifying the other process that a frame is
  // ready. Pair with the Acquire fence in `recv_frame_ready`.
  sync::shm_publish_frame();
  let mut buf = [0u8; FRAME_READY_LEN];
  write_u32_le(&mut buf, 0, TAG_FRAME_READY).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  write_u32_le(&mut buf, 4, msg.slot_id).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  write_u32_le(&mut buf, 8, msg.width).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  write_u32_le(&mut buf, 12, msg.height).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  write_u32_le(&mut buf, 16, msg.stride).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  write_u64_le(&mut buf, 20, msg.seq).ok_or_else(|| Error::Other("frame_ready encode failed".to_string()))?;
  sock.send(&buf)?;
  Ok(())
}

pub fn recv_frame_ready(sock: &UnixSeqpacket) -> Result<FrameReady, Error> {
  let mut buf = [0u8; FRAME_READY_LEN];
  let (n, fds) = sock.recv_with_fds(&mut buf, 4)?;
  if !fds.is_empty() {
    drop(fds);
    return Err(Error::Other("frame_ready must not include fds".to_string()));
  }
  if n != FRAME_READY_LEN {
    return Err(Error::Other("frame_ready has unexpected length".to_string()));
  }

  let tag = read_u32_le(&buf, 0).unwrap_or(0);
  if tag != TAG_FRAME_READY {
    return Err(Error::Other("unexpected frame_ready tag".to_string()));
  }

  let slot_id = read_u32_le(&buf, 4).ok_or_else(|| Error::Other("missing slot_id".to_string()))?;
  let width = read_u32_le(&buf, 8).ok_or_else(|| Error::Other("missing width".to_string()))?;
  let height = read_u32_le(&buf, 12).ok_or_else(|| Error::Other("missing height".to_string()))?;
  let stride = read_u32_le(&buf, 16).ok_or_else(|| Error::Other("missing stride".to_string()))?;
  let seq = read_u64_le(&buf, 20).ok_or_else(|| Error::Other("missing seq".to_string()))?;

  // Consume the readiness signal before reading from the shared-memory slot. This prevents the CPU
  // and compiler from reordering subsequent loads from the slot to before the `FrameReady` message
  // was observed. Pair with the Release fence in `send_frame_ready`.
  sync::shm_consume_frame();

  Ok(FrameReady { slot_id, width, height, stride, seq })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ipc::sync;

  #[test]
  fn init_and_frame_ready_smoke() -> Result<(), Error> {
    let (browser_sock, renderer_sock) = UnixSeqpacket::pair()?;

    let publish_before = sync::shm_publish_count_for_test();
    let consume_before = sync::shm_consume_count_for_test();

    let browser_slots = FrameSlots::new(2, 4096)?;
    browser_slots.send_init(&browser_sock)?;

    let renderer_slots = recv_init(&renderer_sock, 8, 16 * 1024)?;
    assert_eq!(renderer_slots.len(), 2);
    assert_eq!(renderer_slots[0].0, 0);

    // Renderer writes a known pattern into slot 0.
    let mut renderer_map = renderer_slots[0].1.map_mut()?;
    for (idx, b) in renderer_map.as_slice_mut().iter_mut().enumerate() {
      *b = (idx as u8).wrapping_mul(31);
    }

    send_frame_ready(
      &renderer_sock,
      FrameReady { slot_id: 0, width: 64, height: 64, stride: 256, seq: 1 },
    )?;
    assert!(
      sync::shm_publish_count_for_test() > publish_before,
      "expected publish fence to run before sending FrameReady"
    );

    let ready = recv_frame_ready(&browser_sock)?;
    assert!(
      sync::shm_consume_count_for_test() > consume_before,
      "expected consume fence to run after receiving FrameReady"
    );
    assert_eq!(ready.slot_id, 0);
    assert_eq!(ready.seq, 1);

    // Browser reads the bytes back from its own mapping of slot 0.
    let mut browser_map = browser_slots.slots[0].1.map_mut()?;
    assert_eq!(browser_map.as_slice()[0], 0);
    assert_eq!(browser_map.as_slice()[1], 31);
    assert_eq!(browser_map.as_slice()[2], 62);
    assert_eq!(browser_map.as_slice()[3], 93);
    Ok(())
  }

  #[test]
  fn recv_init_rejects_malformed_inputs_without_panicking() -> Result<(), Error> {
    // Short message.
    let (sock_a, sock_b) = UnixSeqpacket::pair()?;
    sock_a.send(&[0u8; 3])?;
    assert!(recv_init(&sock_b, 4, 4096).is_err());

    // slot_count too large.
    let (sock_a, sock_b) = UnixSeqpacket::pair()?;
    let mut msg = [0u8; INIT_HEADER_LEN];
    write_u32_le(&mut msg, 0, TAG_INIT).unwrap();
    write_u32_le(&mut msg, 4, 5).unwrap(); // > max_slots
    write_u64_le(&mut msg, 8, 1024).unwrap();
    sock_a.send(&msg)?;
    assert!(recv_init(&sock_b, 4, 4096).is_err());

    // slot_size too large.
    let (sock_a, sock_b) = UnixSeqpacket::pair()?;
    let mut msg = [0u8; INIT_HEADER_LEN];
    write_u32_le(&mut msg, 0, TAG_INIT).unwrap();
    write_u32_le(&mut msg, 4, 1).unwrap();
    write_u64_le(&mut msg, 8, 5000).unwrap(); // > max_slot_size
    sock_a.send(&msg)?;
    assert!(recv_init(&sock_b, 4, 4096).is_err());

    Ok(())
  }

  #[test]
  fn send_with_fds_rejects_fd_only_messages() -> Result<(), Error> {
    let (sock, _peer) = UnixSeqpacket::pair()?;
    let shm = SharedMemory::new(1024)?;
    let err = sock
      .send_with_fds(&[], &[shm.as_raw_fd()])
      .expect_err("expected fd-only send to be rejected");
    assert!(matches!(err, Error::Other(_)), "unexpected error: {err:?}");
    Ok(())
  }

  #[test]
  fn shared_memory_new_zero_initializes() -> Result<(), Error> {
    let shm = SharedMemory::new(256)?;
    let map = shm.map_mut()?;
    assert!(
      map.as_slice().iter().all(|b| *b == 0),
      "newly created shared-memory slots should be zero-initialized"
    );
    Ok(())
  }

  #[test]
  fn shared_memory_slots_are_sealed_against_resize_when_supported() -> Result<(), Error> {
    let shm = SharedMemory::new(1024)?;

    let seals = unsafe { libc::fcntl(shm.as_raw_fd(), libc::F_GET_SEALS) };
    if seals == -1 {
      let err = std::io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => return Ok(()),
        _ => return Err(Error::Io(err)),
      }
    }

    let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    // Older kernels create memfds without `MFD_ALLOW_SEALING` in a permanently unsealable state
    // (`F_SEAL_SEAL` only). Treat that as "sealing unsupported" for this test.
    if (seals & libc::F_SEAL_SEAL) != 0 && (seals & required) != required {
      return Ok(());
    }

    assert_eq!(
      seals & required,
      required,
      "expected frame-slot memfd to have shrink/grow seals (got seals=0x{seals:x})"
    );
    Ok(())
  }
}
