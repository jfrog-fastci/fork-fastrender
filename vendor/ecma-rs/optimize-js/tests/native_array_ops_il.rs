#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Inst, InstTyp};
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  blocks
    .into_iter()
    .flat_map(|(_, block)| block.iter().cloned())
    .collect()
}

fn has_call_to(insts: &[Inst], callee: &str) -> bool {
  insts.iter().any(|inst| {
    if inst.t != InstTyp::Call {
      return false;
    }
    let (_, callee_arg, _, _, _) = inst.as_call();
    matches!(callee_arg, Arg::Builtin(path) if path == callee)
  })
}

#[cfg(all(feature = "semantic-ops", feature = "native-array-ops"))]
mod native_array_ops {
  use super::*;

  #[test]
  fn lowers_array_chain_to_structured_inst() {
    let program = compile_source(
      r#"
        arr["map"](x => x + 1)
          ["filter"](x => x > 0)
          ["reduce"]((acc, x) => acc + x, 0);
      "#,
      TopLevelMode::Module,
      false,
    );
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      insts.iter().any(|inst| inst.t == InstTyp::ArrayChain),
      "expected ArrayChain instruction but got {insts:#?}"
    );

    for builtin in [
      "Array.prototype.map",
      "Array.prototype.filter",
      "Array.prototype.reduce",
      "Array.prototype.find",
      "Array.prototype.every",
      "Array.prototype.some",
    ] {
      assert!(
        !has_call_to(&insts, builtin),
        "did not expect builtin {builtin} call when native-array-ops is enabled"
      );
    }
  }
}

#[cfg(all(feature = "semantic-ops", not(feature = "native-array-ops")))]
mod legacy_lowering {
  use super::*;

  #[test]
  fn still_lowers_array_chain_via_builtin_calls() {
    let program = compile_source(
      r#"
        arr["map"](x => x + 1)
          ["filter"](x => x > 0)
          ["reduce"]((acc, x) => acc + x, 0);
      "#,
      TopLevelMode::Module,
      false,
    );
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      has_call_to(&insts, "Array.prototype.map"),
      "expected builtin Array.prototype.map call when native-array-ops is disabled"
    );
    assert!(
      has_call_to(&insts, "Array.prototype.filter"),
      "expected builtin Array.prototype.filter call when native-array-ops is disabled"
    );
    assert!(
      has_call_to(&insts, "Array.prototype.reduce"),
      "expected builtin Array.prototype.reduce call when native-array-ops is disabled"
    );
  }
}

