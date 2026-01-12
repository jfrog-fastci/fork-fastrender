#[path = "common/mod.rs"]
mod common;

use common::compile_source;
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
fn assert_narrows_range_and_prunes_impossible_branch() {
  let program = compile_source(
    r#"
      function f(i) {
        assert(i >= 0);
        if (i < 0) {
          console.log("neg-branch");
        } else {
          console.log("ok-branch");
        }
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
    !cfg_contains_string(cfg, "neg-branch"),
    "expected `if (i < 0)` true branch to be pruned after assert(i >= 0)"
  );
  assert!(
    cfg_contains_string(cfg, "ok-branch"),
    "expected the remaining branch to stay reachable"
  );
}

