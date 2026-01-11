#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

#[test]
fn function_falling_off_end_inserts_explicit_return() {
  let src = r#"
    const f = () => { let x = 1; };
    f();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(program.functions.len(), 1);

  let cfg = &program.functions[0].body;
  let terminal_labels: Vec<u32> = cfg
    .graph
    .labels_sorted()
    .into_iter()
    .filter(|&label| cfg.graph.children_sorted(label).is_empty())
    .collect();
  assert!(
    !terminal_labels.is_empty(),
    "expected at least one terminal block in function CFG"
  );
  for label in &terminal_labels {
    let block = cfg.bblocks.get(*label);
    let Some(last) = block.last() else {
      panic!("terminal block {label} is empty; expected Return/Throw terminator");
    };
    assert!(
      matches!(last.t, InstTyp::Return | InstTyp::Throw),
      "terminal block {label} should end in Return/Throw, got last inst: {last:?}"
    );
  }

  let has_implicit_return = program.functions[0]
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| inst.t == InstTyp::Return && inst.as_return().is_none());

  assert!(
    has_implicit_return,
    "expected implicit function return to be lowered as Return() (implicit undefined)"
  );
}

#[test]
fn top_level_does_not_insert_implicit_return() {
  let program = compile_source("let x = 1; x;", TopLevelMode::Module, false);

  let has_return = program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| inst.t == InstTyp::Return);

  assert!(
    !has_return,
    "top-level bodies should not synthesize Return terminators"
  );
}
