use crate::analysis::dataflow::{
  AnalysisBoundary, BlockState, DataFlowAnalysis, DataFlowResult, Direction,
  ResolvedAnalysisBoundary,
};
use crate::analysis::facts::{replay_forward_after_inst, replay_forward_before_inst, Edge, InstLoc};
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, StringEncoding, UnOp, ValueTypeSummary};
use ahash::HashMap;
use std::cmp;

/// Forward dataflow analysis state: dense temp-indexed encoding facts.
pub type EncodingState = Vec<StringEncoding>;

/// Result of the string encoding analysis.
///
/// The analysis is flow-sensitive (tracks state per basic block) and conservative:
/// it only classifies values as [`StringEncoding::Ascii`] / [`StringEncoding::Utf8`]
/// when they are proven to be string values derived from string literals.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EncodingResult {
  pub boundary: ResolvedAnalysisBoundary,
  #[cfg_attr(feature = "serde", serde(serialize_with = "crate::analysis::serde::serialize_hashmap_sorted"))]
  pub blocks: HashMap<u32, BlockState<EncodingState>>,
  pub temp_count: usize,
}

impl EncodingResult {
  /// State at basic block entry, after merging all incoming edges.
  pub fn state_at_block_entry(&self, label: u32) -> Option<&EncodingState> {
    self.block_entry(label)
  }

  /// State at basic block exit.
  pub fn state_at_block_exit(&self, label: u32) -> Option<&EncodingState> {
    self.block_exit(label)
  }

  /// State flowing into `edge.to` along the given edge.
  ///
  /// Encoding analysis is not edge-sensitive, so the incoming state is the same
  /// as the source block's exit state.
  pub fn state_at_edge_entry(&self, edge: Edge) -> Option<&EncodingState> {
    let _ = edge.to;
    self.block_exit(edge.from)
  }

  pub fn block_entry(&self, label: u32) -> Option<&EncodingState> {
    self.blocks.get(&label).map(|b| &b.entry)
  }

  pub fn block_exit(&self, label: u32) -> Option<&EncodingState> {
    self.blocks.get(&label).map(|b| &b.exit)
  }

  pub fn encoding_at_entry(&self, label: u32, var: u32) -> StringEncoding {
    self
      .block_entry(label)
      .and_then(|state| state.get(var as usize).copied())
      .unwrap_or(StringEncoding::Unknown)
  }

  pub fn encoding_at_exit(&self, label: u32, var: u32) -> StringEncoding {
    self
      .block_exit(label)
      .and_then(|state| state.get(var as usize).copied())
      .unwrap_or(StringEncoding::Unknown)
  }

  /// Compute the analysis state immediately before `inst_idx` in `label`.
  ///
  /// This is computed by replaying the transfer function inside the block from
  /// the stored block entry state. This avoids storing per-instruction states.
  pub fn state_before_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> EncodingState {
    let entry = self
      .block_entry(label)
      .cloned()
      .unwrap_or_else(|| vec![StringEncoding::Unknown; self.temp_count]);
    let mut analysis = EncodingAnalysis {
      temp_count: self.temp_count,
      types: ValueTypeSummaries::new(cfg),
    };
    replay_forward_before_inst(cfg, label, &entry, inst_idx, |label, inst_idx, inst, state| {
      analysis.apply_to_instruction(label, inst_idx, inst, state);
    })
  }

  /// Compute the analysis state immediately after `inst_idx` in `label`.
  pub fn state_after_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> EncodingState {
    let entry = self
      .block_entry(label)
      .cloned()
      .unwrap_or_else(|| vec![StringEncoding::Unknown; self.temp_count]);
    let mut analysis = EncodingAnalysis {
      temp_count: self.temp_count,
      types: ValueTypeSummaries::new(cfg),
    };
    replay_forward_after_inst(cfg, label, &entry, inst_idx, |label, inst_idx, inst, state| {
      analysis.apply_to_instruction(label, inst_idx, inst, state);
    })
  }

  pub fn state_before_loc(&self, cfg: &Cfg, loc: InstLoc) -> EncodingState {
    self.state_before_inst(cfg, loc.block, loc.inst)
  }

  pub fn state_after_loc(&self, cfg: &Cfg, loc: InstLoc) -> EncodingState {
    self.state_after_inst(cfg, loc.block, loc.inst)
  }

  pub fn fact_for_arg(&self, state: &EncodingState, arg: &Arg) -> StringEncoding {
    let analysis = EncodingAnalysis {
      temp_count: self.temp_count,
      types: ValueTypeSummaries::default(),
    };
    analysis.encoding_of_arg(state, arg)
  }
}

struct EncodingAnalysis {
  temp_count: usize,
  types: ValueTypeSummaries,
}

impl EncodingAnalysis {
  fn new(cfg: &Cfg) -> Self {
    Self {
      temp_count: count_temps(cfg),
      types: ValueTypeSummaries::new(cfg),
    }
  }

  #[inline]
  fn get(&self, state: &[StringEncoding], var: u32) -> StringEncoding {
    state
      .get(var as usize)
      .copied()
      .unwrap_or(StringEncoding::Unknown)
  }

  #[inline]
  fn set(&self, state: &mut [StringEncoding], var: u32, enc: StringEncoding) {
    if let Some(slot) = state.get_mut(var as usize) {
      *slot = enc;
    }
  }

  #[inline]
  fn encoding_for_str(value: &str) -> StringEncoding {
    if value.is_ascii() {
      StringEncoding::Ascii
    } else {
      // Treat any non-ASCII string as `Utf8` (contains non-ASCII).
      StringEncoding::Utf8
    }
  }

  #[inline]
  fn encoding_of_arg(&self, state: &[StringEncoding], arg: &Arg) -> StringEncoding {
    match arg {
      Arg::Const(Const::Str(s)) => Self::encoding_for_str(s),
      Arg::Var(var) => self.get(state, *var),
      _ => StringEncoding::Unknown,
    }
  }

  #[inline]
  fn is_definitely_string(&self, state: &[StringEncoding], arg: &Arg) -> bool {
    match arg {
      Arg::Const(Const::Str(_)) => true,
      Arg::Var(var) => self.get(state, *var) != StringEncoding::Unknown,
      _ => false,
    }
  }

  fn is_string_concat(&self, state: &[StringEncoding], tgt: u32, left: &Arg, right: &Arg) -> bool {
    // Existing heuristic: known string encodings imply the value is a string.
    let known_string_operand =
      self.is_definitely_string(state, left) || self.is_definitely_string(state, right);

    known_string_operand
      || self
        .types
        .var(tgt)
        .is_some_and(|ty| ty.is_definitely_string())
      || self
        .types
        .arg(left)
        .is_some_and(|ty| ty.is_definitely_string())
      || self
        .types
        .arg(right)
        .is_some_and(|ty| ty.is_definitely_string())
  }

  fn encoding_for_concat_operand(&self, state: &[StringEncoding], arg: &Arg) -> StringEncoding {
    match arg {
      Arg::Const(Const::Str(s)) => Self::encoding_for_str(s),
      // When either side of `+` is a string, the other side is stringified via
      // `ToPrimitive`/`ToString`. For these primitive kinds, the result is
      // guaranteed to be ASCII.
      Arg::Const(
        Const::BigInt(_) | Const::Bool(_) | Const::Null | Const::Num(_) | Const::Undefined,
      ) => StringEncoding::Ascii,
      Arg::Var(var) => {
        let enc = self.get(state, *var);
        let Some(ty) = self.types.var(*var) else {
          return enc;
        };

        if ty.is_definitely_string() {
          return enc;
        }

        // If the type is definitely within the set of primitives with ASCII
        // `ToString` results, treat it as ASCII even when we don't have a
        // literal-derived encoding fact.
        if !ty.is_unknown()
          && !ty.contains(ValueTypeSummary::STRING)
          && !ty.contains(ValueTypeSummary::SYMBOL)
          && !ty.contains(ValueTypeSummary::OBJECT)
          && !ty.contains(ValueTypeSummary::FUNCTION)
        {
          return StringEncoding::Ascii;
        }

        enc
      }
      Arg::Fn(_) | Arg::Builtin(_) => StringEncoding::Unknown,
    }
  }

  fn transfer_var_assign(&self, state: &mut [StringEncoding], tgt: u32, arg: &Arg) {
    let enc = match arg {
      Arg::Const(Const::Str(s)) => Self::encoding_for_str(s),
      Arg::Const(_) | Arg::Fn(_) | Arg::Builtin(_) => StringEncoding::Unknown,
      Arg::Var(src) => self.get(state, *src),
    };
    self.set(state, tgt, enc);
  }

  fn transfer_phi(&self, state: &mut [StringEncoding], tgt: u32, args: &[Arg]) {
    let mut acc: Option<StringEncoding> = None;
    for arg in args {
      let enc = self.encoding_of_arg(state, arg);
      acc = Some(match acc {
        None => enc,
        Some(existing) => {
          if existing == enc {
            existing
          } else {
            StringEncoding::Unknown
          }
        }
      });
      if matches!(acc, Some(StringEncoding::Unknown)) {
        break;
      }
    }
    self.set(state, tgt, acc.unwrap_or(StringEncoding::Unknown));
  }

  fn transfer_bin_add(&self, state: &mut [StringEncoding], tgt: u32, left: &Arg, right: &Arg) {
    // Treat as string concatenation only when at least one operand is definitely
    // a string, or when type information proves the result/operands are strings.
    let is_concat = self.is_string_concat(state, tgt, left, right);
    if !is_concat {
      self.set(state, tgt, StringEncoding::Unknown);
      return;
    }

    let left_enc = self.encoding_for_concat_operand(state, left);
    let right_enc = self.encoding_for_concat_operand(state, right);
    let normalize = |enc| match enc {
      StringEncoding::Latin1 => StringEncoding::Utf8,
      other => other,
    };
    let (left_enc, right_enc) = (normalize(left_enc), normalize(right_enc));

    let enc = match (left_enc, right_enc) {
      (StringEncoding::Ascii, StringEncoding::Ascii) => StringEncoding::Ascii,
      (StringEncoding::Utf8, StringEncoding::Ascii | StringEncoding::Utf8)
      | (StringEncoding::Ascii, StringEncoding::Utf8) => StringEncoding::Utf8,
      _ => StringEncoding::Unknown,
    };
    self.set(state, tgt, enc);
  }

  fn transfer_template_call(&self, state: &mut [StringEncoding], tgt: u32, args: &[Arg]) {
    if args.is_empty() {
      self.set(state, tgt, StringEncoding::Unknown);
      return;
    }

    let mut any_non_ascii = false;
    for arg in args {
      match arg {
        Arg::Const(Const::Str(s)) => {
          if !s.is_ascii() {
            any_non_ascii = true;
          }
        }
        _ => {
          self.set(state, tgt, StringEncoding::Unknown);
          return;
        }
      }
    }

    self.set(
      state,
      tgt,
      if any_non_ascii {
        StringEncoding::Utf8
      } else {
        StringEncoding::Ascii
      },
    );
  }
}

impl DataFlowAnalysis for EncodingAnalysis {
  type State = EncodingState;

  const DIRECTION: Direction = Direction::Forward;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    vec![StringEncoding::Unknown; self.temp_count]
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    let Some((_, first)) = states.first() else {
      return vec![StringEncoding::Unknown; self.temp_count];
    };

    // Clone through a slice copy: `states` stores references, so `clone()` would
    // clone the reference instead of the underlying vector.
    let mut merged = first.to_vec();
    for (_, state) in states.iter().skip(1) {
      debug_assert_eq!(merged.len(), state.len());
      for (dst, src) in merged.iter_mut().zip(state.iter()) {
        if *dst != *src {
          *dst = StringEncoding::Unknown;
        }
      }
    }
    merged
  }

  fn apply_to_instruction(
    &mut self,
    _label: u32,
    _inst_idx: usize,
    inst: &Inst,
    state: &mut Self::State,
  ) {
    match inst.t {
      InstTyp::VarAssign => {
        let (tgt, arg) = inst.as_var_assign();
        self.transfer_var_assign(state, tgt, arg);
      }
      InstTyp::Un => {
        let (tgt, op, _arg) = inst.as_un();
        let enc = match op {
          // `typeof` always yields one of a small set of ASCII string literals
          // ("undefined", "object", ...).
          UnOp::Typeof => StringEncoding::Ascii,
          _ => StringEncoding::Unknown,
        };
        self.set(state, tgt, enc);
      }
      InstTyp::Phi => {
        let tgt = inst.tgts[0];
        self.transfer_phi(state, tgt, &inst.args);
      }
      InstTyp::Bin => {
        let (tgt, left, op, right) = inst.as_bin();
        if op == BinOp::Add {
          self.transfer_bin_add(state, tgt, left, right);
        } else {
          self.set(state, tgt, StringEncoding::Unknown);
        }
      }
      InstTyp::Call => {
        let Some(tgt) = inst.tgts.get(0).copied() else {
          return;
        };
        let (_, callee, _, args, _) = inst.as_call();
        match callee {
          Arg::Builtin(path) if path == "__optimize_js_template" => {
            self.transfer_template_call(state, tgt, args);
          }
          _ => {
            self.set(state, tgt, StringEncoding::Unknown);
          }
        }
      }
      // Any other value-producing instruction is treated as unknown.
      _ => {
        for &tgt in inst.tgts.iter() {
          self.set(state, tgt, StringEncoding::Unknown);
        }
      }
    }
  }
}

pub fn analyze_cfg_encoding(cfg: &Cfg) -> EncodingResult {
  let mut analysis = EncodingAnalysis::new(cfg);
  let temp_count = analysis.temp_count;
  let DataFlowResult { boundary, blocks } =
    analysis.analyze(cfg, AnalysisBoundary::Entry(cfg.entry));
  EncodingResult {
    boundary,
    blocks,
    temp_count,
  }
}

pub fn annotate_cfg_encoding(cfg: &mut Cfg, result: &EncodingResult) {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  let mut analysis = EncodingAnalysis {
    temp_count: result.temp_count,
    types: ValueTypeSummaries::new(cfg),
  };
  for label in labels {
    let Some(block_state) = result.blocks.get(&label) else {
      continue;
    };
    let mut state = block_state.entry.clone();

    let insts_len = cfg.bblocks.get(label).len();
    for inst_idx in 0..insts_len {
      let encoding_for_inst = {
        let inst = &cfg.bblocks.get(label)[inst_idx];
        analysis.apply_to_instruction(label, inst_idx, inst, &mut state);
        inst.tgts.get(0).copied().map(|tgt| {
          let enc = state
            .get(tgt as usize)
            .copied()
            .unwrap_or(StringEncoding::Unknown);
          (tgt, enc)
        })
      };

      let Some((_tgt, enc)) = encoding_for_inst else {
        continue;
      };
      if matches!(enc, StringEncoding::Ascii | StringEncoding::Utf8) {
        cfg.bblocks.get_mut(label)[inst_idx]
          .meta
          .result_type
          .string_encoding = Some(enc);
      }
    }
  }
}

fn count_temps(cfg: &Cfg) -> usize {
  let max_tgt = cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .flat_map(|inst| inst.tgts.iter().copied())
    .max();

  let max_arg = cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .flat_map(|inst| inst.args.iter())
    .filter_map(|arg| match arg {
      Arg::Var(t) => Some(*t),
      _ => None,
    })
    .max();

  let Some(max_temp) = cmp::max(max_tgt, max_arg) else {
    return 0;
  };
  usize::try_from(max_temp).unwrap() + 1
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};

  fn cfg_with_blocks(blocks: &[(u32, Vec<Inst>)], edges: &[(u32, u32)]) -> Cfg {
    let labels: Vec<u32> = blocks.iter().map(|(label, _)| *label).collect();
    let mut graph = CfgGraph::default();
    for &(from, to) in edges {
      graph.connect(from, to);
    }
    for &label in &labels {
      if !graph.contains(label) {
        graph.connect(label, label);
        graph.disconnect(label, label);
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
  fn distinguishes_ascii_and_utf8_literals() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Str("hello".to_string()))),
            Inst::var_assign(1, Arg::Const(Const::Str("π".to_string()))),
          ],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 0), StringEncoding::Ascii);
    assert_eq!(result.encoding_at_entry(1, 1), StringEncoding::Utf8);
  }

  #[test]
  fn latin1_literals_are_treated_as_utf8() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![Inst::var_assign(
            0,
            Arg::Const(Const::Str("ÿ".to_string())),
          )],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );
    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 0), StringEncoding::Utf8);
  }

  #[test]
  fn typeof_is_ascii() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![Inst::un(
            0,
            UnOp::Typeof,
            Arg::Const(Const::Undefined),
          )],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );
    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 0), StringEncoding::Ascii);
  }

  #[test]
  fn concatenation_of_ascii_literals_is_ascii() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Str("a".to_string()))),
            Inst::var_assign(1, Arg::Const(Const::Str("b".to_string()))),
            Inst::bin(2, Arg::Var(0), BinOp::Add, Arg::Var(1)),
          ],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 2), StringEncoding::Ascii);
  }

  #[test]
  fn concatenation_with_non_ascii_literal_is_utf8() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Str("a".to_string()))),
            Inst::var_assign(1, Arg::Const(Const::Str("π".to_string()))),
            Inst::bin(2, Arg::Var(0), BinOp::Add, Arg::Var(1)),
          ],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 2), StringEncoding::Utf8);
  }

  #[test]
  fn join_disagrees_to_unknown() {
    let cfg = cfg_with_blocks(
      &[
        (0, vec![Inst::cond_goto(Arg::Const(Const::Bool(true)), 1, 2)]),
        (1, vec![Inst::var_assign(0, Arg::Const(Const::Str("a".to_string())))]),
        (2, vec![Inst::var_assign(0, Arg::Const(Const::Str("π".to_string())))]),
        (3, vec![]),
      ],
      &[(0, 1), (0, 2), (1, 3), (2, 3)],
    );
    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(3, 0), StringEncoding::Unknown);
  }

  #[test]
  fn annotate_cfg_encoding_sets_inst_meta() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![Inst::var_assign(
          0,
          Arg::Const(Const::Str("hello".to_string())),
        )],
      )],
      &[],
    );

    let result = analyze_cfg_encoding(&cfg);
    annotate_cfg_encoding(&mut cfg, &result);

    let inst = &cfg.bblocks.get(0)[0];
    assert_eq!(
      inst.meta.result_type.string_encoding,
      Some(StringEncoding::Ascii)
    );
  }

  #[test]
  fn annotate_cfg_encoding_uses_per_instruction_states() {
    let mut cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::var_assign(0, Arg::Const(Const::Str("hello".to_string()))),
          Inst::var_assign(0, Arg::Const(Const::Str("π".to_string()))),
        ],
      )],
      &[],
    );

    let result = analyze_cfg_encoding(&cfg);
    annotate_cfg_encoding(&mut cfg, &result);

    assert_eq!(
      cfg.bblocks.get(0)[0].meta.result_type.string_encoding,
      Some(StringEncoding::Ascii)
    );
    assert_eq!(
      cfg.bblocks.get(0)[1].meta.result_type.string_encoding,
      Some(StringEncoding::Utf8)
    );
  }

  #[test]
  fn template_call_tracks_constant_encoding() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![Inst::call(
          0,
          Arg::Builtin("__optimize_js_template".to_string()),
          Arg::Const(Const::Undefined),
          vec![Arg::Const(Const::Str("hello".to_string()))],
          Vec::new(),
        )],
      )],
      &[],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_exit(0, 0), StringEncoding::Ascii);
  }

  #[test]
  fn template_call_with_unknown_expr_is_unknown() {
    let cfg = cfg_with_blocks(
      &[(
        0,
        vec![
          Inst::unknown_load(1, "x".to_string()),
          Inst::call(
            0,
            Arg::Builtin("__optimize_js_template".to_string()),
            Arg::Const(Const::Undefined),
            vec![
              Arg::Const(Const::Str("a".to_string())),
              Arg::Var(1),
              Arg::Const(Const::Str("b".to_string())),
            ],
            Vec::new(),
          ),
        ],
      )],
      &[],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_exit(0, 0), StringEncoding::Unknown);
  }

  #[test]
  fn phi_joins_encodings() {
    let mut phi = Inst::phi_empty(3);
    // `InstTyp::Phi` uses `Arg` inputs because earlier optimizations (e.g. const
    // propagation) can fold phi operands into constants.
    phi.insert_phi(1, Arg::Const(Const::Str("a".to_string())));
    phi.insert_phi(2, Arg::Const(Const::Str("b".to_string())));

    let cfg = cfg_with_blocks(
      &[
        (0, vec![Inst::cond_goto(Arg::Const(Const::Bool(true)), 1, 2)]),
        (1, vec![Inst::goto(3)]),
        (2, vec![Inst::goto(3)]),
        (3, vec![phi]),
      ],
      &[(0, 1), (0, 2), (1, 3), (2, 3)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_exit(3, 3), StringEncoding::Ascii);
  }
}
