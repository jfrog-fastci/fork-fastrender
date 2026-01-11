use crate::cfg::cfg::Cfg;
use crate::il::inst::Arg;
use crate::il::inst::InstTyp;
use ahash::HashMap;
use ahash::HashMapExt;
use std::collections::VecDeque;

/// Conservative escape classification for SSA values.
///
/// This is intentionally simple: we treat any use in a context that can store the
/// value beyond the current function as an escape, then propagate that escape
/// through obvious aliasing operations (`VarAssign` and `Phi`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EscapeState {
  /// No observed escape.
  NoEscape,
  /// Escapes through being passed as an argument.
  ArgEscape,
  /// Escapes by being returned from the current function.
  ///
  /// Note: `optimize-js` IR does not currently model returns explicitly; this is
  /// kept for forward compatibility.
  ReturnEscape,
  /// Escapes to global/outer scope storage (e.g. `ForeignStore`).
  GlobalEscape,
  /// Escapes in an unknown way (e.g. `UnknownStore` or `PropAssign`).
  Unknown,
}

impl EscapeState {
  pub fn join(self, other: Self) -> Self {
    use EscapeState::*;
    // Deterministic "worst wins" join.
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (GlobalEscape, _) | (_, GlobalEscape) => GlobalEscape,
      (ReturnEscape, _) | (_, ReturnEscape) => ReturnEscape,
      (ArgEscape, _) | (_, ArgEscape) => ArgEscape,
      (NoEscape, NoEscape) => NoEscape,
    }
  }

  pub fn escapes(self) -> bool {
    self != EscapeState::NoEscape
  }
}

pub type EscapeResult = HashMap<u32, EscapeState>;

fn add_alias_edge(edges: &mut HashMap<u32, Vec<u32>>, a: u32, b: u32) {
  edges.entry(a).or_default().push(b);
  edges.entry(b).or_default().push(a);
}

fn mark_escape(states: &mut EscapeResult, queue: &mut VecDeque<u32>, var: u32, esc: EscapeState) {
  let entry = states.entry(var).or_insert(EscapeState::NoEscape);
  let next = entry.join(esc);
  if next != *entry {
    *entry = next;
    queue.push_back(var);
  }
}

pub fn analyze_cfg_escapes(cfg: &Cfg) -> EscapeResult {
  let mut alias_edges: HashMap<u32, Vec<u32>> = HashMap::new();
  let mut states: EscapeResult = HashMap::new();
  let mut queue = VecDeque::new();

  for (_label, insts) in cfg.bblocks.all() {
    for inst in insts {
      match inst.t {
        // Aliasing operations.
        InstTyp::VarAssign => {
          if let (Some(&tgt), Some(&Arg::Var(src))) = (inst.tgts.get(0), inst.args.get(0)) {
            add_alias_edge(&mut alias_edges, tgt, src);
          }
        }
        InstTyp::Phi => {
          if let Some(&tgt) = inst.tgts.get(0) {
            for arg in &inst.args {
              if let Arg::Var(src) = arg {
                add_alias_edge(&mut alias_edges, tgt, *src);
              }
            }
          }
        }
        _ => {}
      }

      // Escape sites.
      match inst.t {
        InstTyp::ForeignStore => {
          if let Some(Arg::Var(v)) = inst.args.get(0) {
            mark_escape(&mut states, &mut queue, *v, EscapeState::GlobalEscape);
          }
        }
        InstTyp::UnknownStore => {
          if let Some(Arg::Var(v)) = inst.args.get(0) {
            mark_escape(&mut states, &mut queue, *v, EscapeState::Unknown);
          }
        }
        InstTyp::PropAssign => {
          // args[2] is the assigned value.
          if let Some(Arg::Var(v)) = inst.args.get(2) {
            mark_escape(&mut states, &mut queue, *v, EscapeState::Unknown);
          }
        }
        InstTyp::Call => {
          // Any argument passed to an unknown call might be retained.
          for arg in &inst.args {
            if let Arg::Var(v) = arg {
              mark_escape(&mut states, &mut queue, *v, EscapeState::ArgEscape);
            }
          }
        }
        _ => {}
      }
    }
  }

  // Propagate escapes through aliasing edges.
  while let Some(var) = queue.pop_front() {
    let esc = states.get(&var).copied().unwrap_or(EscapeState::NoEscape);
    let Some(neighbors) = alias_edges.get(&var) else {
      continue;
    };
    for &neighbor in neighbors {
      let entry = states.entry(neighbor).or_insert(EscapeState::NoEscape);
      let next = entry.join(esc);
      if next != *entry {
        *entry = next;
        queue.push_back(neighbor);
      }
    }
  }

  states
}
