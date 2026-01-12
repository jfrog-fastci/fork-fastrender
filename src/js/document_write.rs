use crate::js::JsExecutionOptions;
use std::cell::RefCell;

/// Warning emitted when `document.write()` / `document.writeln()` is invoked when parsing is no
/// longer active (e.g. after streaming parsing completes).
///
/// FastRender intentionally treats such calls as deterministic no-ops (instead of implicitly
/// calling `document.open()` and rewriting the document), but emits this warning so callers/tests
/// can detect the ignored write.
pub const DOCUMENT_WRITE_IGNORED_NO_PARSER_WARNING: &str =
  "Ignored document.write()/document.writeln() because no streaming parser is active";

/// Host-managed state for `document.write()` / `document.writeln()`.
///
/// This tracks:
/// - whether a streaming HTML parser is currently active,
/// - a pending buffer of injected markup (to be pushed into the parser input stream),
/// - and per-navigation budgets (bytes + calls).
#[derive(Debug, Clone)]
pub struct DocumentWriteState {
  parsing_active: bool,
  pending_html: String,
  bytes_written_total: usize,
  write_calls: usize,

  max_bytes_per_call: usize,
  max_bytes_total: usize,
  max_calls: usize,

  warned_ignored_write: bool,
}

impl Default for DocumentWriteState {
  fn default() -> Self {
    let opts = JsExecutionOptions::default();
    Self {
      parsing_active: false,
      pending_html: String::new(),
      bytes_written_total: 0,
      write_calls: 0,
      max_bytes_per_call: opts.max_document_write_bytes_per_call,
      max_bytes_total: opts.max_document_write_bytes_total,
      max_calls: opts.max_document_write_calls,
      warned_ignored_write: false,
    }
  }
}

impl DocumentWriteState {
  pub fn reset_for_navigation(&mut self) {
    self.parsing_active = false;
    self.pending_html.clear();
    self.bytes_written_total = 0;
    self.write_calls = 0;
    self.warned_ignored_write = false;
  }

  pub fn update_limits(&mut self, options: JsExecutionOptions) {
    self.max_bytes_per_call = options.max_document_write_bytes_per_call;
    self.max_bytes_total = options.max_document_write_bytes_total;
    self.max_calls = options.max_document_write_calls;
  }

  pub fn set_parsing_active(&mut self, active: bool) {
    self.parsing_active = active;
    if !active {
      self.pending_html.clear();
    }
  }

  pub fn parsing_active(&self) -> bool {
    self.parsing_active
  }

  pub fn max_bytes_per_call(&self) -> usize {
    self.max_bytes_per_call
  }

  pub fn max_bytes_total(&self) -> usize {
    self.max_bytes_total
  }

  pub fn max_calls(&self) -> usize {
    self.max_calls
  }

  pub fn bytes_written_total(&self) -> usize {
    self.bytes_written_total
  }

  pub fn write_calls(&self) -> usize {
    self.write_calls
  }

  /// Record a `document.write(...)` / `document.writeln(...)` call against the per-navigation
  /// budgets and, when `enqueue` is true, append the markup to the pending buffer.
  ///
  /// Note: budgets are enforced regardless of whether a streaming parser is active. This keeps
  /// post-parse no-op `document.write` calls deterministic and bounded.
  pub fn try_write(&mut self, input: &str, enqueue: bool) -> Result<(), DocumentWriteLimitError> {
    if self.write_calls >= self.max_calls {
      return Err(DocumentWriteLimitError::TooManyCalls {
        limit: self.max_calls,
      });
    }
    let len = input.len();
    if len > self.max_bytes_per_call {
      return Err(DocumentWriteLimitError::PerCallBytesExceeded {
        len,
        limit: self.max_bytes_per_call,
      });
    }
    if self.bytes_written_total.saturating_add(len) > self.max_bytes_total {
      return Err(DocumentWriteLimitError::TotalBytesExceeded {
        current: self.bytes_written_total,
        add: len,
        limit: self.max_bytes_total,
      });
    }

    self.write_calls = self.write_calls.saturating_add(1);
    self.bytes_written_total = self.bytes_written_total.saturating_add(len);
    if enqueue && self.parsing_active {
      self.pending_html.push_str(input);
    }
    Ok(())
  }

  pub fn take_pending_html(&mut self) -> String {
    std::mem::take(&mut self.pending_html)
  }

  /// Mark that a `document.write` / `document.writeln` call was ignored because parsing is no
  /// longer active.
  ///
  /// Returns `true` exactly once per navigation so callers can avoid spamming diagnostics.
  pub fn note_ignored_write(&mut self) -> bool {
    if self.warned_ignored_write {
      return false;
    }
    self.warned_ignored_write = true;
    true
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentWriteLimitError {
  TooManyCalls { limit: usize },
  PerCallBytesExceeded { len: usize, limit: usize },
  TotalBytesExceeded { current: usize, add: usize, limit: usize },
}

impl DocumentWriteLimitError {
  pub fn range_error_message(&self) -> String {
    match *self {
      DocumentWriteLimitError::TooManyCalls { limit } => {
        format!("document.write exceeded max call count (limit={limit})")
      }
      DocumentWriteLimitError::PerCallBytesExceeded { len, limit } => {
        format!("document.write exceeded max bytes per call (len={len}, limit={limit})")
      }
      DocumentWriteLimitError::TotalBytesExceeded { current, add, limit } => format!(
        "document.write exceeded max cumulative bytes (current={current}, add={add}, limit={limit})"
      ),
    }
  }
}

thread_local! {
  static DOCUMENT_WRITE_STACK: RefCell<Vec<*mut DocumentWriteState>> = RefCell::new(Vec::new());
}

struct DocumentWriteStackGuard {
  expected_ptr: *mut DocumentWriteState,
}

impl Drop for DocumentWriteStackGuard {
  fn drop(&mut self) {
    DOCUMENT_WRITE_STACK.with(|stack| {
      let popped = stack.borrow_mut().pop();
      debug_assert!(popped.is_some(), "document write stack underflow");
      if let Some(popped) = popped {
        debug_assert_eq!(
          popped, self.expected_ptr,
          "document write stack corruption (expected different pointer)"
        );
      }
    });
  }
}

unsafe fn push_document_write_ptr(ptr: *mut DocumentWriteState) -> DocumentWriteStackGuard {
  DOCUMENT_WRITE_STACK.with(|stack| stack.borrow_mut().push(ptr));
  DocumentWriteStackGuard { expected_ptr: ptr }
}

struct DocumentWriteSwapGuard<'a> {
  slot: &'a mut DocumentWriteState,
  owned: DocumentWriteState,
}

impl<'a> DocumentWriteSwapGuard<'a> {
  fn new(slot: &'a mut DocumentWriteState) -> Self {
    let owned = std::mem::take(slot);
    Self { slot, owned }
  }

  fn owned_ptr(&mut self) -> *mut DocumentWriteState {
    &mut self.owned as *mut DocumentWriteState
  }
}

impl Drop for DocumentWriteSwapGuard<'_> {
  fn drop(&mut self) {
    *self.slot = std::mem::take(&mut self.owned);
  }
}

/// Runs `f` with `state` installed as the current JS-visible `DocumentWriteState`.
pub fn with_document_write_state<R>(state: &mut DocumentWriteState, f: impl FnOnce() -> R) -> R {
  let mut swap = DocumentWriteSwapGuard::new(state);
  // SAFETY: `swap` owns the moved-out state for the duration of this call.
  let _guard = unsafe { push_document_write_ptr(swap.owned_ptr()) };
  f()
}

pub(crate) fn current_document_write_state_mut() -> Option<&'static mut DocumentWriteState> {
  let ptr = DOCUMENT_WRITE_STACK.with(|stack| stack.borrow().last().copied())?;
  // SAFETY: `with_document_write_state` installs a valid pointer for the duration of the call.
  Some(unsafe { &mut *ptr })
}
