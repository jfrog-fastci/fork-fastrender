use crate::analysis::purity::Purity;
use crate::symbol::semantics::SymbolId;
use crate::types::{TypeId, ValueTypeSummary};
use hir_js::ExprId;
use effect_model::EffectSummary;
use num_bigint::BigInt;
use parse_js::num::JsNumber;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::{self};

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub enum EffectLocation {
  /// Heap memory reachable via object/array properties.
  Heap,
  /// Variables captured from an ancestor scope (including the global scope when in module/script
  /// mode).
  Foreign(SymbolId),
  /// Unknown/global identifier accesses in global mode.
  Unknown(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct EffectSet {
  pub reads: BTreeSet<EffectLocation>,
  pub writes: BTreeSet<EffectLocation>,
  pub summary: EffectSummary,
  pub unknown: bool,
}

impl EffectSet {
  pub fn is_default(&self) -> bool {
    self.reads.is_empty()
      && self.writes.is_empty()
      && self.summary.is_pure()
      && !self.unknown
  }

  pub fn is_pure(&self) -> bool {
    self.is_default()
  }

  pub fn mark_unknown(&mut self) {
    self.unknown = true;
    self.summary = EffectSummary::UNKNOWN;
  }

  pub fn merge(&mut self, other: &Self) {
    self.reads.extend(other.reads.iter().cloned());
    self.writes.extend(other.writes.iter().cloned());
    self.summary = EffectSummary::join(self.summary, other.summary);
    if other.unknown {
      self.mark_unknown();
    }
  }

  pub fn union(mut self, other: Self) -> Self {
    self.merge(&other);
    self
  }
}

/// Best-effort string encoding classification for compile-time-known string
/// values.
///
/// This is intentionally conservative and only used when we can prove that a
/// particular SSA value is a string constant (or derived from string constants).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum StringEncoding {
  Ascii,
  Latin1,
  Utf8,
  Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct TypeInfo {
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub string_encoding: Option<StringEncoding>,
}

impl TypeInfo {
  pub fn is_default(&self) -> bool {
    self.string_encoding.is_none()
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum OwnershipState {
  Owned,
  Borrowed,
  Shared,
  Unknown,
}

/// Ownership use-mode for an instruction argument.
///
/// This is intentionally defined in the IL layer (instead of the analysis layer)
/// so downstream backends can consume it directly from [`InstMeta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ArgUseMode {
  Borrow,
  Consume,
}

impl Default for ArgUseMode {
  fn default() -> Self {
    Self::Borrow
  }
}

fn is_default_arg_use_modes(modes: &[ArgUseMode]) -> bool {
  modes.is_empty() || modes.iter().all(|m| matches!(m, ArgUseMode::Borrow))
}

impl Default for OwnershipState {
  fn default() -> Self {
    Self::Unknown
  }
}

impl OwnershipState {
  pub fn is_default(&self) -> bool {
    matches!(self, Self::Unknown)
  }

  pub fn join(self, other: Self) -> Self {
    use OwnershipState::*;
    // Deterministic "worse wins" join.
    match (self, other) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (Shared, _) | (_, Shared) => Shared,
      (Borrowed, _) | (_, Borrowed) => Borrowed,
      (Owned, Owned) => Owned,
    }
  }
}

/// Hints for downstream lowering that can implement certain ownership transfers
/// without a clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum InPlaceHint {
  /// This `VarAssign` is a move of an owned value and can be implemented as a transfer/no-clone in
  /// downstream lowering.
  MoveNoClone { src: u32, tgt: u32 },
}

/// Branch-local nullability information derived from comparisons against `null`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Nullability {
  #[default]
  Unknown,
  Nullish,
  NonNullish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NullabilityNarrowing {
  pub var: u32,
  pub when_true: Nullability,
  pub when_false: Nullability,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct InstMeta {
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "EffectSet::is_default")
  )]
  pub effects: EffectSet,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "TypeInfo::is_default")
  )]
  pub result_type: TypeInfo,
  #[cfg_attr(
    feature = "serde",
    serde(
      default,
      skip_serializing_if = "Option::is_none",
      serialize_with = "serialize_type_id"
    )
  )]
  pub type_id: Option<TypeId>,
  #[cfg_attr(
    feature = "serde",
    serde(
      default,
      skip_serializing_if = "Option::is_none",
      serialize_with = "serialize_expr_id"
    )
  )]
  pub hir_expr: Option<ExprId>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub type_summary: Option<ValueTypeSummary>,
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
  pub excludes_nullish: bool,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "OwnershipState::is_default")
  )]
  pub ownership: OwnershipState,
  /// Per-argument ownership use modes for this instruction.
  ///
  /// Convention: an empty vector means all args are [`ArgUseMode::Borrow`]. This keeps the IL
  /// compact for the common case; the vector is only populated when at least one argument is
  /// [`ArgUseMode::Consume`].
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "is_default_arg_use_modes")
  )]
  pub arg_use_modes: Vec<ArgUseMode>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub in_place_hint: Option<InPlaceHint>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub result_escape: Option<crate::analysis::escape::EscapeState>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "crate::analysis::purity::is_default_purity")
  )]
  pub callee_purity: Purity,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "Option::is_none")
  )]
  pub nullability_narrowing: Option<NullabilityNarrowing>,
}

impl InstMeta {
  pub fn set_type_id(&mut self, type_id: Option<TypeId>) {
    self.type_id = type_id;
  }

  pub fn clear_type_id(&mut self) {
    self.type_id = None;
  }

  pub fn is_default(&self) -> bool {
    self.effects.is_default()
      && self.result_type.is_default()
      && self.type_id.is_none()
      && self.hir_expr.is_none()
      && self.type_summary.is_none()
      && !self.excludes_nullish
      && self.ownership.is_default()
      && is_default_arg_use_modes(&self.arg_use_modes)
      && self.in_place_hint.is_none()
      && self.result_escape.is_none()
      && crate::analysis::purity::is_default_purity(&self.callee_purity)
      && self.nullability_narrowing.is_none()
  }

  pub fn is_pure(&self) -> bool {
    self.effects.is_pure()
  }
}

impl Default for InstMeta {
  fn default() -> Self {
    Self {
      effects: EffectSet::default(),
      result_type: TypeInfo::default(),
      type_id: None,
      hir_expr: None,
      type_summary: None,
      excludes_nullish: false,
      ownership: OwnershipState::default(),
      arg_use_modes: Vec::new(),
      in_place_hint: None,
      result_escape: None,
      callee_purity: Purity::Impure,
      nullability_narrowing: None,
    }
  }
}

#[cfg(feature = "serde")]
fn serialize_type_id<S>(value: &Option<TypeId>, serializer: S) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  use serde::Serialize;
  #[cfg(feature = "typed")]
  {
    value.map(|id| id.0).serialize(serializer)
  }
  #[cfg(not(feature = "typed"))]
  {
    value.serialize(serializer)
  }
}

#[cfg(feature = "serde")]
fn serialize_expr_id<S>(value: &Option<ExprId>, serializer: S) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  use serde::Serialize;
  value.map(|id| id.0).serialize(serializer)
}

#[cfg(feature = "serde")]
fn is_false(value: &bool) -> bool {
  !*value
}

// PartialOrd and Ord are for some arbitrary canonical order, even if semantics of ordering is opaque.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Const {
  BigInt(BigInt),
  Bool(bool),
  Null,
  Num(JsNumber),
  Str(String),
  Undefined,
}

impl Debug for Const {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::BigInt(v) => write!(f, "{v}"),
      Self::Bool(v) => write!(f, "{v}"),
      Self::Null => write!(f, "null"),
      Self::Num(v) => write!(f, "{v}"),
      Self::Str(v) => write!(f, "'{v}'"),
      Self::Undefined => write!(f, "undefined"),
    }
  }
}

// PartialOrd and Ord are for some arbitrary canonical order, even if semantics of ordering are opaque.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Arg {
  Builtin(String), // The value is a path (e.g. `Array.prototype.forEach`). Using a single string makes it easier to match.
  Const(Const),
  Fn(usize), // The value is a function index. Functions are immutable so are similar to Const rather than an inst to load it.
  Var(u32),
}

impl Arg {
  pub fn maybe_var(&self) -> Option<u32> {
    match self {
      Arg::Var(n) => Some(*n),
      _ => None,
    }
  }

  pub fn to_var(&self) -> u32 {
    self.maybe_var().expect("not a variable")
  }

  pub fn to_const(&self) -> Const {
    match self {
      Arg::Const(c) => c.clone(),
      _ => panic!("not a constant"),
    }
  }
}

impl Debug for Arg {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Builtin(p) => write!(f, "{p}"),
      Self::Const(v) => write!(f, "{v:?}"),
      Self::Fn(n) => write!(f, "Fn{n}"),
      Self::Var(n) => write!(f, "%{n}"),
    }
  }
}

/// These must all be pure; impure operations (e.g. prop assign) are separate insts.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum BinOp {
  Add,
  BitAnd,
  BitOr,
  BitXor,
  Div, // Divide.
  Exp, // Exponentiate.
  Geq, // Greater than or equals to.
  GetProp,
  Gt,  // Greater than.
  Leq, // Less than or equals to.
  LooseEq,
  Lt,  // Less than.
  Mod, // Modulo.
  Mul, // Multiply.
  NotLooseEq,
  NotStrictEq,
  Shl,  // Shift left.
  Shr,  // Shift right.
  UShr, // Unsigned shift right.
  StrictEq,
  Sub, // Subtract.
  _Dummy,
}

impl Debug for BinOp {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Add => write!(f, "+"),
      Self::BitAnd => write!(f, "&"),
      Self::BitOr => write!(f, "|"),
      Self::BitXor => write!(f, "^"),
      Self::Div => write!(f, "/"),
      Self::Exp => write!(f, "**"),
      Self::Geq => write!(f, ">="),
      Self::GetProp => write!(f, "."),
      Self::Gt => write!(f, ">"),
      Self::Leq => write!(f, "<="),
      Self::LooseEq => write!(f, "=="),
      Self::Lt => write!(f, "<"),
      Self::Mod => write!(f, "%"),
      Self::Mul => write!(f, "*"),
      Self::NotLooseEq => write!(f, "!="),
      Self::NotStrictEq => write!(f, "!=="),
      Self::Shl => write!(f, "<<"),
      Self::Shr => write!(f, ">>"),
      Self::UShr => write!(f, ">>>"),
      Self::StrictEq => write!(f, "==="),
      Self::Sub => write!(f, "-"),
      Self::_Dummy => write!(f, "_DUMMY"),
    }
  }
}

/// These must all be pure; impure operations (e.g. prop assign) are separate insts.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum UnOp {
  BitNot,
  Neg,
  Not,
  Plus,
  Typeof,
  Void,
  _Dummy,
}

impl Debug for UnOp {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::BitNot => write!(f, "~"),
      Self::Neg => write!(f, "-"),
      Self::Not => write!(f, "!"),
      Self::Plus => write!(f, "+"),
      Self::Typeof => write!(f, "typeof"),
      Self::Void => write!(f, "void"),
      Self::_Dummy => write!(f, "_DUMMY"),
    }
  }
}

#[derive(PartialEq, Eq, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum InstTyp {
  Bin,        // tgts[0] = args[0] bin_op args[1]
  Un,         // tgts[0] = un_op args[0]
  VarAssign,  // tgts[0] = args[0]
  PropAssign, // args[0][args[1]] = args[2]
  CondGoto,   // goto labels[0] if args[0] else labels[1]
  /// Return from the current body (function).
  ///
  /// If `args` is empty, the return value is implicitly `undefined`; otherwise `args[0]` is the
  /// returned value.
  Return,
  /// Throw from the current body (function or top-level). `args[0]` is the thrown value.
  Throw,
  Call,       // tgts.at(0)? = args[0](this=args[1], ...args[2..])
  // A foreign variable is one in an ancestor scope, all the way up to and including the global scope.
  // We don't simply add another Target variant (e.g. Target::Foreign) as it makes analyses and optimisations more tedious. Consider that standard SSA doesn't really have a concept of nonlocal memory locations. In LLVM such vars are covered using ordinary memory location read/write instructions.
  // NOTE: It still violates SSA if we only have ForeignStore but not ForeignLoad (and instead use another enum variant for Arg). Consider: `%a0 = foreign(3); %a1 = %a0 + 42; foreign(3) = %a1; %a2 = foreign(3);` but `%a0` and `%a2` are not identical.
  ForeignLoad,  // tgts[0] = foreign
  ForeignStore, // foreign = args[0]
  UnknownLoad,  // tgts[0] = unknown
  UnknownStore, // unknown = args[0]
  // Pick one assigned value of `tgt` from one of these blocks. Due to const propagation, input targets could be transformed to const values, which is why we have `Arg` and not just `Target`.
  Phi, // tgts[0] = ϕ{labels[0]: args[0], labels[1]: args[1], ...}
  // No-op marker for a position in Vec<Inst> during source_to_inst. We can't just use indices as we may reorder and splice the instructions during optimisations.
  _Label, // labels[0]
  // We only want these during source_to_inst. Afterwards, refer to the graph children; otherwise, it's two separate things we have to keep in sync and check.
  _Goto, // labels[0]
  _Dummy,
}

#[cfg(feature = "serde")]
fn is_dummy_binop(op: &BinOp) -> bool {
  matches!(op, BinOp::_Dummy)
}

#[cfg(feature = "serde")]
fn is_dummy_unop(op: &UnOp) -> bool {
  matches!(op, UnOp::_Dummy)
}

#[cfg(feature = "serde")]
fn is_dummy_symbol(sym: &SymbolId) -> bool {
  sym.raw_id() == u32::MAX as u64
}

fn dummy_symbol() -> SymbolId {
  SymbolId(u32::MAX as u64)
}

#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Inst {
  pub t: InstTyp,
  pub tgts: Vec<u32>,
  pub args: Vec<Arg>,
  pub spreads: Vec<usize>, // Indices into `args` that are spread, for Call. Cannot have values less than 2 as the first two args are `callee` and `this`.
  pub labels: Vec<u32>,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "InstMeta::is_default")
  )]
  pub meta: InstMeta,
  // Garbage values if not applicable.
  #[cfg_attr(
    feature = "serde",
    serde(
      default = "BinOp::_Unreachable",
      skip_serializing_if = "is_dummy_binop"
    )
  )]
  pub bin_op: BinOp,
  #[cfg_attr(
    feature = "serde",
    serde(default = "UnOp::_Unreachable", skip_serializing_if = "is_dummy_unop")
  )]
  pub un_op: UnOp,
  #[cfg_attr(
    feature = "serde",
    serde(default = "dummy_symbol", skip_serializing_if = "is_dummy_symbol")
  )]
  pub foreign: SymbolId,
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "String::is_empty")
  )]
  pub unknown: String,
}

impl PartialEq for Inst {
  fn eq(&self, other: &Self) -> bool {
    self.t == other.t
      && self.tgts == other.tgts
      && self.args == other.args
      && self.spreads == other.spreads
      && self.labels == other.labels
      && self.bin_op == other.bin_op
      && self.un_op == other.un_op
      && self.foreign == other.foreign
      && self.unknown == other.unknown
  }
}

impl Eq for Inst {}

impl Debug for Inst {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    // Keep the debug output stable (tests assert on it) by not including `meta`.
    f.debug_struct("Inst")
      .field("t", &self.t)
      .field("tgts", &self.tgts)
      .field("args", &self.args)
      .field("spreads", &self.spreads)
      .field("labels", &self.labels)
      .field("bin_op", &self.bin_op)
      .field("un_op", &self.un_op)
      .field("foreign", &self.foreign)
      .field("unknown", &self.unknown)
      .finish()
  }
}

impl Inst {
  pub fn remove_phi(&mut self, label: u32) -> Option<Arg> {
    assert!(self.t == InstTyp::Phi);
    assert_eq!(self.labels.len(), self.args.len());
    let i = self.labels.iter().position(|&l| l == label)?;
    self.labels.remove(i);
    Some(self.args.remove(i))
  }

  pub fn insert_phi(&mut self, label: u32, arg: Arg) {
    assert!(self.t == InstTyp::Phi);
    assert_eq!(self.labels.len(), self.args.len());
    // This catches a lot of bugs.
    assert!(
      !self.labels.contains(&label),
      "can't insert {label}=>{arg:?} to {self:?}"
    );
    self.labels.push(label);
    self.args.push(arg);
  }
}

impl Default for Inst {
  fn default() -> Self {
    Self {
      t: InstTyp::_Dummy,
      tgts: Default::default(),
      args: Default::default(),
      spreads: Default::default(),
      labels: Default::default(),
      meta: Default::default(),
      bin_op: BinOp::_Dummy,
      un_op: UnOp::_Dummy,
      foreign: dummy_symbol(),
      unknown: Default::default(),
    }
  }
}

/// Convenient builders for Inst.
impl Inst {
  pub fn bin(tgt: u32, left: Arg, op: BinOp, right: Arg) -> Self {
    Self {
      t: InstTyp::Bin,
      tgts: vec![tgt],
      args: vec![left, right],
      bin_op: op,
      ..Default::default()
    }
  }

  pub fn un(tgt: u32, op: UnOp, arg: Arg) -> Self {
    Self {
      t: InstTyp::Un,
      tgts: vec![tgt],
      args: vec![arg],
      un_op: op,
      ..Default::default()
    }
  }

  pub fn var_assign(tgt: u32, arg: Arg) -> Self {
    Self {
      t: InstTyp::VarAssign,
      tgts: vec![tgt],
      args: vec![arg],
      ..Default::default()
    }
  }

  pub fn prop_assign(obj: Arg, prop: Arg, val: Arg) -> Self {
    Self {
      t: InstTyp::PropAssign,
      args: vec![obj, prop, val],
      ..Default::default()
    }
  }

  pub fn goto(label: u32) -> Self {
    Self {
      t: InstTyp::_Goto,
      labels: vec![label],
      ..Default::default()
    }
  }

  pub fn cond_goto(cond: Arg, t: u32, f: u32) -> Self {
    Self {
      t: InstTyp::CondGoto,
      args: vec![cond],
      labels: vec![t, f],
      ..Default::default()
    }
  }

  pub fn ret(value: Option<Arg>) -> Self {
    Self {
      t: InstTyp::Return,
      args: value.into_iter().collect(),
      ..Default::default()
    }
  }

  pub fn throw(value: Arg) -> Self {
    Self {
      t: InstTyp::Throw,
      args: vec![value],
      ..Default::default()
    }
  }

  pub fn call(
    tgt: impl Into<Option<u32>>,
    callee: Arg,
    this: Arg,
    args: Vec<Arg>,
    spreads: Vec<usize>,
  ) -> Self {
    let total_args_len = args.len() + 2;
    assert!(spreads.iter().all(|&i| i >= 2 && i < total_args_len));
    Self {
      t: InstTyp::Call,
      tgts: tgt.into().into_iter().collect(),
      args: [callee, this].into_iter().chain(args).collect(),
      spreads,
      ..Default::default()
    }
  }

  pub fn foreign_load(tgt: u32, foreign: SymbolId) -> Self {
    Self {
      t: InstTyp::ForeignLoad,
      tgts: vec![tgt],
      foreign,
      ..Default::default()
    }
  }

  pub fn foreign_store(foreign: SymbolId, arg: Arg) -> Self {
    Self {
      t: InstTyp::ForeignStore,
      args: vec![arg],
      foreign,
      ..Default::default()
    }
  }

  pub fn unknown_load(tgt: u32, unknown: String) -> Self {
    Self {
      t: InstTyp::UnknownLoad,
      tgts: vec![tgt],
      unknown,
      ..Default::default()
    }
  }

  pub fn unknown_store(unknown: String, arg: Arg) -> Self {
    Self {
      t: InstTyp::UnknownStore,
      args: vec![arg],
      unknown,
      ..Default::default()
    }
  }

  /// Use .insert_phi() to add more labels and args.
  pub fn phi_empty(tgt: u32) -> Self {
    Self {
      t: InstTyp::Phi,
      tgts: vec![tgt],
      ..Default::default()
    }
  }

  pub fn label(label: u32) -> Self {
    Self {
      t: InstTyp::_Label,
      labels: vec![label],
      ..Default::default()
    }
  }
}

/// Convenient component getters for Inst.
impl Inst {
  pub fn as_bin(&self) -> (u32, &Arg, BinOp, &Arg) {
    assert_eq!(self.t, InstTyp::Bin);
    (self.tgts[0], &self.args[0], self.bin_op, &self.args[1])
  }

  pub fn as_un(&self) -> (u32, UnOp, &Arg) {
    assert_eq!(self.t, InstTyp::Un);
    (self.tgts[0], self.un_op, &self.args[0])
  }

  pub fn as_var_assign(&self) -> (u32, &Arg) {
    assert_eq!(self.t, InstTyp::VarAssign);
    (self.tgts[0], &self.args[0])
  }

  pub fn as_prop_assign(&self) -> (&Arg, &Arg, &Arg) {
    assert_eq!(self.t, InstTyp::PropAssign);
    (&self.args[0], &self.args[1], &self.args[2])
  }

  pub fn as_cond_goto(&self) -> (&Arg, u32, u32) {
    assert_eq!(self.t, InstTyp::CondGoto);
    (&self.args[0], self.labels[0], self.labels[1])
  }

  pub fn as_return(&self) -> Option<&Arg> {
    assert_eq!(self.t, InstTyp::Return);
    self.args.get(0)
  }

  pub fn as_throw(&self) -> &Arg {
    assert_eq!(self.t, InstTyp::Throw);
    &self.args[0]
  }

  pub fn as_call(&self) -> (Option<u32>, &Arg, &Arg, &[Arg], &[usize]) {
    assert_eq!(self.t, InstTyp::Call);
    (
      self.tgts.get(0).copied(),
      &self.args[0],
      &self.args[1],
      &self.args[2..],
      &self.spreads,
    )
  }

  pub fn as_foreign_load(&self) -> (u32, SymbolId) {
    assert_eq!(self.t, InstTyp::ForeignLoad);
    (self.tgts[0], self.foreign)
  }

  pub fn as_foreign_store(&self) -> (SymbolId, &Arg) {
    assert_eq!(self.t, InstTyp::ForeignStore);
    (self.foreign, &self.args[0])
  }

  pub fn as_unknown_load(&self) -> (u32, &String) {
    assert_eq!(self.t, InstTyp::UnknownLoad);
    (self.tgts[0], &self.unknown)
  }

  pub fn as_unknown_store(&self) -> (&String, &Arg) {
    assert_eq!(self.t, InstTyp::UnknownStore);
    (&self.unknown, &self.args[0])
  }
}
