//! Shared-memory frame buffer pool management for multiprocess rendering.
//!
//! This module is designed to be reused by both the browser process (frame buffer allocation +
//! mapping) and the renderer process (buffer acquisition + release gated by browser acks).
//!
//! In the multiprocess architecture, a rendered frame buffer must not be reused/overwritten by the
//! renderer until the browser has finished uploading/copying (or explicitly dropping) the frame.
//! The browser signals that via an explicit acknowledgement message (e.g.
//! `ipc::protocol::renderer::BrowserToRenderer::FrameAck { frame_seq }`).

use crate::ipc::IpcError;
use crate::ipc::limits::{BYTES_PER_PIXEL, MAX_FRAME_BUFFERS};
use crate::ipc::sync;
use crate::paint::pixmap::MAX_PIXMAP_BYTES;
use memmap2::MmapMut;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::NamedTempFile;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn invalid_parameters(message: impl Into<String>) -> IpcError {
  IpcError::Io(std::io::Error::new(
    std::io::ErrorKind::InvalidInput,
    message.into(),
  ))
}

fn protocol_violation(message: impl Into<String>) -> IpcError {
  IpcError::Io(std::io::Error::new(
    std::io::ErrorKind::InvalidData,
    message.into(),
  ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameBufferLayout {
  pub width_px: u32,
  pub height_px: u32,
  pub stride_bytes: u32,
  pub byte_len: u64,
}

/// Descriptor that the browser can send to the renderer so it can open and map a frame buffer.
///
/// Note: The transport of these descriptors is out of scope for this module; it simply provides
/// stable data that can be sent over an IPC channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameBufferDesc {
  pub index: u32,
  pub width_px: u32,
  pub height_px: u32,
  pub stride_bytes: u32,
  pub byte_len: u64,
  pub path: PathBuf,
}

impl FrameBufferDesc {
  pub fn layout(&self) -> Result<FrameBufferLayout, IpcError> {
    frame_buffer_layout(self.width_px, self.height_px)
  }
}

#[derive(Debug)]
pub struct ShmemRegion {
  mmap: MmapMut,
  backing: ShmemBacking,
  path: PathBuf,
}

#[derive(Debug)]
enum ShmemBacking {
  Temp(NamedTempFile),
  File(File),
}

impl ShmemRegion {
  /// Create and map a new shared-memory region of `len` bytes.
  ///
  /// Security: we explicitly zero-initialize the mapping before returning. Even though most OS
  /// APIs hand out zeroed pages for new mappings, making the invariant explicit avoids leaking
  /// previous-process memory (or stale named-shm contents) if a backing name/path is ever reused
  /// accidentally.
  pub fn create(len: u64) -> Result<Self, IpcError> {
    let mut region = Self::create_temp_mapped(len)?;
    region.as_bytes_mut().fill(0);
    Ok(region)
  }

  fn create_temp_mapped(len: u64) -> Result<Self, IpcError> {
    let len_usize = usize::try_from(len).map_err(|_| IpcError::ArithmeticOverflow)?;

    let mut tmp = tempfile::Builder::new()
      .prefix("fastr_framebuf_")
      .tempfile()?;

    tmp.as_file_mut().set_len(len)?;
    let path = tmp.path().to_path_buf();

    // SAFETY: We just sized the file to `len` and keep the handle alive, so the mapping length
    // remains valid for the lifetime of the mapping.
    let mmap = unsafe { MmapMut::map_mut(tmp.as_file())? };
    if mmap.len() != len_usize {
      return Err(IpcError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!(
          "memory map length mismatch: expected {len_usize} bytes, got {} bytes",
          mmap.len()
        ),
      )));
    }

    Ok(Self {
      mmap,
      backing: ShmemBacking::Temp(tmp),
      path,
    })
  }

  fn open_mapped(path: &Path, len: u64) -> Result<Self, IpcError> {
    let len_usize = usize::try_from(len).map_err(|_| IpcError::ArithmeticOverflow)?;

    let file = OpenOptions::new().read(true).write(true).open(path)?;

    // SAFETY: The mapping length is checked against the expected descriptor length immediately
    // after mapping. The file handle is kept alive to reduce platform-specific surprises.
    let mmap = unsafe { MmapMut::map_mut(&file)? };
    if mmap.len() != len_usize {
      return Err(protocol_violation(format!(
        "shared memory size mismatch for {}: expected {len_usize} bytes, got {} bytes",
        path.display(),
        mmap.len()
      )));
    }

    Ok(Self {
      mmap,
      backing: ShmemBacking::File(file),
      path: path.to_path_buf(),
    })
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.mmap
  }

  pub fn as_bytes_mut(&mut self) -> &mut [u8] {
    &mut self.mmap
  }

  pub fn path(&self) -> &Path {
    &self.path
  }
}

/// Browser-side pool: owns and keeps a set of shared-memory frame buffers mapped.
#[derive(Debug)]
pub struct BrowserFramePool {
  pub generation: u64,
  buffers: Vec<ShmemRegion>,
  descs: Vec<FrameBufferDesc>,
}

impl BrowserFramePool {
  pub fn new_for_viewport(
    count: usize,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Result<Self, IpcError> {
    if count == 0 {
      return Err(invalid_parameters(
        "frame pool buffer count must be > 0".to_string(),
      ));
    }
    if count > MAX_FRAME_BUFFERS {
      return Err(IpcError::TooManyFrameBuffers {
        len: count,
        max: MAX_FRAME_BUFFERS,
      });
    }

    let (width_px, height_px) = viewport_to_device_px(viewport_css, dpr)?;
    let layout = frame_buffer_layout(width_px, height_px)?;

    let mut buffers = Vec::with_capacity(count);
    let mut descs = Vec::with_capacity(count);

    for index in 0..count {
      let region = ShmemRegion::create(layout.byte_len)?;
      let desc = FrameBufferDesc {
        index: index as u32,
        width_px: layout.width_px,
        height_px: layout.height_px,
        stride_bytes: layout.stride_bytes,
        byte_len: layout.byte_len,
        path: region.path().to_path_buf(),
      };
      buffers.push(region);
      descs.push(desc);
    }

    Ok(Self {
      generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
      buffers,
      descs,
    })
  }

  pub fn descriptors(&self) -> &[FrameBufferDesc] {
    &self.descs
  }

  pub fn buffer_bytes(&self, index: u32) -> Option<&[u8]> {
    // The browser must not speculatively read from the shared memory buffer before it has observed
    // the corresponding `FrameReady` message. Pair with the renderer-side Release fence in
    // `RendererFramePool::mark_sent`.
    sync::shm_consume_frame();
    let idx = usize::try_from(index).ok()?;
    self.buffers.get(idx).map(ShmemRegion::as_bytes)
  }
}

/// Renderer-side pool: maps shared buffers and tracks free/in-flight indices to prevent overwrites.
#[derive(Debug)]
pub struct RendererFramePool {
  pub generation: u64,
  buffers: Vec<ShmemRegion>,
  descs: Vec<FrameBufferDesc>,
  free: VecDeque<u32>,
  writing: HashSet<u32>,
  in_use: HashSet<u32>,
}

impl RendererFramePool {
  pub fn install(generation: u64, descs: Vec<FrameBufferDesc>) -> Result<Self, IpcError> {
    if descs.is_empty() {
      return Err(invalid_parameters(
        "frame pool descriptors must be non-empty".to_string(),
      ));
    }
    if descs.len() > MAX_FRAME_BUFFERS {
      return Err(IpcError::TooManyFrameBuffers {
        len: descs.len(),
        max: MAX_FRAME_BUFFERS,
      });
    }

    // Validate descriptors are contiguous and consistent. This keeps index handling simple and
    // ensures protocol violations are caught eagerly.
    for (pos, desc) in descs.iter().enumerate() {
      let expected = pos as u32;
      if desc.index != expected {
        return Err(protocol_violation(format!(
          "frame buffer descriptor index mismatch at position {pos}: expected {expected}, got {}",
          desc.index
        )));
      }
      let layout = desc.layout()?;
      if layout.byte_len != desc.byte_len {
        return Err(protocol_violation(format!(
          "frame buffer descriptor {} byte_len mismatch: expected {}, got {}",
          desc.index, layout.byte_len, desc.byte_len
        )));
      }
    }

    // Also ensure all descriptors share the same layout (single viewport).
    let first_layout = descs[0].layout()?;
    for desc in &descs[1..] {
      if desc.width_px != first_layout.width_px
        || desc.height_px != first_layout.height_px
        || desc.stride_bytes != first_layout.stride_bytes
        || desc.byte_len != first_layout.byte_len
      {
        return Err(protocol_violation(
          "frame buffer descriptors have inconsistent layouts".to_string(),
        ));
      }
    }

    let mut buffers = Vec::with_capacity(descs.len());
    for desc in &descs {
      buffers.push(ShmemRegion::open_mapped(&desc.path, desc.byte_len)?);
    }

    let mut free = VecDeque::with_capacity(descs.len());
    for idx in 0..descs.len() {
      free.push_back(idx as u32);
    }

    Ok(Self {
      generation,
      buffers,
      descs,
      free,
      writing: HashSet::new(),
      in_use: HashSet::new(),
    })
  }

  pub fn acquire(&mut self) -> Option<(u32, &mut [u8], FrameBufferLayout)> {
    let idx = self.free.pop_front()?;

    // Maintain internal invariants (never hand out the same buffer twice).
    debug_assert!(!self.writing.contains(&idx));
    debug_assert!(!self.in_use.contains(&idx));
    self.writing.insert(idx);

    let layout = self.descs.get(idx as usize).and_then(|d| d.layout().ok())?;
    let bytes = self.buffers.get_mut(idx as usize)?.as_bytes_mut();
    Some((idx, bytes, layout))
  }

  pub fn mark_sent(&mut self, idx: u32) -> Result<(), IpcError> {
    self.ensure_index(idx)?;

    if !self.writing.remove(&idx) {
      return Err(protocol_violation(format!(
        "mark_sent for buffer {idx} that is not currently acquired for writing"
      )));
    }

    // Publish pixel writes performed by the renderer before it sends `FrameReady` for this buffer.
    //
    // The pixels live in shared memory, but the readiness signal is delivered via a separate IPC
    // channel. A Release fence here (paired with an Acquire fence in
    // `BrowserFramePool::buffer_bytes`) prevents store/load reordering across the message boundary,
    // ensuring that once the browser receives `FrameReady`, it sees a fully-written frame.
    sync::shm_publish_frame();

    if !self.in_use.insert(idx) {
      return Err(protocol_violation(format!(
        "mark_sent for buffer {idx} that is already in_use"
      )));
    }
    Ok(())
  }

  pub fn release(&mut self, idx: u32) -> Result<(), IpcError> {
    self.ensure_index(idx)?;

    if !self.in_use.remove(&idx) {
      return Err(protocol_violation(format!(
        "release for buffer {idx} that is not currently in_use"
      )));
    }
    if self.writing.contains(&idx) {
      return Err(protocol_violation(format!(
        "release for buffer {idx} that is still marked as writing"
      )));
    }

    self.free.push_back(idx);
    Ok(())
  }

  fn ensure_index(&self, idx: u32) -> Result<(), IpcError> {
    let idx_usize = usize::try_from(idx).map_err(|_| IpcError::ArithmeticOverflow)?;
    if idx_usize >= self.buffers.len() {
      return Err(IpcError::InvalidBufferIndex {
        buffer_index: idx,
        buffer_count: self.buffers.len(),
      });
    }
    Ok(())
  }
}

fn viewport_to_device_px(viewport_css: (u32, u32), dpr: f32) -> Result<(u32, u32), IpcError> {
  let dpr = if dpr.is_finite() && dpr > 0.0 {
    dpr
  } else {
    1.0
  };
  let w_css = viewport_css.0.max(1) as f64;
  let h_css = viewport_css.1.max(1) as f64;

  let w_f = (w_css * (dpr as f64)).round();
  let h_f = (h_css * (dpr as f64)).round();

  if !w_f.is_finite() || !h_f.is_finite() {
    return Err(invalid_parameters("viewport size is not finite".to_string()));
  }

  let w = u32::try_from(w_f as u64).map_err(|_| invalid_parameters(format!("viewport width out of range: {w_f}")))?;
  let h = u32::try_from(h_f as u64).map_err(|_| invalid_parameters(format!("viewport height out of range: {h_f}")))?;

  Ok((w.max(1), h.max(1)))
}

fn frame_buffer_layout(width_px: u32, height_px: u32) -> Result<FrameBufferLayout, IpcError> {
  if width_px == 0 || height_px == 0 {
    return Err(IpcError::FrameDimensionsZero {
      width_px,
      height_px,
    });
  }

  let stride_bytes = width_px
    .checked_mul(BYTES_PER_PIXEL as u32)
    .ok_or(IpcError::ArithmeticOverflow)?;

  let byte_len = (stride_bytes as u64)
    .checked_mul(height_px as u64)
    .ok_or(IpcError::ArithmeticOverflow)?;

  if byte_len > MAX_PIXMAP_BYTES {
    return Err(IpcError::FrameTooLarge {
      len: usize::try_from(byte_len).map_err(|_| IpcError::ArithmeticOverflow)?,
      max: usize::try_from(MAX_PIXMAP_BYTES).map_err(|_| IpcError::ArithmeticOverflow)?,
    });
  }

  Ok(FrameBufferLayout {
    width_px,
    height_px,
    stride_bytes,
    byte_len,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn acquiring_and_releasing_reuses_buffers() {
    let browser = BrowserFramePool::new_for_viewport(2, (32, 16), 1.0).unwrap();
    let mut renderer =
      RendererFramePool::install(browser.generation, browser.descriptors().to_vec()).unwrap();

    let (idx0, buf0, _) = renderer.acquire().unwrap();
    buf0[0] = 1;
    renderer.mark_sent(idx0).unwrap();

    let (idx1, buf1, _) = renderer.acquire().unwrap();
    buf1[0] = 2;
    renderer.mark_sent(idx1).unwrap();

    assert_ne!(idx0, idx1);

    renderer.release(idx0).unwrap();

    let (idx2, _, _) = renderer.acquire().unwrap();
    assert_eq!(idx2, idx0);
  }

  #[test]
  fn renderer_cannot_acquire_when_all_buffers_in_use() {
    let browser = BrowserFramePool::new_for_viewport(2, (32, 16), 1.0).unwrap();
    let mut renderer =
      RendererFramePool::install(browser.generation, browser.descriptors().to_vec()).unwrap();

    let (idx0, _, _) = renderer.acquire().unwrap();
    renderer.mark_sent(idx0).unwrap();

    let (idx1, _, _) = renderer.acquire().unwrap();
    renderer.mark_sent(idx1).unwrap();

    assert!(renderer.acquire().is_none());
  }

  #[test]
  fn shm_fences_invoked_on_publish_and_consume() {
    let publish_before = crate::ipc::sync::shm_publish_count_for_test();
    let consume_before = crate::ipc::sync::shm_consume_count_for_test();

    let browser = BrowserFramePool::new_for_viewport(1, (32, 16), 1.0).unwrap();
    let mut renderer =
      RendererFramePool::install(browser.generation, browser.descriptors().to_vec()).unwrap();

    let (idx, buf, _) = renderer.acquire().unwrap();
    buf[0] = 0xAB;
    renderer.mark_sent(idx).unwrap();
    assert!(
      crate::ipc::sync::shm_publish_count_for_test() > publish_before,
      "expected renderer publish fence to run when marking buffer sent"
    );

    let bytes = browser.buffer_bytes(idx).unwrap();
    assert_eq!(bytes[0], 0xAB);
    assert!(
      crate::ipc::sync::shm_consume_count_for_test() > consume_before,
      "expected browser consume fence to run when reading buffer bytes"
    );
  }

  #[test]
  fn shmem_region_create_zero_initializes() {
    let region = ShmemRegion::create(128).expect("create shmem region");
    assert!(
      region.as_bytes().iter().all(|b| *b == 0),
      "newly created shared-memory region should be zero-initialized"
    );
  }
}
