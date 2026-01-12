#![cfg(feature = "typed")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source_typed;
use optimize_js::il::inst::{Arg, Const};
use optimize_js::TopLevelMode;

fn cfg_contains_string(cfg: &optimize_js::cfg::cfg::Cfg, needle: &str) -> bool {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .flat_map(|inst| inst.args.iter())
    .any(|arg| matches!(arg, Arg::Const(Const::Str(s)) if s == needle))
}

#[test]
fn assert_narrows_nullability_and_prunes_unreachable_branch() {
  let program = compile_source_typed(
    r#"
      function f(x: string | null) {
        assert(x !== null);
        if (x === null) {
          console.log("null-branch");
        }
        console.log(x.toString());
      }
    "#,
    TopLevelMode::Module,
    false,
  );

  assert_eq!(
    program.functions.len(),
    1,
    "expected a single nested function in test program"
  );
  let cfg = program.functions[0].analyzed_cfg();
  assert!(
    !cfg_contains_string(cfg, "null-branch"),
    "expected `if (x === null)` branch to be pruned after assert(x !== null)"
  );
}

