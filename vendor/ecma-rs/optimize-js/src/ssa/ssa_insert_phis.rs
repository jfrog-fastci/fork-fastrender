use crate::cfg::cfg::Cfg;
use crate::dom::Dom;
use crate::il::inst::Inst;
use crate::types::TypeId;
use ahash::HashMap;
use ahash::HashSet;
use itertools::Itertools;
use std::collections::VecDeque;

pub fn insert_phis_for_ssa_construction(
  defs: &mut HashMap<u32, HashSet<u32>>,
  cfg: &mut Cfg,
  dom: &Dom,
) {
  // Best-effort type metadata for pre-SSA temporaries. This must run before we insert any new Phi
  // nodes so that we only consider "real" definitions from lowering.
  #[derive(Clone, Copy, Debug)]
  enum TypeState {
    Unknown,
    Known(TypeId),
    Conflict,
  }

  let mut type_state: HashMap<u32, TypeState> = defs
    .keys()
    .map(|&v| (v, TypeState::Unknown))
    .collect();

  for (_, block) in cfg.bblocks.all() {
    for inst in block.iter() {
      let Some(type_id) = inst.meta.type_id else {
        continue;
      };
      for &tgt in inst.tgts.iter() {
        let Some(state) = type_state.get_mut(&tgt) else {
          continue;
        };
        *state = match *state {
          TypeState::Unknown => TypeState::Known(type_id),
          TypeState::Known(existing) if existing == type_id => TypeState::Known(existing),
          TypeState::Known(_) => TypeState::Conflict,
          TypeState::Conflict => TypeState::Conflict,
        };
      }
    }
  }

  let type_ids: HashMap<u32, Option<TypeId>> = type_state
    .into_iter()
    .map(|(v, state)| {
      (
        v,
        match state {
          TypeState::Known(id) => Some(id),
          TypeState::Unknown | TypeState::Conflict => None,
        },
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
        phi.meta.type_id = type_ids.get(&v).copied().flatten();
        cfg.bblocks.get_mut(label).insert(0, phi);
        defs.get_mut(&v).unwrap().insert(label);
        if seen.insert(label) {
          q.push_back(label);
        };
      }
    }
  }
}
