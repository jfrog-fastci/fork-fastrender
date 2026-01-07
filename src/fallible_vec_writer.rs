use std::io;
use std::io::Write;

/// A growable in-memory writer that fails gracefully when allocations fail.
///
/// The default `impl Write for Vec<u8>` uses infallible allocation and will abort the process on
/// OOM. Some encoders (notably the `image` crate's JPEG encoder) may write output one byte at a
/// time, so we also need to avoid O(n) realloc patterns while still bounding maximum output size.
pub(crate) struct FallibleVecWriter {
  buf: Vec<u8>,
  max_bytes: usize,
  context: &'static str,
}

impl FallibleVecWriter {
  pub(crate) fn new(max_bytes: usize, context: &'static str) -> Self {
    Self {
      buf: Vec::new(),
      max_bytes,
      context,
    }
  }

  pub(crate) fn into_inner(self) -> Vec<u8> {
    self.buf
  }
}

impl Write for FallibleVecWriter {
  fn write(&mut self, data: &[u8]) -> io::Result<usize> {
    let new_len = self
      .buf
      .len()
      .checked_add(data.len())
      .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("{}: output length overflow", self.context)))?;

    if new_len > self.max_bytes {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        format!(
          "{}: output exceeded {} bytes (attempted {})",
          self.context, self.max_bytes, new_len
        ),
      ));
    }

    // Grow our backing buffer with a bounded exponential strategy:
    // - avoids the pathological case where the upstream encoder writes 1 byte at a time, which
    //   would otherwise force O(n) reallocations
    // - never requests more than `max_bytes` total length.
    if new_len > self.buf.capacity() {
      let mut target_cap = self.buf.capacity().max(1);
      while target_cap < new_len {
        target_cap = target_cap.saturating_mul(2);
      }
      target_cap = target_cap.min(self.max_bytes);
      if target_cap < new_len {
        return Err(io::Error::new(
          io::ErrorKind::Other,
          format!(
            "{}: output exceeded {} bytes (attempted {})",
            self.context, self.max_bytes, new_len
          ),
        ));
      }

      let additional = target_cap.saturating_sub(self.buf.len());
      self.buf.try_reserve_exact(additional).map_err(|err| {
        io::Error::new(
          io::ErrorKind::Other,
          format!("{}: output buffer allocation failed: {err}", self.context),
        )
      })?;
    }

    self.buf.extend_from_slice(data);
    Ok(data.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

