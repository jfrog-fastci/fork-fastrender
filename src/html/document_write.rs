//! Minimal `document.write()` integration for the streaming HTML parser.
//!
//! The HTML Standard defines `Document.write()` as interacting with the parser input stream when
//! parsing is in progress ("re-entrant parsing"). FastRender's `StreamingHtmlParser` already
//! supports `document.write`-style injection via `push_front_str`, but JS bindings need a way to
//! reach the currently active parser.
//!
//! This module provides a thread-local stack of active `StreamingHtmlParser` pointers:
//! - `BrowserTab` installs the current parser while executing parser-blocking scripts.
//! - JS bindings (currently `WindowRealm`'s `document.write`) consult the TLS slot and inject
//!   bytes into the parser's buffered input.
//!
//! FastRender currently implements a deterministic subset of HTML's
//! "ignore-destructive-writes counter": when no streaming parser is active, `document.write()`
//! performs no-op instead of implicitly calling `document.open()` and clearing the document.
//! This avoids destructive, non-deterministic post-load writes.

use crate::html::streaming_parser::StreamingHtmlParser;

use std::cell::RefCell;

thread_local! {
  /// Stack of pointers to the currently active streaming HTML parser.
  ///
  /// This is a stack (not a single slot) so nested script execution / re-entrant parsing can
  /// restore the previous parser on unwind.
  static STREAMING_PARSER_STACK: RefCell<Vec<*const StreamingHtmlParser>> = RefCell::new(Vec::new());
}

struct StreamingParserStackGuard {
  expected_ptr: *const StreamingHtmlParser,
}

impl Drop for StreamingParserStackGuard {
  fn drop(&mut self) {
    STREAMING_PARSER_STACK.with(|stack| {
      let popped = stack.borrow_mut().pop();
      debug_assert!(popped.is_some(), "streaming parser stack underflow");
      if let Some(popped) = popped {
        debug_assert_eq!(
          popped, self.expected_ptr,
          "streaming parser stack corruption (expected different pointer)"
        );
      }
    });
  }
}

/// Runs `f` with `parser` installed as the current JS-visible streaming parser.
pub(crate) fn with_active_streaming_parser<R>(
  parser: &StreamingHtmlParser,
  f: impl FnOnce() -> R,
) -> R {
  with_active_streaming_parser_ptr(parser as *const StreamingHtmlParser, f)
}

/// Runs `f` with the provided streaming parser pointer installed as the current JS-visible parser.
///
/// This is equivalent to [`with_active_streaming_parser`] but accepts a raw pointer so callers can
/// install a parser from behind interior mutability (e.g. `Rc<RefCell<...>>`) without holding a
/// borrow guard across JS execution.
pub(crate) fn with_active_streaming_parser_ptr<R>(
  parser: *const StreamingHtmlParser,
  f: impl FnOnce() -> R,
) -> R {
  STREAMING_PARSER_STACK.with(|stack| stack.borrow_mut().push(parser));
  let _guard = StreamingParserStackGuard {
    expected_ptr: parser,
  };
  f()
}

/// Returns the currently installed streaming parser pointer, if any.
pub(crate) fn current_streaming_parser() -> Option<&'static StreamingHtmlParser> {
  let ptr = STREAMING_PARSER_STACK.with(|stack| stack.borrow().last().copied());
  let ptr = ptr?;
  // SAFETY: `with_active_streaming_parser` installs a valid pointer for the duration of a host
  // call into JS. The pointer is only used during that dynamic extent.
  Some(unsafe { &*ptr })
}
