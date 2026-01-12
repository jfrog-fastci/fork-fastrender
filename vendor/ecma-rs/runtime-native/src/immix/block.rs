use std::alloc::Layout;
use std::ptr::NonNull;

use super::bitmap;
use super::BLOCK_SIZE;
use super::LINE_SIZE;
use super::LINES_PER_BLOCK;
use core::sync::atomic::AtomicU64;

#[derive(Clone, Copy, Debug)]
enum BlockAllocKind {
  Mmap,
  Heap,
}

/// An Immix block: a 32KB region with a 256-bit line map.
#[derive(Debug)]
pub struct Block {
  pub start: NonNull<u8>,
  pub line_map: bitmap::LineMap,
  pub id: usize,
  alloc_kind: BlockAllocKind,
}

impl Block {
  pub fn new(id: usize) -> Option<Self> {
    let (start, alloc_kind) = alloc_block_aligned()?;
    Some(Self {
      start,
      line_map: core::array::from_fn(|_| AtomicU64::new(0)),
      id,
      alloc_kind,
    })
  }

  #[inline]
  pub fn start_addr(&self) -> usize {
    self.start.as_ptr() as usize
  }

  #[inline]
  pub fn end_addr(&self) -> usize {
    self.start_addr() + BLOCK_SIZE
  }

  #[inline]
  pub fn clear_line_map(&mut self) {
    bitmap::clear(&self.line_map);
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    bitmap::is_empty(&self.line_map)
  }

  #[inline]
  pub fn contains_addr(&self, addr: usize) -> bool {
    addr >= self.start_addr() && addr < self.end_addr()
  }

  pub fn mark_addr_range(&self, addr_start: usize, addr_end: usize) {
    debug_assert!(addr_start <= addr_end);
    debug_assert!(self.contains_addr(addr_start));
    debug_assert!(addr_end <= self.end_addr());

    let start_off = addr_start - self.start_addr();
    let end_off = addr_end - self.start_addr();
    let start_line = start_off / LINE_SIZE;
    let end_line = end_off.div_ceil(LINE_SIZE);
    debug_assert!(start_line < LINES_PER_BLOCK);
    debug_assert!(end_line <= LINES_PER_BLOCK);
    bitmap::set_range(&self.line_map, start_line, end_line);
  }

  pub fn metrics(&self) -> BlockMetrics {
    BlockMetrics {
      free_lines: bitmap::free_lines(&self.line_map),
      largest_hole_lines: bitmap::largest_hole_lines(&self.line_map),
    }
  }
}

impl Drop for Block {
  fn drop(&mut self) {
    unsafe {
      match self.alloc_kind {
        BlockAllocKind::Mmap => free_block_mmap(self.start),
        BlockAllocKind::Heap => free_block_heap(self.start),
      }
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockMetrics {
  pub free_lines: usize,
  pub largest_hole_lines: usize,
}

fn alloc_block_aligned() -> Option<(NonNull<u8>, BlockAllocKind)> {
  #[cfg(unix)]
  unsafe {
    if let Some(ptr) = alloc_block_mmap() {
      return Some((ptr, BlockAllocKind::Mmap));
    }
  }

  unsafe { alloc_block_heap().map(|ptr| (ptr, BlockAllocKind::Heap)) }
}

unsafe fn alloc_block_heap() -> Option<NonNull<u8>> {
  let layout = Layout::from_size_align(BLOCK_SIZE, BLOCK_SIZE).ok()?;
  let ptr = std::alloc::alloc(layout);
  NonNull::new(ptr)
}

unsafe fn free_block_heap(ptr: NonNull<u8>) {
  let layout = Layout::from_size_align_unchecked(BLOCK_SIZE, BLOCK_SIZE);
  std::alloc::dealloc(ptr.as_ptr(), layout);
}

#[cfg(unix)]
unsafe fn alloc_block_mmap() -> Option<NonNull<u8>> {
  let page_size = libc::sysconf(libc::_SC_PAGESIZE);
  if page_size <= 0 {
    return None;
  }
  let page_size = page_size as usize;
  if BLOCK_SIZE % page_size != 0 {
    // We rely on being able to `munmap` prefix/suffix chunks on page boundaries.
    return None;
  }

  let size = BLOCK_SIZE * 2;
  let base = loop {
    let base = libc::mmap(
      std::ptr::null_mut(),
      size,
      libc::PROT_READ | libc::PROT_WRITE,
      libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
      -1,
      0,
    );
    if base == libc::MAP_FAILED {
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      return None;
    }
    if base.is_null() {
      // Mapping at address 0 is unexpected; unmap and fall back to heap allocation.
      let _ = libc::munmap(base, size);
      return None;
    }
    break base;
  };

  let base_addr = base as usize;
  let aligned_addr = (base_addr + (BLOCK_SIZE - 1)) & !(BLOCK_SIZE - 1);
  debug_assert_eq!(aligned_addr % BLOCK_SIZE, 0);

  let prefix = aligned_addr - base_addr;
  if prefix > 0 {
    loop {
      let rc = libc::munmap(base, prefix);
      if rc == 0 {
        break;
      }
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      debug_assert_eq!(rc, 0);
      break;
    }
  }

  let suffix_addr = aligned_addr + BLOCK_SIZE;
  let end_addr = base_addr + size;
  let suffix = end_addr - suffix_addr;
  if suffix > 0 {
    loop {
      let rc = libc::munmap(suffix_addr as *mut libc::c_void, suffix);
      if rc == 0 {
        break;
      }
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINTR) {
        continue;
      }
      debug_assert_eq!(rc, 0);
      break;
    }
  }

  Some(NonNull::new_unchecked(aligned_addr as *mut u8))
}

#[cfg(unix)]
unsafe fn free_block_mmap(ptr: NonNull<u8>) {
  loop {
    let rc = libc::munmap(ptr.as_ptr() as *mut libc::c_void, BLOCK_SIZE);
    if rc == 0 {
      break;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EINTR) {
      continue;
    }
    debug_assert_eq!(rc, 0);
    break;
  }
}

#[cfg(not(unix))]
unsafe fn free_block_mmap(_ptr: NonNull<u8>) {
  unreachable!("mmap-backed blocks are only used on unix targets");
}
