use crate::error::RenderError;
use std::collections::TryReserveError;
use tiny_skia::{IntSize, Pixmap};

const BYTES_PER_PIXEL: u64 = 4;
/// Upper bound on a single pixmap allocation to avoid process aborts on OOM.
pub(crate) const MAX_PIXMAP_BYTES: u64 = 512 * 1024 * 1024;

/// Copies RGBA8 bytes from a tightly packed [`Pixmap`] into an externally-provided RGBA buffer with
/// a caller-specified stride.
///
/// This is used by multiprocess/shared-memory frame transports that may require row padding.
pub(crate) fn copy_pixmap_rgba_into_strided_buffer(
  src: &Pixmap,
  dst: &mut [u8],
  dst_stride_bytes: usize,
) -> Result<(), RenderError> {
  let width = src.width() as usize;
  let height = src.height() as usize;
  let src_stride = width
    .checked_mul(4)
    .ok_or_else(|| RenderError::InvalidParameters {
      message: "pixmap copy overflow (src_stride)".to_string(),
    })?;
  if dst_stride_bytes < src_stride {
    return Err(RenderError::InvalidParameters {
      message: format!(
        "destination stride is too small for pixmap copy: stride={dst_stride_bytes}, need >= {src_stride}"
      ),
    });
  }
  let required = dst_stride_bytes
    .checked_mul(height)
    .ok_or_else(|| RenderError::InvalidParameters {
      message: "pixmap copy overflow (dst size)".to_string(),
    })?;
  if dst.len() < required {
    return Err(RenderError::InvalidParameters {
      message: format!(
        "destination buffer is too small for pixmap copy: need {required} bytes, got {}",
        dst.len()
      ),
    });
  }

  let src_data = src.data();
  for row in 0..height {
    let src_off = row * src_stride;
    let dst_off = row * dst_stride_bytes;
    dst[dst_off..dst_off + src_stride].copy_from_slice(&src_data[src_off..src_off + src_stride]);
    if dst_stride_bytes > src_stride {
      dst[dst_off + src_stride..dst_off + dst_stride_bytes].fill(0);
    }
  }
  Ok(())
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewPixmapAllocRecord {
  pub width: u32,
  pub height: u32,
  pub file: &'static str,
  pub line: u32,
}

#[cfg(test)]
thread_local! {
  static RECORD_NEW_PIXMAP: std::cell::Cell<bool> = std::cell::Cell::new(false);
  static NEW_PIXMAP_RECORDS: std::cell::RefCell<Vec<NewPixmapAllocRecord>> =
    std::cell::RefCell::new(Vec::new());
}

/// Records `new_pixmap` allocations for the current thread.
///
/// This is intended for unit tests that want to assert on temporary allocation patterns without
/// relying on global allocators or OS-level accounting.
#[cfg(test)]
pub(crate) struct NewPixmapAllocRecorder;

#[cfg(test)]
impl NewPixmapAllocRecorder {
  pub(crate) fn start() -> Self {
    RECORD_NEW_PIXMAP.with(|flag| flag.set(true));
    NEW_PIXMAP_RECORDS.with(|records| records.borrow_mut().clear());
    Self
  }

  pub(crate) fn take(&self) -> Vec<NewPixmapAllocRecord> {
    NEW_PIXMAP_RECORDS.with(|records| std::mem::take(&mut *records.borrow_mut()))
  }
}

#[cfg(test)]
impl Drop for NewPixmapAllocRecorder {
  fn drop(&mut self) {
    RECORD_NEW_PIXMAP.with(|flag| flag.set(false));
    NEW_PIXMAP_RECORDS.with(|records| records.borrow_mut().clear());
  }
}

pub(crate) fn guard_allocation_bytes(bytes: u64, context: &str) -> Result<usize, RenderError> {
  if bytes > MAX_PIXMAP_BYTES {
    return Err(RenderError::InvalidParameters {
      message: format!(
        "{context}: allocation would require {bytes} bytes (limit {MAX_PIXMAP_BYTES})"
      ),
    });
  }
  usize::try_from(bytes).map_err(|_| RenderError::InvalidParameters {
    message: format!("{context}: allocation size {bytes} does not fit in usize"),
  })
}

pub(crate) fn reserve_buffer(bytes: u64, context: &str) -> Result<Vec<u8>, RenderError> {
  let capacity = guard_allocation_bytes(bytes, context)?;
  crate::render_control::reserve_allocation(bytes, context)?;
  let mut buffer = Vec::new();
  buffer
    .try_reserve_exact(capacity)
    .map_err(|err: TryReserveError| RenderError::InvalidParameters {
      message: format!("{context}: buffer allocation failed for {bytes} bytes: {err}"),
    })?;
  Ok(buffer)
}

fn guard_dimensions(width: u32, height: u32, context: &str) -> Result<usize, RenderError> {
  if width == 0 || height == 0 {
    return Err(RenderError::InvalidParameters {
      message: format!("{context}: pixmap size is zero ({width}x{height})"),
    });
  }

  let pixels = (width as u64)
    .checked_mul(height as u64)
    .ok_or(RenderError::InvalidParameters {
      message: format!("{context}: pixmap dimensions overflow ({width}x{height})"),
    })?;
  let bytes = pixels
    .checked_mul(BYTES_PER_PIXEL)
    .ok_or(RenderError::InvalidParameters {
      message: format!("{context}: pixmap byte size overflow ({width}x{height})"),
    })?;
  if bytes > MAX_PIXMAP_BYTES {
    return Err(RenderError::InvalidParameters {
      message: format!(
        "{context}: pixmap {}x{} would allocate {} bytes (limit {})",
        width, height, bytes, MAX_PIXMAP_BYTES
      ),
    });
  }

  Ok(bytes as usize)
}

fn allocate_pixmap_bytes(bytes: usize) -> Result<Vec<u8>, RenderError> {
  let mut buffer = Vec::new();
  if let Err(err) = buffer.try_reserve_exact(bytes) {
    return Err(RenderError::InvalidParameters {
      message: format!("pixmap allocation failed: {err}"),
    });
  }
  buffer.resize(bytes, 0);
  Ok(buffer)
}

fn allocate_pixmap_bytes_uninitialized(bytes: usize) -> Result<Vec<u8>, RenderError> {
  let mut buffer = Vec::new();
  if let Err(err) = buffer.try_reserve_exact(bytes) {
    return Err(RenderError::InvalidParameters {
      message: format!("pixmap allocation failed: {err}"),
    });
  }
  // SAFETY: The caller must ensure every byte is written before any read occurs. This is used by
  // rasterizers that overwrite the entire pixmap before returning it (and may early-return with an
  // error, in which case the partially-initialized buffer is safely dropped without being read).
  unsafe {
    buffer.set_len(bytes);
  }
  Ok(buffer)
}

#[track_caller]
pub(crate) fn new_pixmap_with_context(
  width: u32,
  height: u32,
  context: &str,
) -> Result<Pixmap, RenderError> {
  let caller = std::panic::Location::caller();
  let context = format!("{context} (at {}:{})", caller.file(), caller.line());
  let bytes = guard_dimensions(width, height, &context)?;
  crate::render_control::reserve_allocation(bytes as u64, &context)?;
  let buffer = allocate_pixmap_bytes(bytes)?;
  let size = IntSize::from_wh(width, height).ok_or(RenderError::InvalidParameters {
    message: format!(
      "{context}: pixmap dimensions out of range ({}x{})",
      width, height
    ),
  })?;
  Pixmap::from_vec(buffer, size).ok_or(RenderError::InvalidParameters {
    message: format!(
      "{context}: pixmap creation failed for {}x{} ({} bytes)",
      width, height, bytes
    ),
  })
}

#[track_caller]
pub(crate) fn new_pixmap(width: u32, height: u32) -> Option<Pixmap> {
  #[cfg(test)]
  {
    if RECORD_NEW_PIXMAP.with(|flag| flag.get()) {
      let caller = std::panic::Location::caller();
      NEW_PIXMAP_RECORDS.with(|records| {
        records.borrow_mut().push(NewPixmapAllocRecord {
          width,
          height,
          file: caller.file(),
          line: caller.line(),
        });
      });
    }
  }
  new_pixmap_with_context(width, height, "pixmap").ok()
}

#[track_caller]
pub(crate) fn new_pixmap_uninitialized(width: u32, height: u32) -> Result<Pixmap, RenderError> {
  #[cfg(test)]
  {
    if RECORD_NEW_PIXMAP.with(|flag| flag.get()) {
      let caller = std::panic::Location::caller();
      NEW_PIXMAP_RECORDS.with(|records| {
        records.borrow_mut().push(NewPixmapAllocRecord {
          width,
          height,
          file: caller.file(),
          line: caller.line(),
        });
      });
    }
  }

  let caller = std::panic::Location::caller();
  let context = format!("pixmap (at {}:{})", caller.file(), caller.line());
  let bytes = guard_dimensions(width, height, &context)?;
  crate::render_control::reserve_allocation(bytes as u64, &context)?;
  let buffer = allocate_pixmap_bytes_uninitialized(bytes)?;
  let size = IntSize::from_wh(width, height).ok_or(RenderError::InvalidParameters {
    message: format!(
      "{context}: pixmap dimensions out of range ({}x{})",
      width, height
    ),
  })?;
  Pixmap::from_vec(buffer, size).ok_or(RenderError::InvalidParameters {
    message: format!(
      "{context}: pixmap creation failed for {}x{} ({} bytes)",
      width, height, bytes
    ),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::RenderStage;
  use crate::render_control::{StageAllocationBudget, StageAllocationBudgetGuard};
  use crate::render_control::{StageGuard, StageHeartbeat, StageHeartbeatGuard};
  use std::sync::Arc;

  #[test]
  fn rejects_zero_dimensions() {
    assert!(matches!(
      new_pixmap_with_context(0, 10, "zero"),
      Err(RenderError::InvalidParameters { .. })
    ));
    assert!(matches!(
      new_pixmap_with_context(10, 0, "zero"),
      Err(RenderError::InvalidParameters { .. })
    ));
  }

  #[test]
  fn rejects_overflow_and_limit() {
    assert!(matches!(
      new_pixmap_with_context(u32::MAX, 2, "overflow"),
      Err(RenderError::InvalidParameters { .. })
    ));

    let bytes_per_row = MAX_PIXMAP_BYTES / BYTES_PER_PIXEL + 1;
    let width = bytes_per_row as u32;
    assert!(matches!(
      new_pixmap_with_context(width, 1, "too_big"),
      Err(RenderError::InvalidParameters { .. })
    ));
  }

  #[test]
  fn allocates_small_pixmaps() {
    let pixmap = new_pixmap_with_context(4, 4, "ok").expect("small pixmap");
    assert_eq!(pixmap.width(), 4);
    assert_eq!(pixmap.height(), 4);
  }

  #[test]
  fn rejects_oversized_buffer_allocation() {
    assert!(reserve_buffer(MAX_PIXMAP_BYTES + 1, "buffer").is_err());
  }

  #[test]
  fn stage_allocation_budget_exceeded_returns_structured_error() {
    let budget = Arc::new(StageAllocationBudget::new(16));
    let _budget_guard = StageAllocationBudgetGuard::install(Some(&budget));
    let _stage_guard = StageGuard::install(crate::render_control::active_stage());
    let _heartbeat_guard =
      StageHeartbeatGuard::install(crate::render_control::active_stage_heartbeat());
    crate::render_control::record_stage(StageHeartbeat::PaintRasterize);

    let err = new_pixmap_with_context(4, 4, "budget").expect_err("expected budget error");
    let display = err.to_string();
    assert!(display.contains("allocation budget exceeded during paint_rasterize"));

    match err {
      RenderError::StageAllocationBudgetExceeded {
        stage,
        heartbeat,
        allocated_bytes,
        budget_bytes,
        context,
      } => {
        assert_eq!(stage, RenderStage::Paint);
        assert_eq!(heartbeat, StageHeartbeat::PaintRasterize);
        assert!(allocated_bytes > budget_bytes);
        assert!(!context.is_empty());
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }
}
