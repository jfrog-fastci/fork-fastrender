#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::nullability::{calculate_nullability, NullabilityMask, NullabilityResult};
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, BinOp, Const, InstTyp};
use optimize_js::TopLevelMode;

fn find_to_string_getprop(cfg: &Cfg) -> (u32, usize, u32) {
  let mut matches = Vec::new();
  for (label, block) in cfg.bblocks.all() {
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
      let Arg::Var(obj) = left else {
        panic!(
          "expected GetProp base for toString to be a variable, got {left:?} in {inst:?}"
        );
      };
      matches.push((label, inst_idx, *obj));
    }
  }

  assert_eq!(
    matches.len(),
    1,
    "expected exactly one GetProp(.toString) in the compiled CFG, got {matches:?}"
  );
  matches[0]
}

fn mask_before_inst(
  analysis: &NullabilityResult,
  cfg: &Cfg,
  label: u32,
  inst_idx: usize,
  var: u32,
) -> NullabilityMask {
  // The nullability API currently exposes block entry states, not per-instruction
  // states. For this test we require that the receiver variable isn't reassigned
  // between the block entry and the GetProp instruction, so the entry state is
  // the state immediately before the instruction.
  let block = cfg.bblocks.get(label);
  assert!(
    block
      .iter()
      .take(inst_idx)
      .all(|inst| !inst.tgts.iter().any(|tgt| *tgt == var)),
    "receiver var %{var} is assigned before the GetProp instruction; test needs per-instruction state support"
  );

  assert!(
    analysis.entry_state(label).is_reachable(),
    "expected GetProp block {label} to be reachable"
  );

  analysis.mask_of_var_at_entry(label, var)
}

#[test]
fn loose_nullish_check_refines_else_path_to_non_nullish() {
  let src = r#"
    let x = foo;
    if (x == null) {
      bar();
    } else {
      x.toString();
    }
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let analysis = calculate_nullability(cfg);

  let (label, inst_idx, obj_var) = find_to_string_getprop(cfg);
  let mask = mask_before_inst(&analysis, cfg, label, inst_idx, obj_var);
  assert!(
    mask.is_non_nullish(),
    "expected receiver to be refined to non-nullish before x.toString(), got {mask:?}"
  );
}

#[test]
fn strict_undefined_check_clears_maybe_undef_in_else_path() {
  let src = r#"
    let x = foo;
    if (x === undefined) {
      bar();
    } else {
      x.toString();
    }
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let analysis = calculate_nullability(cfg);

  let (label, inst_idx, obj_var) = find_to_string_getprop(cfg);
  let mask = mask_before_inst(&analysis, cfg, label, inst_idx, obj_var);
  assert!(
    !mask.may_be_undefined(),
    "expected receiver to be refined to not-undefined before x.toString(), got {mask:?}"
  );
  assert!(
    mask.may_be_null(),
    "expected receiver to still possibly be null after `x === undefined` check, got {mask:?}"
  );
  assert!(
    mask.contains(NullabilityMask::OTHER),
    "expected receiver to still include non-nullish values after `x === undefined` check, got {mask:?}"
  );
}
