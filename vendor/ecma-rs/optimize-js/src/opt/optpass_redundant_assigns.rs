use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, InstTyp, ValueTypeSummary};
use crate::opt::PassResult;
use ahash::HashMap;
use ahash::HashMapExt;

// VarAssigns are always useless in strict SSA. However, dominator-based value numbering doesn't manage to detect and remove all such insts, with one reason being that DVNT only traverses domtree children.
// My theory for correctness:
// - Strict SSA requires all defs to dominate all their uses.
// - Targets are only assigned in one place globally.
pub fn optpass_redundant_assigns(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();
  let mut tgt_to_arg = HashMap::new();
  // When typed IL metadata is available we want to preserve it through copy
  // propagation. A VarAssign like `%x = %y` is eliminated by this pass, so any
  // type summary on `%x` needs to be transferred to `%y`'s defining instruction.
  let mut propagate_types = HashMap::<u32, ValueTypeSummary>::new();
  for (_, bblock) in cfg.bblocks.all_mut() {
    let mut to_delete = Vec::new();
    for (i, inst) in bblock.iter().enumerate() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      let (tgt, value) = inst.as_var_assign();
      if !inst.value_type.is_unknown() {
        if let Arg::Var(rhs) = value {
          propagate_types
            .entry(*rhs)
            .and_modify(|existing| *existing |= inst.value_type)
            .or_insert(inst.value_type);
        }
      }
      to_delete.push(i);
      assert!(tgt_to_arg.insert(tgt, value.clone()).is_none());
    }
    for i in to_delete.into_iter().rev() {
      bblock.remove(i);
      result.mark_changed();
    }
  }
  for (_, bblock) in cfg.bblocks.all_mut() {
    for inst in bblock.iter_mut() {
      for arg in inst.args.iter_mut() {
        let Arg::Var(t) = arg else {
          continue;
        };
        let Some(new_arg) = tgt_to_arg.get(t) else {
          continue;
        };
        *arg = new_arg.clone();
      }
    }
  }
  if !propagate_types.is_empty() {
    for (_, bblock) in cfg.bblocks.all_mut() {
      for inst in bblock.iter_mut() {
        for tgt in inst.tgts.iter() {
          if let Some(extra) = propagate_types.get(tgt).copied() {
            inst.value_type |= extra;
          }
        }
      }
    }
  }
  result
}
