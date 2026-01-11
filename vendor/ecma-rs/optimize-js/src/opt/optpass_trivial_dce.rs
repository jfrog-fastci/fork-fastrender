use crate::analysis::purity::Purity;
use crate::cfg::cfg::Cfg;
use crate::il::inst::Arg;
use crate::il::inst::InstTyp;
use crate::opt::PassResult;
use ahash::HashSet;
use ahash::HashSetExt;

pub fn optpass_trivial_dce(cfg: &mut Cfg) -> PassResult {
  let mut used = HashSet::new();
  for (_, bblock) in cfg.bblocks.all() {
    for inst in bblock.iter() {
      for arg in inst.args.iter() {
        let Arg::Var(t) = arg else {
          continue;
        };
        used.insert(*t);
      }
    }
  }
  let mut result = PassResult::default();
  for (_, bblock) in cfg.bblocks.all_mut() {
    for i in (0..bblock.len()).rev() {
      // We should delete if all targets are unused. (There should only ever be zero or one targets.)
      let should_delete =
        !bblock[i].tgts.is_empty() && bblock[i].tgts.iter().all(|var| !used.contains(var));
      if should_delete {
        if bblock[i].t == InstTyp::Call {
          // Calls are only removable when we know the callee has no observable effects.
          //
          // When purity metadata is present (via `analysis::purity::annotate_cfg_purity`), we can
          // eliminate unused pure calls. Otherwise stay conservative and only remove the unused
          // target.
          if matches!(
            bblock[i].meta.callee_purity,
            Purity::Pure | Purity::ReadOnly | Purity::Allocating
          ) {
            bblock.remove(i);
          } else {
            bblock[i].tgts.clear();
          }
        } else {
          bblock.remove(i);
        };
        result.mark_changed();
      };
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};
  use crate::il::inst::{Arg, Const, Inst};

  fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
    let mut graph = CfgGraph::default();
    // Ensure the entry label exists.
    graph.connect(0, 0);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, insts);
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn impure_call_with_unused_target_keeps_call_but_drops_target() {
    let mut cfg = cfg_single_block(vec![Inst::call(
      0,
      Arg::Builtin("__optimize_js_new".to_string()),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    )]);
    let result = optpass_trivial_dce(&mut cfg);
    assert!(result.changed);
    let insts = cfg.bblocks.get(0);
    assert_eq!(insts.len(), 1);
    assert_eq!(insts[0].t, InstTyp::Call);
    assert!(insts[0].tgts.is_empty(), "expected target to be cleared");
  }

  #[test]
  fn pure_call_with_unused_target_is_removed_when_purity_metadata_is_present() {
    let mut inst = Inst::call(
      0,
      Arg::Builtin("Math.abs".to_string()),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    inst.meta.callee_purity = Purity::Pure;
    let mut cfg = cfg_single_block(vec![inst]);
    let result = optpass_trivial_dce(&mut cfg);
    assert!(result.changed);
    assert!(cfg.bblocks.get(0).is_empty(), "expected pure call to be removed");
  }
}
