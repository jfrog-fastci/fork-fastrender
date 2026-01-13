use vm_js::{Heap, HeapLimits, SourceTextModuleRecord, VmError};

fn assert_syntax(src: &str) {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  match SourceTextModuleRecord::parse(&mut heap, src) {
    Err(VmError::Syntax(_)) => {}
    Err(VmError::Unimplemented(msg)) => panic!("expected VmError::Syntax, got Unimplemented({msg})"),
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn invalid_import_binding_patterns_are_syntax_errors() {
  assert_syntax("import { x as {y} } from 'm';");
  assert_syntax("import * as {y} from 'm';");
  assert_syntax("import {x as y = 1} from 'm';");
}

