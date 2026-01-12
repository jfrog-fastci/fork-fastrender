#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::il::inst::{InstTyp};
use optimize_js::TopLevelMode;

#[test]
fn assert_proven_true_is_removed() {
  let program = compile_source(
    r#"
      assert(true);
      console.log("alive");
    "#,
    TopLevelMode::Module,
    false,
  );

  let cfg = program.top_level.analyzed_cfg();
  let insts: Vec<_> = cfg
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .collect();

  assert!(
    !insts.iter().any(|inst| matches!(inst.t, InstTyp::Assume)),
    "expected no Assume instructions when assert(true) is removed"
  );

  assert!(
    !insts
      .iter()
      .any(|inst| matches!(inst.t, InstTyp::UnknownLoad) && inst.unknown == "assert"),
    "expected `assert(true)` to be removed entirely (no `UnknownLoad \"assert\"` remaining)"
  );
}
