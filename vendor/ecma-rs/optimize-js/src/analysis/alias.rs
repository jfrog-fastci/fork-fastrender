use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Inst, InstTyp};
use crate::symbol::semantics::SymbolId;
use ahash::HashMap;
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AbstractLoc {
  /// Unknown location; aliases everything.
  Top,
  /// A distinct heap allocation site.
  Alloc { block: u32, inst_idx: u32 },
  /// A value loaded from a captured variable.
  Foreign(SymbolId),
  /// A value loaded from a global/unknown name.
  UnknownGlobal(String),
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct PointsToSet {
  locs: BTreeSet<AbstractLoc>,
}

impl PointsToSet {
  pub fn empty() -> Self {
    Self::default()
  }

  pub fn top() -> Self {
    Self::singleton(AbstractLoc::Top)
  }

  pub fn singleton(loc: AbstractLoc) -> Self {
    let mut locs = BTreeSet::new();
    locs.insert(loc);
    let mut out = Self { locs };
    out.canonicalize();
    out
  }

  pub fn is_empty(&self) -> bool {
    self.locs.is_empty()
  }

  pub fn len(&self) -> usize {
    self.locs.len()
  }

  pub fn is_top(&self) -> bool {
    self.locs.contains(&AbstractLoc::Top)
  }

  pub fn iter(&self) -> impl Iterator<Item = &AbstractLoc> {
    self.locs.iter()
  }

  fn canonicalize(&mut self) {
    if self.locs.contains(&AbstractLoc::Top) && self.locs.len() > 1 {
      self.locs.clear();
      self.locs.insert(AbstractLoc::Top);
    }
  }

  pub fn union_with(&mut self, other: &Self) {
    if self.is_top() {
      return;
    }
    if other.is_top() {
      self.locs.clear();
      self.locs.insert(AbstractLoc::Top);
      return;
    }
    self.locs.extend(other.locs.iter().cloned());
    self.canonicalize();
  }
}

impl fmt::Debug for PointsToSet {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if self.is_top() {
      return write!(f, "Top");
    }
    f.debug_set().entries(self.locs.iter()).finish()
  }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AliasResult {
  pub points_to: HashMap<u32, PointsToSet>,
}

impl AliasResult {
  pub fn points_to_sorted(&self) -> Vec<(u32, &PointsToSet)> {
    let mut entries: Vec<_> = self.points_to.iter().map(|(k, v)| (*k, v)).collect();
    entries.sort_by_key(|(k, _)| *k);
    entries
  }

  pub fn may_alias(&self, a: u32, b: u32) -> bool {
    if a == b {
      return true;
    }
    let a_pts = self.points_to.get(&a);
    let b_pts = self.points_to.get(&b);
    let Some(a_pts) = a_pts else {
      // Unknown variable => Top.
      return true;
    };
    let Some(b_pts) = b_pts else {
      return true;
    };
    if a_pts.is_top() || b_pts.is_top() {
      return true;
    }
    if a_pts.is_empty() || b_pts.is_empty() {
      return false;
    }
    // BTreeSet iteration order is deterministic.
    a_pts.locs.iter().any(|loc| b_pts.locs.contains(loc))
  }

  pub fn no_alias(&self, a: u32, b: u32) -> bool {
    !self.may_alias(a, b)
  }

  pub fn must_alias(&self, a: u32, b: u32) -> bool {
    if a == b {
      return true;
    }
    let Some(a_pts) = self.points_to.get(&a) else {
      return false;
    };
    let Some(b_pts) = self.points_to.get(&b) else {
      return false;
    };
    if a_pts.is_top() || b_pts.is_top() {
      return false;
    }
    a_pts.len() == 1 && a_pts == b_pts
  }
}

impl fmt::Debug for AliasResult {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    struct Sorted<'a>(&'a HashMap<u32, PointsToSet>);

    impl fmt::Debug for Sorted<'_> {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(k, _)| *k);
        let mut map = f.debug_map();
        for (k, v) in entries {
          map.entry(k, v);
        }
        map.finish()
      }
    }

    f.debug_struct("AliasResult")
      .field("points_to", &Sorted(&self.points_to))
      .finish()
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct InstLoc {
  label: u32,
  idx: usize,
}

fn pop_first(worklist: &mut BTreeSet<InstLoc>) -> Option<InstLoc> {
  let first = *worklist.iter().next()?;
  worklist.take(&first)
}

fn points_to_of_arg(result: &AliasResult, arg: &Arg) -> PointsToSet {
  match arg {
    Arg::Var(v) => result.points_to.get(v).cloned().unwrap_or_else(PointsToSet::top),
    Arg::Const(_) => PointsToSet::empty(),
    Arg::Builtin(name) => PointsToSet::singleton(AbstractLoc::UnknownGlobal(name.clone())),
    Arg::Fn(_) => PointsToSet::top(),
  }
}

fn is_internal_alloc_builder(callee: &Arg) -> bool {
  let Arg::Builtin(name) = callee else {
    return false;
  };
  matches!(
    name.as_str(),
    "__optimize_js_array" | "__optimize_js_object" | "__optimize_js_regex"
  )
}

fn points_to_of_inst(loc: InstLoc, inst: &Inst, result: &AliasResult) -> PointsToSet {
  match inst.t {
    InstTyp::VarAssign => {
      let (_, arg) = inst.as_var_assign();
      points_to_of_arg(result, arg)
    }
    InstTyp::Phi => {
      let mut out = PointsToSet::empty();
      for arg in inst.args.iter() {
        out.union_with(&points_to_of_arg(result, arg));
        if out.is_top() {
          break;
        }
      }
      out
    }
    InstTyp::ForeignLoad => {
      let (_, sym) = inst.as_foreign_load();
      PointsToSet::singleton(AbstractLoc::Foreign(sym))
    }
    InstTyp::UnknownLoad => {
      let (_, name) = inst.as_unknown_load();
      PointsToSet::singleton(AbstractLoc::UnknownGlobal(name.clone()))
    }
    InstTyp::Call => {
      let (tgt, callee, _, _, _) = inst.as_call();
      if tgt.is_none() {
        return PointsToSet::empty();
      }
      if is_internal_alloc_builder(callee) {
        PointsToSet::singleton(AbstractLoc::Alloc {
          block: loc.label,
          inst_idx: loc.idx as u32,
        })
      } else {
        PointsToSet::top()
      }
    }
    InstTyp::Bin => {
      let (_, _, op, _) = inst.as_bin();
      match op {
        BinOp::GetProp => PointsToSet::top(),
        _ => PointsToSet::empty(),
      }
    }
    InstTyp::Un => PointsToSet::empty(),
    // Any other instruction that defines a SSA variable is conservatively treated as unknown.
    _ => PointsToSet::top(),
  }
}

pub fn calculate_alias(cfg: &Cfg) -> AliasResult {
  let mut labels = cfg.graph.labels_sorted();
  labels.extend(cfg.bblocks.all().map(|(label, _)| label));
  labels.sort_unstable();
  labels.dedup();

  let mut worklist = BTreeSet::<InstLoc>::new();
  let mut dependents: HashMap<u32, Vec<InstLoc>> = HashMap::default();
  let mut result = AliasResult::default();

  for label in labels {
    let Some(block) = cfg.bblocks.maybe_get(label) else {
      continue;
    };
    for (idx, inst) in block.iter().enumerate() {
      if inst.tgts.is_empty() {
        continue;
      }
      let loc = InstLoc { label, idx };
      worklist.insert(loc);
      for arg in inst.args.iter() {
        if let Arg::Var(var) = arg {
          dependents.entry(*var).or_default().push(loc);
        }
      }
      for &tgt in inst.tgts.iter() {
        result.points_to.entry(tgt).or_default();
      }
    }
  }

  for deps in dependents.values_mut() {
    deps.sort_unstable();
    deps.dedup();
  }

  while let Some(loc) = pop_first(&mut worklist) {
    let Some(block) = cfg.bblocks.maybe_get(loc.label) else {
      continue;
    };
    let Some(inst) = block.get(loc.idx) else {
      continue;
    };
    if inst.tgts.is_empty() {
      continue;
    }

    let new_points_to = points_to_of_inst(loc, inst, &result);
    for &tgt in inst.tgts.iter() {
      let old = result.points_to.get(&tgt).expect("target variables seeded");
      if old == &new_points_to {
        continue;
      }
      result.points_to.insert(tgt, new_points_to.clone());
      if let Some(users) = dependents.get(&tgt) {
        for &user in users {
          worklist.insert(user);
        }
      }
    }
  }

  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::{Const, Inst};

  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    for &label in &labels {
      if !graph.contains(label) {
        graph.ensure_label(label);
      }
    }

    let mut bblocks = CfgBBlocks::default();
    for (label, insts) in blocks.iter() {
      bblocks.add(*label, insts.clone());
    }
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  #[test]
  fn distinct_alloc_sites_do_not_alias() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::call(
            1,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
        ],
      )],
      &[],
    );

    let result = calculate_alias(&cfg);
    assert!(result.no_alias(0, 1));
    assert!(!result.may_alias(0, 1));
    assert!(!result.must_alias(0, 1));
    assert_eq!(
      result.points_to.get(&0),
      Some(&PointsToSet::singleton(AbstractLoc::Alloc {
        block: 0,
        inst_idx: 0
      }))
    );
    assert_eq!(
      result.points_to.get(&1),
      Some(&PointsToSet::singleton(AbstractLoc::Alloc {
        block: 0,
        inst_idx: 1
      }))
    );
  }

  #[test]
  fn var_assign_propagates_points_to() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_object".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          ),
          Inst::var_assign(1, Arg::Var(0)),
        ],
      )],
      &[],
    );

    let result = calculate_alias(&cfg);
    assert!(result.must_alias(0, 1));
    assert!(result.may_alias(0, 1));
    assert!(!result.no_alias(0, 1));
  }

  #[test]
  fn phi_merges_allocations() {
    let mut phi = Inst::phi_empty(2);
    phi.insert_phi(1, Arg::Var(0));
    phi.insert_phi(2, Arg::Var(1));

    let cfg = cfg_with_blocks(
      &[
        (0, vec![]),
        (
          1,
          vec![Inst::call(
            0,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          )],
        ),
        (
          2,
          vec![Inst::call(
            1,
            Arg::Builtin("__optimize_js_array".to_string()),
            Arg::Const(Const::Undefined),
            vec![],
            vec![],
          )],
        ),
        (3, vec![phi]),
      ],
      &[(0, 1), (0, 2), (1, 3), (2, 3)],
    );

    let result = calculate_alias(&cfg);
    assert!(result.no_alias(0, 1));
    assert!(result.may_alias(2, 0));
    assert!(result.may_alias(2, 1));
    assert!(!result.must_alias(2, 0));
    assert!(!result.must_alias(2, 1));
  }

  #[test]
  fn foreign_and_unknown_loads_have_stable_locations() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::foreign_load(0, SymbolId(1)),
          Inst::foreign_load(1, SymbolId(1)),
          Inst::foreign_load(2, SymbolId(2)),
          Inst::unknown_load(3, "g".to_string()),
          Inst::unknown_load(4, "g".to_string()),
          Inst::unknown_load(5, "h".to_string()),
        ],
      )],
      &[],
    );

    let result = calculate_alias(&cfg);
    assert!(result.must_alias(0, 1));
    assert!(result.must_alias(3, 4));
    assert!(result.no_alias(3, 5));
    assert!(result.no_alias(0, 2));
  }
}
