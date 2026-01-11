#![cfg(feature = "typed")]

use optimize_js::analysis::nullability;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp};
use optimize_js::{CompileCfgOptions, TopLevelMode};

#[test]
fn typed_nullability_excludes_nullish_branch() {
  let src = r#"
    declare function get(): string;
    let s = get();
    if ((s as any) == null) { console.log("bad"); } else { console.log(s); }
  "#;

  let cfg_options = CompileCfgOptions {
    run_opt_passes: false,
    ..CompileCfgOptions::default()
  };
  let program = optimize_js::compile_source_typed_cfg_options(src, TopLevelMode::Module, false, cfg_options)
    .expect("compile typed source");

  let cfg = &program.top_level.body;
  let result = nullability::calculate_nullability(cfg);

  let mut match_edge: Option<(u32, u32)> = None;
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get(label);
    let Some(term) = block.last() else {
      continue;
    };
    if term.t != InstTyp::CondGoto {
      continue;
    }
    let (cond, then_label, _else_label) = term.as_cond_goto();
    let Arg::Var(cond_var) = cond else {
      continue;
    };

    let mut is_null_cmp = false;
    for inst in block.iter() {
      if inst.t != InstTyp::Bin {
        continue;
      }
      let (tgt, left, op, right) = inst.as_bin();
      if tgt != *cond_var {
        continue;
      }
      if op != BinOp::LooseEq {
        continue;
      }
      if matches!(left, Arg::Const(Const::Null)) || matches!(right, Arg::Const(Const::Null)) {
        is_null_cmp = true;
        break;
      }
    }

    if is_null_cmp {
      match_edge = Some((label, then_label));
      break;
    }
  }

  let Some((pred, then_label)) = match_edge else {
    panic!("missing CondGoto for `s == null`");
  };

  assert!(
    !result.edge_is_reachable(pred, then_label),
    "expected true edge of `s == null` to be unreachable"
  );
}
