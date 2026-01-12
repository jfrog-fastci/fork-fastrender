use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::Inst;
use crate::types::{TypeId, ValueTypeSummary};
use ahash::HashMap;
use ahash::HashSet;
use hir_js::ExprId;
use itertools::Itertools;
use std::collections::VecDeque;

pub fn insert_phis_for_ssa_construction(
  defs: &mut HashMap<u32, HashSet<u32>>,
  cfg: &mut Cfg,
  dom: &Dom,
) {
  // Best-effort metadata for pre-SSA temporaries. This must run before we insert any new Phi
  // nodes so that we only consider "real" definitions from lowering.
  //
  // We derive per-variable facts from the metadata on its defining instructions. Phi nodes created
  // by SSA construction would otherwise lose typed metadata (e.g. HIR expr id / type summary) once
  // they are lowered into VarAssign instructions during SSA deconstruction.
  #[derive(Clone, Copy, Debug)]
  enum FactState<T> {
    Unknown,
    Known(T),
    Conflict,
  }

  impl<T: Copy + Eq> FactState<T> {
    fn update(&mut self, value: T) {
      *self = match *self {
        FactState::Unknown => FactState::Known(value),
        FactState::Known(existing) if existing == value => FactState::Known(existing),
        FactState::Known(_) => FactState::Conflict,
        FactState::Conflict => FactState::Conflict,
      };
    }

    fn into_option(self) -> Option<T> {
      match self {
        FactState::Known(value) => Some(value),
        FactState::Unknown | FactState::Conflict => None,
      }
    }
  }

  #[derive(Clone, Copy, Debug)]
  struct VarFacts {
    type_id: FactState<TypeId>,
    #[cfg(feature = "typed")]
    native_layout: FactState<types_ts_interned::LayoutId>,
    hir_expr: FactState<ExprId>,
    type_summary: FactState<ValueTypeSummary>,
    excludes_nullish: FactState<bool>,
  }

  impl Default for VarFacts {
    fn default() -> Self {
      Self {
        type_id: FactState::Unknown,
        #[cfg(feature = "typed")]
        native_layout: FactState::Unknown,
        hir_expr: FactState::Unknown,
        type_summary: FactState::Unknown,
        excludes_nullish: FactState::Unknown,
      }
    }
  }

  let mut fact_state: HashMap<u32, VarFacts> = defs.keys().map(|&v| (v, VarFacts::default())).collect();

  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      for &tgt in inst.tgts.iter() {
        let Some(state) = fact_state.get_mut(&tgt) else {
          continue;
        };

        if let Some(type_id) = inst.meta.type_id {
          state.type_id.update(type_id);
        }
        #[cfg(feature = "typed")]
        if let Some(layout) = inst.meta.native_layout {
          state.native_layout.update(layout);
        }
        if let Some(expr_id) = inst.meta.hir_expr {
          state.hir_expr.update(expr_id);
        }
        if let Some(summary) = inst.meta.type_summary {
          state.type_summary.update(summary);
        }
        if inst.meta.hir_expr.is_some()
          || inst.meta.type_summary.is_some()
          || inst.meta.type_id.is_some()
        {
          state.excludes_nullish.update(inst.meta.excludes_nullish);
        }
      }
    }
  }

  #[cfg(not(feature = "typed"))]
  let facts: HashMap<u32, (Option<TypeId>, Option<ExprId>, Option<ValueTypeSummary>, bool)> =
    fact_state
      .into_iter()
      .map(|(v, state)| {
        (
          v,
          (
            state.type_id.into_option(),
            state.hir_expr.into_option(),
            state.type_summary.into_option(),
            matches!(state.excludes_nullish, FactState::Known(true)),
          ),
        )
      })
      .collect();

  #[cfg(feature = "typed")]
  let facts: HashMap<
    u32,
    (
      Option<TypeId>,
      Option<types_ts_interned::LayoutId>,
      Option<ExprId>,
      Option<ValueTypeSummary>,
      bool,
    ),
  > = fact_state
    .into_iter()
    .map(|(v, state)| {
      (
        v,
        (
          state.type_id.into_option(),
          state.native_layout.into_option(),
          state.hir_expr.into_option(),
          state.type_summary.into_option(),
          matches!(state.excludes_nullish, FactState::Known(true)),
        ),
      )
    })
    .collect();

  let domfront = dom.dominance_frontiers(cfg);
  let mut vars = defs.keys().cloned().collect_vec();
  vars.sort_unstable();
  for v in vars {
    let mut already_inserted = HashSet::default();
    // We'll start with these blocks but add more as we process, so we can't just use `defs[v].iter()`.
    let mut queue_items: Vec<_> = defs[&v].iter().copied().collect();
    queue_items.sort_unstable();
    let mut q = VecDeque::from(queue_items);
    let mut seen = HashSet::from_iter(q.clone());
    while let Some(d) = q.pop_front() {
      // Look at the blocks in the dominance frontier for block `d`.
      let Some(blocks) = domfront.get(&d) else {
        continue;
      };
      let mut labels: Vec<_> = blocks.iter().copied().collect();
      labels.sort_unstable();
      for label in labels {
        if already_inserted.contains(&label) {
          continue;
        };
        already_inserted.insert(label);
        // We'll populate this new Phi inst later.
        let mut phi = Inst::phi_empty(v);
        #[cfg(not(feature = "typed"))]
        if let Some((type_id, hir_expr, type_summary, excludes_nullish)) = facts.get(&v).copied() {
          phi.meta.type_id = type_id;
          phi.meta.hir_expr = hir_expr;
          phi.meta.type_summary = type_summary;
          phi.meta.excludes_nullish = excludes_nullish;
          if let Some(summary) = type_summary {
            phi.value_type |= summary;
          }
        }
        #[cfg(feature = "typed")]
        if let Some((type_id, native_layout, hir_expr, type_summary, excludes_nullish)) =
          facts.get(&v).copied()
        {
          phi.meta.type_id = type_id;
          phi.meta.native_layout = native_layout;
          phi.meta.hir_expr = hir_expr;
          phi.meta.type_summary = type_summary;
          phi.meta.excludes_nullish = excludes_nullish;
          if let Some(summary) = type_summary {
            phi.value_type |= summary;
          }
        }
        cfg.bblocks.get_mut(label).insert(0, phi);
        defs.get_mut(&v).unwrap().insert(label);
        if seen.insert(label) {
          q.push_back(label);
        };
      }
    }
  }
}
