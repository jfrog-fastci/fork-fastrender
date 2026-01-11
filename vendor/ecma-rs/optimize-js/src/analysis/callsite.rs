use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, InstTyp};
use ahash::HashMap;
use ahash::HashMapExt;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallSiteCallee {
  DirectBuiltin(String),
  DirectFn(usize),
  Member { receiver: Arg, property: Arg },
  Indirect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSiteInfo {
  pub callee: CallSiteCallee,
  pub this_arg: Arg,
}

pub type CallSiteMap = BTreeMap<(u32 /*label*/, usize /*inst_idx*/), CallSiteInfo>;

pub fn analyze_callsites(cfg: &Cfg) -> CallSiteMap {
  // Track variables that are known to come from a `GetProp` binop.
  //
  // Note: this analysis is intended to run on SSA-ish IL. In fully deconstructed SSA, the
  // same variable may be assigned in multiple predecessor blocks; this analysis deliberately
  // does not model that control-flow sensitivity yet.
  let mut getprop_origin = HashMap::<u32, (Arg, Arg)>::new();

  // Track trivial var-to-var aliases so we can resolve `tgt` back to a `GetProp` definition.
  let mut aliases = HashMap::<u32, u32>::new();

  // First pass: collect direct `GetProp` definitions and `VarAssign` aliases deterministically.
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label).iter() {
      match inst.t {
        InstTyp::Bin if inst.bin_op == BinOp::GetProp && inst.tgts.len() == 1 => {
          let tgt = inst.tgts[0];
          let obj = inst.args[0].clone();
          let prop = inst.args[1].clone();
          getprop_origin.insert(tgt, (obj, prop));
        }
        InstTyp::VarAssign if inst.tgts.len() == 1 => {
          if let Arg::Var(src) = inst.args[0] {
            aliases.insert(inst.tgts[0], src);
          }
        }
        _ => {}
      }
    }
  }

  // Resolve a variable through trivial aliases to its underlying `GetProp` origin, if any.
  fn resolve_getprop_origin(
    var: u32,
    getprop_origin: &HashMap<u32, (Arg, Arg)>,
    aliases: &HashMap<u32, u32>,
    memo: &mut HashMap<u32, Option<(Arg, Arg)>>,
    visiting: &mut BTreeSet<u32>,
  ) -> Option<(Arg, Arg)> {
    if let Some(cached) = memo.get(&var) {
      return cached.clone();
    }

    // Break alias cycles deterministically.
    if !visiting.insert(var) {
      memo.insert(var, None);
      return None;
    }

    let res = if let Some(origin) = getprop_origin.get(&var) {
      Some(origin.clone())
    } else if let Some(src) = aliases.get(&var) {
      resolve_getprop_origin(*src, getprop_origin, aliases, memo, visiting)
    } else {
      None
    };

    visiting.remove(&var);
    memo.insert(var, res.clone());
    res
  }

  // Second pass: build callsite map.
  let mut callsites = CallSiteMap::new();
  let mut memo = HashMap::<u32, Option<(Arg, Arg)>>::new();
  for label in cfg.graph.labels_sorted() {
    for (inst_idx, inst) in cfg.bblocks.get(label).iter().enumerate() {
      if inst.t != InstTyp::Call {
        continue;
      }

      let (_tgt, callee, this_arg, _args, _spreads) = inst.as_call();
      let callee = match callee {
        Arg::Builtin(path) => CallSiteCallee::DirectBuiltin(path.clone()),
        Arg::Fn(id) => CallSiteCallee::DirectFn(*id),
        Arg::Var(v) => {
          let mut visiting = BTreeSet::new();
          if let Some((receiver, property)) =
            resolve_getprop_origin(*v, &getprop_origin, &aliases, &mut memo, &mut visiting)
          {
            if &receiver == this_arg {
              CallSiteCallee::Member { receiver, property }
            } else {
              CallSiteCallee::Indirect
            }
          } else {
            CallSiteCallee::Indirect
          }
        }
        _ => CallSiteCallee::Indirect,
      };

      callsites.insert(
        (label, inst_idx),
        CallSiteInfo {
          callee,
          this_arg: this_arg.clone(),
        },
      );
    }
  }

  callsites
}
