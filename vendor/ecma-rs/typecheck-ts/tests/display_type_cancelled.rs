use std::sync::Arc;

use typecheck_ts::{FatalError, FileKey, Host, HostError, Program, TypeId};

#[derive(Clone, Copy)]
struct CancelledHost;

impl Host for CancelledHost {
  fn file_text(&self, _file: &FileKey) -> Result<Arc<str>, HostError> {
    std::panic::panic_any(FatalError::Cancelled);
  }

  fn resolve(&self, _from: &FileKey, _specifier: &str) -> Option<FileKey> {
    None
  }
}

#[test]
fn display_type_does_not_propagate_cancelled_panics() {
  let entry = FileKey::new("index.ts");
  let program = Program::new(CancelledHost, vec![entry]);
  assert_eq!(program.display_type(TypeId(0)).to_string(), "unknown");
}

