#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::nullability::calculate_nullability;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp};
use optimize_js::TopLevelMode;

#[test]
fn truthiness_cond_goto_refines_to_non_nullish() {
  let src = r#"
    let x = foo;
    if (x) {
      x.toString();
    }
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let analysis = calculate_nullability(cfg);

  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);

  let mut saw_to_string_getprop = false;
  for (label, block) in blocks {
    for (inst_idx, inst) in block.iter().enumerate() {
      if inst.t != InstTyp::Bin || inst.bin_op != BinOp::GetProp {
        continue;
      }
      let (_tgt, left, _op, right) = inst.as_bin();
      let Arg::Const(Const::Str(prop)) = right else {
        continue;
      };
      if prop != "toString" {
        continue;
      }
      let Arg::Var(receiver) = left else {
        panic!("expected GetProp receiver to be a temp var, got {left:?}");
      };

      let receiver_nullability = analysis.mask_of_var_before_inst(cfg, label, inst_idx, *receiver);
      assert!(
        receiver_nullability.is_non_nullish(),
        "expected receiver to be NonNullish at GetProp, got {receiver_nullability:?}"
      );
      saw_to_string_getprop = true;
    }
  }

  assert!(
    saw_to_string_getprop,
    "failed to locate GetProp for `toString` in compiled CFG"
  );
}
