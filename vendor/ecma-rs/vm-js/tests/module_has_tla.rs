use vm_js::{Heap, HeapLimits, SourceTextModuleRecord};

fn parse(src: &str) -> SourceTextModuleRecord {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  SourceTextModuleRecord::parse(&mut heap, src).expect("parse module")
}

#[test]
fn module_await_expression_sets_has_tla() {
  let record = parse("await 1; export {};");
  assert!(record.has_tla);
}

#[test]
fn await_inside_async_function_does_not_set_has_tla() {
  let record = parse("async function f(){ await 1; } export {};");
  assert!(!record.has_tla);
}

#[test]
fn for_await_of_sets_has_tla() {
  let record = parse("for await (const x of y) {}");
  assert!(record.has_tla);
}

#[test]
fn for_await_of_inside_function_does_not_set_has_tla() {
  let record = parse("async function g(){ for await (const x of y) {} } export {};");
  assert!(!record.has_tla);
}

#[test]
fn await_in_catch_param_sets_has_tla() {
  let record = parse("try { throw {}; } catch ({ x = await p }) {} export {};");
  assert!(record.has_tla);
}

#[test]
fn await_in_class_static_block_sets_has_tla() {
  let record = parse("class C { static { await 1; } } export {};");
  assert!(record.has_tla);
}

#[test]
fn for_await_of_in_class_static_block_sets_has_tla() {
  let record = parse("class C { static { for await (const x of y) {} } } export {};");
  assert!(record.has_tla);
}
