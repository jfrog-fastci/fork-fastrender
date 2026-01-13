use crate::heap::ExternalMemoryToken;
use crate::fallible_alloc::arc_try_new_vm;
use crate::fallible_format::MAX_ERROR_MESSAGE_BYTES;
use crate::{Heap, VmError};
use core::mem;
use std::fmt::Display;
use std::sync::Arc;

/// Source text for scripts/modules with precomputed line starts.
#[derive(Debug, Clone)]
pub struct SourceText {
  pub name: Arc<str>,
  pub text: Arc<str>,
  line_starts: Vec<u32>,
  line_start_stride: u32,
  #[allow(dead_code)]
  external_memory: Option<Arc<ExternalMemoryToken>>,
}

impl SourceText {
  /// Store at most this many line-start checkpoints.
  ///
  /// `SourceText::new` is infallible and uncharged, so it must avoid unbounded allocations. We cap
  /// the number of stored entries to ensure hostile input (e.g. `eval` with millions of newlines)
  /// cannot force the host to abort due to allocator OOM.
  const MAX_LINE_STARTS: usize = 4096;

  /// Construct a `SourceText` without charging it against [`crate::HeapLimits`].
  ///
  /// This constructor is restricted to `pub(crate)` so that embeddings cannot accidentally bypass
  /// heap limits by storing attacker-controlled scripts/modules without charging their backing
  /// text.
  pub(crate) fn new(name: impl Into<Arc<str>>, text: impl Into<Arc<str>>) -> Self {
    let name = name.into();
    let text = text.into();
    let bytes = text.as_bytes();
    let newline_count = bytes.iter().filter(|&&b| b == b'\n').count();
    let line_count = newline_count.saturating_add(1);

    // When possible, store *dense* line starts to avoid per-call source scans in `line_col`. This
    // is especially important for huge single-line sources, where scanning the entire source for
    // every stack frame would otherwise allow O(n²) behavior.
    if line_count <= Self::MAX_LINE_STARTS {
      let mut line_starts: Vec<u32> = Vec::new();
      if line_starts.try_reserve_exact(line_count).is_err() {
        // If we can't even allocate a small bounded line-start table, fall back to a minimal table
        // that still keeps `line_col` correct (but potentially slower on multi-line sources).
        line_starts.push(0u32);
        let stride = if newline_count == 0 { 1 } else { 2 };
        return Self {
          name,
          text,
          line_starts,
          line_start_stride: stride,
          external_memory: None,
        };
      }

      line_starts.push(0u32);
      for (idx, b) in bytes.iter().enumerate() {
        if *b != b'\n' {
          continue;
        }
        let next = (idx + 1).min(text.len());
        if let Ok(next_u32) = u32::try_from(next) {
          line_starts.push(next_u32);
        }
      }

      return Self {
        name,
        text,
        line_starts,
        line_start_stride: 1,
        external_memory: None,
      };
    }

    // Otherwise, store sparse checkpoints (every Nth newline) with a hard cap on the total number
    // of stored entries, and compute exact `(line, col)` by scanning from the nearest checkpoint.
    // Choose `stride` so we never exceed `MAX_LINE_STARTS`.
    let stride = newline_count.div_ceil(Self::MAX_LINE_STARTS - 1).max(1);
    let stride_u32 = u32::try_from(stride).unwrap_or(u32::MAX);

    let mut line_starts: Vec<u32> = Vec::new();
    if line_starts.try_reserve_exact(Self::MAX_LINE_STARTS).is_err() {
      // Fall back to a minimal table; correctness comes from scanning in `line_col`.
      line_starts.push(0u32);
      return Self {
        name,
        text,
        line_starts,
        line_start_stride: 2,
        external_memory: None,
      };
    }
    line_starts.push(0u32);

    let mut newlines_seen: usize = 0;
    for (idx, b) in bytes.iter().enumerate() {
      if *b != b'\n' {
        continue;
      }
      newlines_seen += 1;
      if newlines_seen % stride != 0 {
        continue;
      }
      if line_starts.len() >= Self::MAX_LINE_STARTS {
        break;
      }
      let next = (idx + 1).min(text.len());
      if let Ok(next_u32) = u32::try_from(next) {
        line_starts.push(next_u32);
      }
    }

    Self {
      name,
      text,
      line_starts,
      line_start_stride: stride_u32,
      external_memory: None,
    }
  }

  pub fn new_charged(
    heap: &mut Heap,
    name: impl Into<Arc<str>>,
    text: impl Into<Arc<str>>,
  ) -> Result<Self, VmError> {
    let mut source = Self::new(name, text);
    let line_starts_bytes = source
      .line_starts
      .capacity()
      .saturating_mul(mem::size_of::<u32>());
    let bytes = source
      .name
      .len()
      .saturating_add(source.text.len())
      .saturating_add(line_starts_bytes);
    let token = heap.charge_external(bytes)?;
    source.external_memory = Some(arc_try_new_vm(token)?);
    Ok(source)
  }

  /// Returns a stable identity pointer for this source text.
  ///
  /// This is intended for internal caching tables keyed by source identity (e.g. function snippet
  /// caches). Charged sources use their external-memory token address so that clones of the same
  /// `SourceText` share the same identity.
  pub(crate) fn cache_key_ptr(&self) -> *const () {
    match &self.external_memory {
      Some(token) => Arc::as_ptr(token) as *const (),
      None => self as *const SourceText as *const (),
    }
  }

  /// Convert a UTF-8 byte offset into 1-based `(line, col)` numbers.
  ///
  /// Columns are reported as 1-based UTF-8 byte offsets from the start of the
  /// line. This is exact for ASCII sources and avoids scanning potentially huge
  /// single-line scripts during stack trace / diagnostic mapping; for non-ASCII
  /// text the reported columns are only an approximation of user-visible
  /// character columns.
  ///
  /// Offsets that fall outside the text are clamped; offsets that fall inside a
  /// UTF-8 sequence are clamped backwards to the nearest valid char boundary.
  pub fn line_col(&self, offset: u32) -> (u32, u32) {
    let mut offset = offset as usize;
    offset = offset.min(self.text.len());
    while offset > 0 && !self.text.is_char_boundary(offset) {
      offset -= 1;
    }

    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);
    let checkpoint_idx = match self.line_starts.binary_search(&offset_u32) {
      Ok(idx) => idx,
      Err(0) => 0,
      Err(idx) => idx - 1,
    };

    let scan_start = *self
      .line_starts
      .get(checkpoint_idx)
      .unwrap_or(&u32::try_from(self.text.len()).unwrap_or(u32::MAX)) as usize;
    let scan_start = scan_start.min(offset);

    let mut line = (checkpoint_idx as u32)
      .saturating_mul(self.line_start_stride)
      .saturating_add(1);
    let mut line_start = scan_start;

    // When line starts are stored sparsely, scan forward from the checkpoint to compute the exact
    // line start. When line starts are dense (`line_start_stride == 1`), the checkpoint is already
    // exact.
    if self.line_start_stride != 1 {
      for (i, b) in self.text.as_bytes()[scan_start..offset].iter().enumerate() {
        if *b == b'\n' {
          line = line.saturating_add(1);
          line_start = scan_start.saturating_add(i).saturating_add(1);
        }
      }
    }

    let col0 = u32::try_from(offset.saturating_sub(line_start)).unwrap_or(u32::MAX);
    (line, col0.saturating_add(1))
  }
}

/// A single stack frame for stack traces and termination errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
  pub function: Option<Arc<str>>,
  pub source: Arc<str>,
  pub line: u32,
  pub col: u32,
}

impl Display for StackFrame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.function {
      Some(function) => write!(
        f,
        "at {function} ({source}:{line}:{col})",
        function = function,
        source = self.source,
        line = self.line,
        col = self.col
      ),
      None => write!(
        f,
        "at {source}:{line}:{col}",
        source = self.source,
        line = self.line,
        col = self.col
      ),
    }
  }
}

/// Format stack frames into a stable stack trace string.
pub fn format_stack_trace(frames: &[StackFrame]) -> String {
  const OOM_PLACEHOLDER: &str = "<stack trace omitted: OOM>";

  #[inline]
  fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
      return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
      end -= 1;
    }
    &s[..end]
  }

  #[inline]
  fn try_push_str_limited(out: &mut String, s: &str, max_bytes: usize) -> Result<(), ()> {
    if out.len() >= max_bytes {
      return Ok(());
    }
    let remaining = max_bytes - out.len();
    let part = truncate_to_char_boundary(s, remaining);
    if part.is_empty() {
      return Ok(());
    }
    out.try_reserve(part.len()).map_err(|_| ())?;
    out.push_str(part);
    Ok(())
  }

  #[inline]
  fn try_push_char_limited(out: &mut String, ch: char, max_bytes: usize) -> Result<(), ()> {
    if out.len() >= max_bytes {
      return Ok(());
    }
    let mut buf = [0u8; 4];
    let encoded = ch.encode_utf8(&mut buf);
    let remaining = max_bytes - out.len();
    if encoded.len() > remaining {
      return Ok(());
    }
    out.try_reserve(encoded.len()).map_err(|_| ())?;
    out.push(ch);
    Ok(())
  }

  #[inline]
  fn try_push_u32_limited(out: &mut String, mut value: u32, max_bytes: usize) -> Result<(), ()> {
    if out.len() >= max_bytes {
      return Ok(());
    }

    // u32::MAX has 10 digits.
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if value == 0 {
      i -= 1;
      buf[i] = b'0';
    } else {
      while value != 0 && i != 0 {
        let digit = (value % 10) as u8;
        value /= 10;
        i -= 1;
        buf[i] = b'0' + digit;
      }
    }
    let s = std::str::from_utf8(&buf[i..]).unwrap_or("0");
    try_push_str_limited(out, s, max_bytes)
  }

  #[inline]
  fn try_format_frame(out: &mut String, frame: &StackFrame, max_bytes: usize) -> Result<(), ()> {
    try_push_str_limited(out, "at ", max_bytes)?;
    match &frame.function {
      Some(function) => {
        try_push_str_limited(out, function, max_bytes)?;
        try_push_str_limited(out, " (", max_bytes)?;
        try_push_str_limited(out, &frame.source, max_bytes)?;
        try_push_char_limited(out, ':', max_bytes)?;
        try_push_u32_limited(out, frame.line, max_bytes)?;
        try_push_char_limited(out, ':', max_bytes)?;
        try_push_u32_limited(out, frame.col, max_bytes)?;
        try_push_char_limited(out, ')', max_bytes)?;
      }
      None => {
        try_push_str_limited(out, &frame.source, max_bytes)?;
        try_push_char_limited(out, ':', max_bytes)?;
        try_push_u32_limited(out, frame.line, max_bytes)?;
        try_push_char_limited(out, ':', max_bytes)?;
        try_push_u32_limited(out, frame.col, max_bytes)?;
      }
    }
    Ok(())
  }

  #[inline]
  fn oom_placeholder() -> String {
    let mut out = String::new();
    if out.try_reserve_exact(OOM_PLACEHOLDER.len()).is_ok() {
      out.push_str(OOM_PLACEHOLDER);
    }
    out
  }

  if frames.is_empty() {
    return String::new();
  }

  let mut out = String::new();
  // Best-effort preallocation. If it fails, we still attempt incremental writes below.
  let estimate = frames
    .len()
    .saturating_mul(32)
    .min(MAX_ERROR_MESSAGE_BYTES);
  let _ = out.try_reserve_exact(estimate);

  for (i, frame) in frames.iter().enumerate() {
    if out.len() >= MAX_ERROR_MESSAGE_BYTES {
      break;
    }
    if i != 0 && try_push_char_limited(&mut out, '\n', MAX_ERROR_MESSAGE_BYTES).is_err() {
      // OOM while adding a separator: return what we've built so far.
      return if out.is_empty() { oom_placeholder() } else { out };
    }
    if try_format_frame(&mut out, frame, MAX_ERROR_MESSAGE_BYTES).is_err() {
      // OOM while formatting a frame: return a partial stack trace (or a placeholder if we failed
      // before producing anything).
      return if out.is_empty() { oom_placeholder() } else { out };
    }
  }

  out
}
