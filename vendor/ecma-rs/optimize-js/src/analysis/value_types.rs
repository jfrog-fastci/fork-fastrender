use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, ValueTypeSummary};
use ahash::HashMap;

/// Query helper for per-variable [`ValueTypeSummary`] information preserved into
/// the IL.
#[derive(Clone, Debug, Default)]
pub struct ValueTypeSummaries {
  vars: HashMap<u32, ValueTypeSummary>,
}

impl ValueTypeSummaries {
  pub fn new(cfg: &Cfg) -> Self {
    let mut vars: HashMap<u32, ValueTypeSummary> = HashMap::default();
    for (_, bblock) in cfg.bblocks.all() {
      for inst in bblock.iter() {
        if inst.value_type.is_unknown() {
          continue;
        }
        for &tgt in inst.tgts.iter() {
          vars
            .entry(tgt)
            .and_modify(|existing| *existing |= inst.value_type)
            .or_insert(inst.value_type);
        }
      }
    }
    Self { vars }
  }

  pub fn var(&self, var: u32) -> Option<ValueTypeSummary> {
    self.vars.get(&var).copied()
  }

  pub fn arg(&self, arg: &Arg) -> Option<ValueTypeSummary> {
    match arg {
      Arg::Const(c) => Some(ValueTypeSummary::from_const(c)),
      Arg::Var(v) => self.var(*v),
      Arg::Fn(_) => Some(ValueTypeSummary::FUNCTION),
      Arg::Builtin(_) => None,
    }
  }
}

