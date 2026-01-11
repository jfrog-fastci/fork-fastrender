use crate::analysis::dataflow::{
  AnalysisBoundary, BlockState, DataFlowAnalysis, DataFlowResult, Direction,
  ResolvedAnalysisBoundary,
};
use crate::analysis::facts::{replay_forward_after_inst, replay_forward_before_inst, Edge, InstLoc};
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, Const, Inst, InstTyp, StringEncoding, UnOp};
use ahash::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EncodingState {
  reachable: bool,
  vars: HashMap<u32, StringEncoding>,
}

impl EncodingState {
  fn bottom() -> Self {
    Self {
      reachable: false,
      vars: HashMap::default(),
    }
  }

  fn entry() -> Self {
    Self {
      reachable: true,
      vars: HashMap::default(),
    }
  }

  fn get(&self, var: u32) -> StringEncoding {
    if !self.reachable {
      return StringEncoding::Unknown;
    }
    self.vars.get(&var).copied().unwrap_or(StringEncoding::Unknown)
  }

  fn set(&mut self, var: u32, enc: StringEncoding) {
    if !self.reachable {
      return;
    }
    self.vars.insert(var, enc);
  }

  fn join_with(&mut self, other: &Self) {
    if !other.reachable {
      return;
    }
    if !self.reachable {
      *self = other.clone();
      return;
    }
    for (&var, &enc_other) in other.vars.iter() {
      let enc = join_encoding(self.vars.get(&var).copied().unwrap_or(enc_other), enc_other);
      self.vars.insert(var, enc);
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EncodingResult {
  pub boundary: ResolvedAnalysisBoundary,
  pub blocks: HashMap<u32, BlockState<EncodingState>>,
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
      .map(|state| state.get(var))
      .unwrap_or(StringEncoding::Unknown)
  }

  pub fn encoding_at_exit(&self, label: u32, var: u32) -> StringEncoding {
    self
      .block_exit(label)
      .map(|state| state.get(var))
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
      .unwrap_or_else(EncodingState::bottom);
    let mut analysis = EncodingAnalysis;
    replay_forward_before_inst(cfg, label, &entry, inst_idx, |label, inst_idx, inst, state| {
      analysis.apply_to_instruction(label, inst_idx, inst, state);
    })
  }

  /// Compute the analysis state immediately after `inst_idx` in `label`.
  pub fn state_after_inst(&self, cfg: &Cfg, label: u32, inst_idx: usize) -> EncodingState {
    let entry = self
      .block_entry(label)
      .cloned()
      .unwrap_or_else(EncodingState::bottom);
    let mut analysis = EncodingAnalysis;
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
    let _ = self;
    let analysis = EncodingAnalysis;
    analysis.encoding_of_arg(state, arg)
  }
}

struct EncodingAnalysis;

fn encoding_for_str(value: &str) -> StringEncoding {
  if value.is_ascii() {
    return StringEncoding::Ascii;
  }
  if value.chars().all(|ch| (ch as u32) <= 0xff) {
    return StringEncoding::Latin1;
  }
  StringEncoding::Utf8
}

fn encoding_for_template_arg(state: &EncodingState, arg: &Arg) -> StringEncoding {
  match arg {
    Arg::Const(Const::Str(s)) => encoding_for_str(s),
    // Template literals coerce non-string primitives to string via `ToString`,
    // which is guaranteed to use ASCII for these primitives.
    Arg::Const(
      Const::BigInt(_) | Const::Bool(_) | Const::Null | Const::Num(_) | Const::Undefined,
    ) => StringEncoding::Ascii,
    Arg::Var(var) => state.get(*var),
    _ => StringEncoding::Unknown,
  }
}

fn join_encoding(a: StringEncoding, b: StringEncoding) -> StringEncoding {
  use StringEncoding::*;
  match (a, b) {
    (Unknown, _) | (_, Unknown) => Unknown,
    (Utf8, _) | (_, Utf8) => Utf8,
    (Latin1, _) | (_, Latin1) => Latin1,
    (Ascii, Ascii) => Ascii,
  }
}

impl EncodingAnalysis {
  fn encoding_of_arg(&self, state: &EncodingState, arg: &Arg) -> StringEncoding {
    match arg {
      Arg::Const(Const::Str(s)) => encoding_for_str(s),
      Arg::Var(var) => state.get(*var),
      _ => StringEncoding::Unknown,
    }
  }

  fn encoding_for_bin_add(&self, state: &EncodingState, left: &Arg, right: &Arg) -> StringEncoding {
    let left_enc = self.encoding_of_arg(state, left);
    let right_enc = self.encoding_of_arg(state, right);
    let is_string_concat = matches!(left, Arg::Const(Const::Str(_)))
      || matches!(right, Arg::Const(Const::Str(_)))
      || (left_enc != StringEncoding::Unknown)
      || (right_enc != StringEncoding::Unknown);
    if !is_string_concat {
      return StringEncoding::Unknown;
    }
    // If one side is definitely a string literal / tracked string, `+` performs
    // string concatenation and coerces the other operand via `ToString`.
    join_encoding(
      encoding_for_template_arg(state, left),
      encoding_for_template_arg(state, right),
    )
  }

  fn encoding_for_template_call(&self, state: &EncodingState, args: &[Arg]) -> StringEncoding {
    let mut acc: Option<StringEncoding> = None;
    for arg in args {
      let enc = encoding_for_template_arg(state, arg);
      acc = Some(match acc {
        None => enc,
        Some(existing) => join_encoding(existing, enc),
      });
      if matches!(acc, Some(StringEncoding::Unknown)) {
        break;
      }
    }
    // The lowering always supplies at least the template head string, but default
    // to `Unknown` if we somehow see an empty arg list.
    acc.unwrap_or(StringEncoding::Unknown)
  }
}

impl DataFlowAnalysis for EncodingAnalysis {
  type State = EncodingState;

  const DIRECTION: Direction = Direction::Forward;

  fn bottom(&self, _cfg: &Cfg) -> Self::State {
    EncodingState::bottom()
  }

  fn boundary_state(&self, boundary: &ResolvedAnalysisBoundary, _cfg: &Cfg) -> Self::State {
    match boundary {
      ResolvedAnalysisBoundary::Entry(_) => EncodingState::entry(),
      ResolvedAnalysisBoundary::VirtualExit { .. } => EncodingState::bottom(),
    }
  }

  fn meet(&mut self, states: &[(u32, &Self::State)]) -> Self::State {
    if states.is_empty() {
      return EncodingState::bottom();
    }
    let mut merged = EncodingState::bottom();
    for (_, state) in states {
      merged.join_with(state);
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
    if !state.reachable {
      return;
    }

    match inst.t {
      InstTyp::VarAssign => {
        let (tgt, arg) = inst.as_var_assign();
        let enc = self.encoding_of_arg(state, arg);
        state.set(tgt, enc);
      }
      InstTyp::Phi => {
        let tgt = inst.tgts[0];
        let mut acc: Option<StringEncoding> = None;
        for arg in inst.args.iter() {
          let enc = self.encoding_of_arg(state, arg);
          acc = Some(match acc {
            None => enc,
            Some(existing) => join_encoding(existing, enc),
          });
        }
        state.set(tgt, acc.unwrap_or(StringEncoding::Unknown));
      }
      InstTyp::Un => {
        let (tgt, op, _arg) = inst.as_un();
        let enc = match op {
          UnOp::Typeof => StringEncoding::Ascii,
          _ => StringEncoding::Unknown,
        };
        state.set(tgt, enc);
      }
      InstTyp::Bin => {
        let (tgt, left, op, right) = inst.as_bin();
        let enc = match op {
          BinOp::Add => self.encoding_for_bin_add(state, left, right),
          _ => StringEncoding::Unknown,
        };
        state.set(tgt, enc);
      }
      InstTyp::Call => {
        let Some(tgt) = inst.tgts.get(0).copied() else {
          return;
        };
        let (_, callee, _, args, _) = inst.as_call();
        let enc = match callee {
          Arg::Builtin(path) if path == "__optimize_js_template" => {
            self.encoding_for_template_call(state, args)
          }
          _ => StringEncoding::Unknown,
        };
        state.set(tgt, enc);
      }
      InstTyp::ForeignLoad | InstTyp::UnknownLoad => {
        let tgt = inst.tgts[0];
        state.set(tgt, StringEncoding::Unknown);
      }
      _ => {
        for &tgt in inst.tgts.iter() {
          state.set(tgt, StringEncoding::Unknown);
        }
      }
    };
  }
}

pub fn analyze_cfg_encoding(cfg: &Cfg) -> EncodingResult {
  let mut analysis = EncodingAnalysis;
  let DataFlowResult { boundary, blocks } =
    analysis.analyze(cfg, AnalysisBoundary::Entry(cfg.entry));
  EncodingResult { boundary, blocks }
}

pub fn annotate_cfg_encoding(cfg: &mut Cfg, result: &EncodingResult) {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  for label in labels {
    let Some(block_state) = result.blocks.get(&label) else {
      continue;
    };
    let mut analysis = EncodingAnalysis;
    let mut state = block_state.entry.clone();

    let insts_len = cfg.bblocks.get(label).len();
    for inst_idx in 0..insts_len {
      let encoding_for_inst = {
        let inst = &cfg.bblocks.get(label)[inst_idx];
        analysis.apply_to_instruction(label, inst_idx, inst, &mut state);
        inst.tgts.get(0).copied().map(|tgt| (tgt, state.get(tgt)))
      };

      let Some((_tgt, enc)) = encoding_for_inst else {
        continue;
      };
      if matches!(enc, StringEncoding::Ascii | StringEncoding::Latin1 | StringEncoding::Utf8) {
        cfg.bblocks.get_mut(label)[inst_idx].meta.result_type.string_encoding = Some(enc);
      }
    }
  }
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
  fn detects_latin1_literals() {
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
    assert_eq!(result.encoding_at_entry(1, 0), StringEncoding::Latin1);
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
  fn concatenation_with_latin1_literal_is_latin1() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![
            Inst::var_assign(0, Arg::Const(Const::Str("a".to_string()))),
            Inst::var_assign(1, Arg::Const(Const::Str("ÿ".to_string()))),
            Inst::bin(2, Arg::Var(0), BinOp::Add, Arg::Var(1)),
          ],
        ),
        (1, vec![]),
      ],
      &[(0, 1)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_entry(1, 2), StringEncoding::Latin1);
  }

  #[test]
  fn concatenation_with_bool_literal_is_ascii() {
    let cfg = cfg_with_blocks(
      &[
        (
          0,
          vec![Inst::bin(
            0,
            Arg::Const(Const::Str("a".to_string())),
            BinOp::Add,
            Arg::Const(Const::Bool(true)),
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
    phi.insert_phi(1, Arg::Var(1));
    phi.insert_phi(2, Arg::Var(2));

    let cfg = cfg_with_blocks(
      &[
        (0, vec![Inst::cond_goto(Arg::Const(Const::Bool(true)), 1, 2)]),
        (
          1,
          vec![
            Inst::var_assign(1, Arg::Const(Const::Str("a".to_string()))),
            Inst::goto(3),
          ],
        ),
        (
          2,
          vec![
            Inst::var_assign(2, Arg::Const(Const::Str("b".to_string()))),
            Inst::goto(3),
          ],
        ),
        (3, vec![phi]),
      ],
      &[(0, 1), (0, 2), (1, 3), (2, 3)],
    );

    let result = analyze_cfg_encoding(&cfg);
    assert_eq!(result.encoding_at_exit(3, 3), StringEncoding::Ascii);
  }
}
