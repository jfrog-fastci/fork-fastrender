use crate::analysis::effect;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, InstTyp};
use crate::opt::PassResult;
use crate::ssa::phi_simplify::simplify_phis;
use effect_model::ThrowBehavior;
use itertools::Itertools;

/// Prune impossible exception paths.
///
/// Today this focuses on removing `Invoke` exception edges when we can prove the
/// callee cannot throw (e.g. internal literal construction helpers). Once those
/// edges are removed, the resulting unreachable catch/landingpad blocks are
/// deleted and Phi nodes are simplified accordingly.
pub fn optpass_exception_prune(cfg: &mut Cfg) -> PassResult {
  let mut result = PassResult::default();

  // First pass: convert `Invoke` to `Call` when the exception edge is impossible.
  for label in cfg.graph.labels_sorted() {
    let Some(inst) = cfg.bblocks.get(label).last() else {
      continue;
    };
    if inst.t != InstTyp::Invoke {
      continue;
    }

    // Extract data without holding a borrow across the mutation below.
    let (_tgt, callee, _this, _args, _spreads, _normal, exception) = inst.as_invoke();

    // `inst_local_effect` intentionally excludes interprocedural effects for `Arg::Fn`
    // callees, so only prune builtins we can classify locally.
    let can_throw = match callee {
      Arg::Builtin(_) => {
        let eff = effect::inst_local_effect(inst);
        !matches!(eff.summary.throws, ThrowBehavior::Never)
      }
      _ => true,
    };

    if can_throw {
      continue;
    }

    // Rewrite the terminator.
    let inst = cfg
      .bblocks
      .get_mut(label)
      .last_mut()
      .expect("checked above");
    inst.t = InstTyp::Call;
    inst.labels.clear();

    // Remove the exception edge; the normal continuation is already represented by the CFG edge.
    cfg.graph.disconnect(label, exception);
    result.mark_cfg_changed();
  }

  // Removing exception edges may render catch/landingpad blocks unreachable.
  let mut to_delete = cfg.graph.find_unreachable(cfg.entry).collect_vec();
  to_delete.sort_unstable();
  if !to_delete.is_empty() {
    cfg.graph.delete_many(to_delete.clone());
    cfg.bblocks.remove_many(to_delete);
    result.mark_cfg_changed();
  }

  if simplify_phis(cfg) {
    result.mark_changed();
  }

  #[cfg(debug_assertions)]
  {
    crate::ssa::phi_simplify::validate_phis(cfg).expect("phi validation failed after exception prune");
  }

  result
}

