use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, AwaitBehavior, Const, Inst, InstTyp, ValueTypeSummary};
use std::collections::BTreeMap;

/// Options controlling async elision / await minimization.
///
/// These options are intentionally conservative by default. In strict mode,
/// `await` must yield even on non-promise values; `aggressive=true` opts into a
/// relaxation where such awaits may be treated as not yielding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AsyncElisionOptions {
  /// When enabled, classify awaits of proven non-promise / non-thenable values
  /// as [`AwaitBehavior::MayNotYield`].
  pub aggressive: bool,
  /// When enabled, optimization passes may rewrite awaits and related patterns
  /// (e.g. singleton `Promise.all`) instead of only annotating metadata.
  pub rewrite: bool,
}

/// Stable identifier for an instruction within a CFG.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstKey {
  pub label: u32,
  pub index: usize,
}

/// Per-`await` classification results for a CFG.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AsyncElisionResult {
  pub awaits: BTreeMap<InstKey, AwaitBehavior>,
}

pub(crate) const INTERNAL_AWAIT_CALLEE: &str = "__optimize_js_await";

/// Returns the awaited operand for the internal await helper call, when `inst`
/// is an await.
pub fn await_operand(inst: &Inst) -> Option<&Arg> {
  if inst.t != InstTyp::Call {
    return None;
  }
  let (_, callee, this, args, spreads) = inst.as_call();
  if !spreads.is_empty() {
    return None;
  }
  if !matches!(this, Arg::Const(Const::Undefined)) {
    return None;
  }
  if !matches!(callee, Arg::Builtin(path) if path == INTERNAL_AWAIT_CALLEE) {
    return None;
  }
  if args.len() != 1 {
    return None;
  }
  Some(&args[0])
}

fn classify_await_operand(
  operand: &Arg,
  types: &ValueTypeSummaries,
  options: AsyncElisionOptions,
) -> AwaitBehavior {
  if !options.aggressive {
    return AwaitBehavior::MustYield;
  }

  let Some(summary) = types.arg(operand) else {
    return AwaitBehavior::MustYield;
  };

  // Conservatively treat any possible object/function as potentially thenable.
  if summary.is_unknown()
    || summary.contains(ValueTypeSummary::OBJECT)
    || summary.contains(ValueTypeSummary::FUNCTION)
  {
    return AwaitBehavior::MustYield;
  }

  AwaitBehavior::MayNotYield
}

/// Classify each `await` in `cfg`.
pub fn analyze_cfg_async_elision(cfg: &Cfg, options: AsyncElisionOptions) -> AsyncElisionResult {
  let types = ValueTypeSummaries::new(cfg);
  let mut awaits = BTreeMap::new();

  // Deterministic traversal order.
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  for label in labels {
    let bblock = cfg.bblocks.get(label);
    for (index, inst) in bblock.iter().enumerate() {
      let Some(operand) = await_operand(inst) else {
        continue;
      };
      let behavior = classify_await_operand(operand, &types, options);
      awaits.insert(InstKey { label, index }, behavior);
    }
  }

  AsyncElisionResult { awaits }
}

/// Build a stable set of all variables used in a CFG (arguments only).
pub(crate) fn cfg_var_uses(cfg: &Cfg) -> BTreeMap<u32, usize> {
  let mut uses = BTreeMap::<u32, usize>::new();
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get(label);
    for inst in block {
      for arg in &inst.args {
        if let Arg::Var(v) = arg {
          *uses.entry(*v).or_default() += 1;
        }
      }
    }
  }
  uses
}
