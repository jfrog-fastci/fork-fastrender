use vm_js::{Heap, HeapLimits, SourceTextModuleRecord, VmError};

fn parse(src: &str) -> Result<SourceTextModuleRecord, VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  SourceTextModuleRecord::parse(&mut heap, src)
}

fn assert_syntax(src: &str) {
  let err = parse(src).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)), "expected VmError::Syntax, got {err:?}");
}

#[test]
fn invalid_import_named_alias_object_pattern_is_syntax_error() {
  assert_syntax("import { x as {y} } from 'm';");
}

#[test]
fn invalid_import_namespace_object_pattern_is_syntax_error() {
  assert_syntax("import * as {y} from 'm';");
}

#[test]
fn invalid_import_named_alias_assignment_pattern_is_syntax_error() {
  assert_syntax("import {x as y = 1} from 'm';");
}

#[test]
fn invalid_import_default_array_pattern_is_syntax_error() {
  assert_syntax("import [x] from 'm';");
}

