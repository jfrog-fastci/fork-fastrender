use parse_js::error::SyntaxErrorType;
use parse_js::{parse_with_options_cancellable, parse_with_options_cancellable_by, Dialect, ParseOptions, SourceType};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[test]
fn parse_with_options_cancellable_reports_cancelled_when_flag_is_set() {
  let cancel = Arc::new(AtomicBool::new(true));
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  let err = parse_with_options_cancellable("let x = 1;", opts, cancel).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::Cancelled);
}

#[test]
fn parse_with_options_cancellable_parses_successfully_when_not_cancelled() {
  let cancel = Arc::new(AtomicBool::new(false));
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  assert!(parse_with_options_cancellable("let x = 1;", opts, cancel).is_ok());
}

#[test]
fn parse_with_options_cancellable_by_reports_cancelled_when_callback_returns_true() {
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  let err = parse_with_options_cancellable_by("let x = 1;", opts, || true).unwrap_err();
  assert_eq!(err.typ, SyntaxErrorType::Cancelled);
}

#[test]
fn parse_with_options_cancellable_by_can_cancel_after_some_progress() {
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };

  // Use a long input so the parser is guaranteed to invoke the cancellation callback many times.
  let mut source = String::new();
  for _ in 0..100 {
    source.push_str("let x = 1; ");
  }

  let mut checks: usize = 0;
  let err = parse_with_options_cancellable_by(&source, opts, || {
    checks += 1;
    checks >= 10
  })
  .unwrap_err();

  assert_eq!(checks, 10);
  assert_eq!(err.typ, SyntaxErrorType::Cancelled);
}

#[test]
fn parse_with_options_cancellable_by_parses_successfully_when_not_cancelled() {
  let opts = ParseOptions {
    dialect: Dialect::Ts,
    source_type: SourceType::Module,
  };
  assert!(parse_with_options_cancellable_by("let x = 1;", opts, || false).is_ok());
}
