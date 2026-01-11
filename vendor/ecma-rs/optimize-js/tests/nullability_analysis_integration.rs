#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::nullability::{calculate_nullability, NullabilityMask};
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
  let mask = analysis.mask_of_var_before_inst(cfg, label, inst_idx, obj_var);
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
  let mask = analysis.mask_of_var_before_inst(cfg, label, inst_idx, obj_var);
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

#[test]
fn loose_not_nullish_check_refines_then_path_to_non_nullish() {
  let src = r#"
    let x = foo;
    if (x != null) {
      x.toString();
    } else {
      bar();
    }
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let analysis = calculate_nullability(cfg);

  let (label, inst_idx, obj_var) = find_to_string_getprop(cfg);
  let mask = analysis.mask_of_var_before_inst(cfg, label, inst_idx, obj_var);
  assert!(
    mask.is_non_nullish(),
    "expected receiver to be refined to non-nullish before x.toString(), got {mask:?}"
  );
}

#[test]
fn not_of_loose_nullish_check_is_handled() {
  let src = r#"
    let x = foo;
    if (!(x == null)) {
      x.toString();
    } else {
      bar();
    }
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let analysis = calculate_nullability(cfg);

  let (label, inst_idx, obj_var) = find_to_string_getprop(cfg);
  let mask = analysis.mask_of_var_before_inst(cfg, label, inst_idx, obj_var);
  assert!(
    mask.is_non_nullish(),
    "expected receiver to be refined to non-nullish before x.toString(), got {mask:?}"
  );
}
