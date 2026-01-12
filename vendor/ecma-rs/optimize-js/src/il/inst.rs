use crate::symbol::semantics::SymbolId;
#[cfg(feature = "semantic-ops")]
use hir_js::ApiId;
use num_bigint::BigInt;
use parse_js::num::JsNumber;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::{self};

pub use crate::il::meta::{
  ArgUseMode, ArrayElemRepr, AwaitBehavior, EffectLocation, EffectSet, FieldAccessMeta, InPlaceHint,
  InstMeta, Nullability, NullabilityNarrowing, NumericRepr, OwnershipState, ParallelPlan,
  ParallelReason, Purity, StringEncoding, TypeInfo, VectorizeHint, VectorizeNoReason,
};
pub use crate::types::ValueTypeSummary;

/// Layout identifier used by native array operations.
///
/// In typed builds this is the real `types_ts_interned::LayoutId`. In untyped
/// builds we keep the field as a dummy `u128` so the core IL types continue to
/// compile without the optional `types-ts-interned` dependency.
#[cfg(feature = "typed")]
pub type ArrayElemLayoutId = types_ts_interned::LayoutId;
#[cfg(not(feature = "typed"))]
pub type ArrayElemLayoutId = u128;

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

impl ValueTypeSummary {
  pub fn from_const(c: &Const) -> Self {
    match c {
      Const::BigInt(_) => Self::BIGINT,
      Const::Bool(_) => Self::BOOLEAN,
      Const::Null => Self::NULL,
      Const::Num(_) => Self::NUMBER,
      Const::Str(_) => Self::STRING,
      Const::Undefined => Self::UNDEFINED,
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

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum FieldRef {
  Prop(String),
  TupleIndex(u32),
  /// Internal field used by compiler/runtime codegen.
  Internal(String),
  _Dummy,
}

impl Default for FieldRef {
  fn default() -> Self {
    Self::_Dummy
  }
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

// NOTE: Array chain semantic instructions are exposed under both `native-fusion` (upstream) and
// `native-array-ops` (this task). Keep the representation unified so enabling both features does
// not introduce duplicate types or match arms.

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

#[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum ArrayChainOp {
  Map { callback: usize },
  Filter { callback: usize },
  Reduce { callback: usize, init: Option<usize> },
  Find { callback: usize },
  Every { callback: usize },
  Some { callback: usize },
}

#[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArrayChainOpData {
  Map { callback: Arg },
  Filter { callback: Arg },
  Reduce { callback: Arg, init: Option<Arg> },
  Find { callback: Arg },
  Every { callback: Arg },
  Some { callback: Arg },
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum InstTyp {
  Bin,        // tgts[0] = args[0] bin_op args[1]
  Un,         // tgts[0] = un_op args[0]
  /// String concatenation / template literal interpolation as a single semantic op.
  ///
  /// `tgts[0] = concat(args...)`.
  ///
  /// Lowering may use this for template literals and typed `+` chains so downstream
  /// analyses/backends can reason about concatenation as a single allocation.
  StringConcat,
  VarAssign,  // tgts[0] = args[0]
  /// Explicit null/undefined check.
  ///
  /// Semantics:
  /// - If `args[0]` is `null` or `undefined`, traps/throws.
  /// - Otherwise, it evaluates to `args[0]` unchanged.
  ///
  /// When `tgts` is non-empty, `tgts[0] = args[0]` (i.e. the checked value is forwarded).
  /// When `tgts` is empty, this is a check-only instruction.
  NullCheck,
  FieldLoad,  // tgts[0] = load_field(args[0], field)
  FieldStore, // store_field(args[0], field, args[1])
  PropAssign, // args[0][args[1]] = args[2]
  /// Length read from a native array header.
  ///
  /// Convention: `tgts[0] = args[0].length`.
  ArrayLen,
  /// Element load from a native array.
  ///
  /// Convention: `tgts[0] = args[0][args[1]]`.
  ArrayLoad,
  /// Element store into a native array.
  ///
  /// Convention: `args[0][args[1]] = args[2]`.
  ArrayStore,
  /// Branch-local assertion/assumption used for analysis-driven optimizations.
  ///
  /// This instruction has no runtime semantics and is expected to be inserted
  /// immediately after a runtime assertion call (e.g. `assert(cond)`), so that
  /// analyses can treat `cond` as true for the remainder of the control-flow
  /// path.
  ///
  /// `args[0]` is the assumed condition.
  Assume,
  CondGoto,   // goto labels[0] if args[0] else labels[1]
  /// Return from the current body (function).
  ///
  /// If `args` is empty, the return value is implicitly `undefined`; otherwise `args[0]` is the
  /// returned value.
  Return,
  /// Throw from the current body (function or top-level). `args[0]` is the thrown value.
  Throw,
  Call,       // tgts.at(0)? = args[0](this=args[1], ...args[2..])
  /// Call with an exception edge ("invoke").
  ///
  /// Semantics:
  /// - Like [`InstTyp::Call`], but is a terminator that has two successor labels:
  ///   - `labels[0]` = normal continuation
  ///   - `labels[1]` = exception continuation
  /// - On the normal edge, `tgts.get(0)` (when present) receives the return value.
  /// - On the exception edge, control transfers to `labels[1]` and the thrown
  ///   value is made available to a [`InstTyp::Catch`] at the start of the handler.
  Invoke,
  /// Materialize the thrown value at the start of a catch/landingpad block.
  ///
  /// `tgts[0]` receives the thrown value for the edge that transferred control to
  /// this block (from an [`InstTyp::Invoke`] or a [`InstTyp::Throw`] with an
  /// exception label).
  Catch,
  #[cfg(feature = "semantic-ops")]
  /// Call to a statically-known API (identified by a stable [`hir_js::ApiId`]).
  ///
  /// Calling convention matches `Call`, except:
  /// - the callee is encoded in the instruction type (`api`)
  /// - `this` is implicitly `undefined` for now (see `hir_js::ExprKind::KnownApiCall`)
  /// - `args` contains only call arguments (no callee/this prefix)
  KnownApiCall { api: ApiId },
  /// Await a promise-like value.
  ///
  /// When `tgts` is non-empty, `tgts[0] = await(args[0])`.
  #[cfg(feature = "native-async-ops")]
  Await,
  /// `Promise.all([args...])` lowered as a first-class semantic op.
  ///
  /// When `tgts` is non-empty, `tgts[0] = Promise.all(args...)`.
  #[cfg(feature = "native-async-ops")]
  PromiseAll,
  /// `Promise.race([args...])` lowered as a first-class semantic op.
  ///
  /// When `tgts` is non-empty, `tgts[0] = Promise.race(args...)`.
  #[cfg(feature = "native-async-ops")]
  PromiseRace,
  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  /// Fused array pipeline suitable for native backends to lower as a single loop.
  ///
  /// Convention:
  /// - `args[0]` is the base array value.
  /// - Callback/init values referenced by [`ArrayChainOp`] indices are stored in `args`.
  /// - `tgts.at(0)?` is the final result (array for `map`/`filter`, scalar for `reduce`/`find`/`every`/`some`).
  ArrayChain,
  /// Array literal construction.
  ///
  /// - `tgts[0]` (when present) is the resulting array value.
  /// - `args` are the array elements.
  /// - `spreads` contains indices into `args` that should be spread.
  ///
  /// Array holes are currently encoded using the sentinel
  /// `Arg::Builtin("__optimize_js_array_hole")`.
  ArrayLit,
  /// Object literal construction.
  ///
  /// - `tgts[0]` (when present) is the resulting object value.
  /// - `args` uses the existing marker encoding:
  ///   - `Arg::Builtin("__optimize_js_object_prop")`, key, value
  ///   - `Arg::Builtin("__optimize_js_object_prop_computed")`, key_expr, value
  ///   - `Arg::Builtin("__optimize_js_object_spread")`, spread_expr, undefined
  ObjectLit,
  /// Regular expression literal construction.
  ///
  /// `args[0]` is the literal body/flags encoding (currently a string).
  RegexLit,
  /// Template literal construction (`\`a${b}c\``).
  ///
  /// `args` is the existing encoding: string segment, substitution, segment, ...
  TemplateLit,
  /// Tagged template literal (`tag\`...\``).
  ///
  /// `args[0]` is the tag expression; remaining args use the same encoding as
  /// [`InstTyp::TemplateLit`].
  TaggedTemplateLit,
  /// `new ctor(...args)`.
  ///
  /// - `args[0]` is the constructor value.
  /// - `args[1..]` are call arguments.
  /// - `spreads` contains indices into `args` (must be >= 1) that are spread.
  New,
  /// `delete obj[prop]`.
  ///
  /// `args[0]` is the object value; `args[1]` is the property key.
  Delete,
  /// `left in right`.
  In,
  /// `left instanceof right`.
  Instanceof,
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

#[cfg(feature = "serde")]
fn is_dummy_fieldref(field: &FieldRef) -> bool {
  matches!(field, FieldRef::_Dummy)
}

fn dummy_symbol() -> SymbolId {
  SymbolId(u32::MAX as u64)
}

#[cfg(feature = "typed")]
fn dummy_layout_id() -> types_ts_interned::LayoutId {
  types_ts_interned::LayoutId(u128::MAX)
}

#[cfg(not(feature = "typed"))]
fn dummy_layout_id() -> u128 {
  u128::MAX
}

#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Inst {
  pub t: InstTyp,
  pub tgts: Vec<u32>,
  pub args: Vec<Arg>,
  /// Indices into `args` that are spread.
  ///
  /// For [`InstTyp::Call`], the first two `args` entries are `callee` and
  /// `this`, so spread indices are always >= 2.
  pub spreads: Vec<usize>,
  pub labels: Vec<u32>,
  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
  pub array_chain: Vec<ArrayChainOp>,
  /// Native array element layout for `ArrayLen`/`ArrayLoad`/`ArrayStore`.
  ///
  /// Garbage value when not applicable.
  #[cfg_attr(
    feature = "serde",
    serde(
      default = "dummy_layout_id",
      skip_serializing_if = "is_dummy_layout_id",
      serialize_with = "serialize_array_elem_layout_id"
    )
  )]
  pub elem_layout: ArrayElemLayoutId,
  /// Whether this access performs a runtime bounds check.
  ///
  /// When `false`, native backends may emit unchecked memory accesses.
  ///
  /// Garbage value when not applicable.
  #[cfg_attr(
    feature = "serde",
    serde(default = "default_checked", skip_serializing_if = "is_true")
  )]
  pub checked: bool,
  #[cfg_attr(feature = "serde", serde(skip))]
  pub value_type: ValueTypeSummary,
  #[cfg_attr(feature = "serde", serde(skip))]
  pub meta: crate::il::meta::InstMeta,
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
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "is_dummy_fieldref")
  )]
  pub field: FieldRef,
}

#[cfg(feature = "serde")]
#[allow(dead_code)]
fn is_dummy_layout_id(layout: &ArrayElemLayoutId) -> bool {
  #[cfg(feature = "typed")]
  {
    layout.0 == u128::MAX
  }
  #[cfg(not(feature = "typed"))]
  {
    *layout == u128::MAX
  }
}

#[cfg(feature = "serde")]
#[allow(dead_code)]
fn serialize_array_elem_layout_id<S>(
  layout: &ArrayElemLayoutId,
  serializer: S,
) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  #[cfg(feature = "typed")]
  {
    serializer.serialize_str(&format!("0x{:032x}", layout.0))
  }
  #[cfg(not(feature = "typed"))]
  {
    serializer.serialize_str(&format!("0x{:032x}", *layout))
  }
}

#[cfg(feature = "serde")]
#[allow(dead_code)]
fn default_checked() -> bool {
  true
}

#[cfg(feature = "serde")]
#[allow(dead_code)]
fn is_true(v: &bool) -> bool {
  *v
}

impl PartialEq for Inst {
  fn eq(&self, other: &Self) -> bool {
    self.t == other.t
      && self.tgts == other.tgts
      && self.args == other.args
      && self.spreads == other.spreads
      && self.labels == other.labels
      && {
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        {
          self.array_chain == other.array_chain
        }
        #[cfg(not(any(feature = "native-fusion", feature = "native-array-ops")))]
        {
          true
        }
      }
      && self.elem_layout == other.elem_layout
      && self.checked == other.checked
      && self.bin_op == other.bin_op
      && self.un_op == other.un_op
      && self.foreign == other.foreign
      && self.unknown == other.unknown
      && self.field == other.field
  }
}

impl Eq for Inst {}

impl Debug for Inst {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    // Keep the debug output stable (tests assert on it) by not including `meta`.
    let mut s = f.debug_struct("Inst");
    s.field("t", &self.t)
      .field("tgts", &self.tgts)
      .field("args", &self.args)
      .field("spreads", &self.spreads)
      .field("labels", &self.labels);
    #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
    {
      if self.t == InstTyp::ArrayChain {
        s.field("array_chain", &self.array_chain);
      }
    }
    if matches!(
      self.t,
      InstTyp::ArrayLen | InstTyp::ArrayLoad | InstTyp::ArrayStore
    ) {
      s.field("elem_layout", &self.elem_layout)
        .field("checked", &self.checked);
    }
    s.field("bin_op", &self.bin_op)
      .field("un_op", &self.un_op)
      .field("foreign", &self.foreign)
      .field("unknown", &self.unknown);
    if !matches!(self.field, FieldRef::_Dummy) {
      s.field("field", &self.field);
    }
    s.finish()
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
      #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
      array_chain: Default::default(),
      elem_layout: dummy_layout_id(),
      checked: true,
      value_type: ValueTypeSummary::UNKNOWN,
      meta: Default::default(),
      bin_op: BinOp::_Dummy,
      un_op: UnOp::_Dummy,
      foreign: dummy_symbol(),
      unknown: Default::default(),
      field: FieldRef::_Dummy,
    }
  }
}

/// Convenient builders for Inst.
impl Inst {
  pub fn bin(tgt: u32, left: Arg, op: BinOp, right: Arg) -> Self {
    let value_type = match op {
      BinOp::Geq
      | BinOp::Gt
      | BinOp::Leq
      | BinOp::Lt
      | BinOp::LooseEq
      | BinOp::NotLooseEq
      | BinOp::NotStrictEq
      | BinOp::StrictEq => ValueTypeSummary::BOOLEAN,
      _ => ValueTypeSummary::UNKNOWN,
    };
    Self {
      t: InstTyp::Bin,
      tgts: vec![tgt],
      args: vec![left, right],
      value_type,
      bin_op: op,
      ..Default::default()
    }
  }

  pub fn un(tgt: u32, op: UnOp, arg: Arg) -> Self {
    let value_type = match op {
      UnOp::Not => ValueTypeSummary::BOOLEAN,
      UnOp::Typeof => ValueTypeSummary::STRING,
      UnOp::Void => ValueTypeSummary::UNDEFINED,
      _ => ValueTypeSummary::UNKNOWN,
    };
    Self {
      t: InstTyp::Un,
      tgts: vec![tgt],
      args: vec![arg],
      value_type,
      un_op: op,
      ..Default::default()
    }
  }

  pub fn string_concat(tgt: u32, parts: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::StringConcat,
      tgts: vec![tgt],
      args: parts,
      value_type: ValueTypeSummary::STRING,
      ..Default::default()
    }
  }

  pub fn var_assign(tgt: u32, arg: Arg) -> Self {
    let value_type = match &arg {
      Arg::Const(c) => ValueTypeSummary::from_const(c),
      Arg::Fn(_) => ValueTypeSummary::FUNCTION,
      _ => ValueTypeSummary::UNKNOWN,
    };
    Self {
      t: InstTyp::VarAssign,
      tgts: vec![tgt],
      args: vec![arg],
      value_type,
      ..Default::default()
    }
  }

  pub fn null_check(tgt: impl Into<Option<u32>>, value: Arg) -> Self {
    Self {
      t: InstTyp::NullCheck,
      tgts: tgt.into().into_iter().collect(),
      args: vec![value],
      ..Default::default()
    }
  }

  pub fn field_load(tgt: u32, obj: Arg, field: FieldRef) -> Self {
    Self {
      t: InstTyp::FieldLoad,
      tgts: vec![tgt],
      args: vec![obj],
      field,
      ..Default::default()
    }
  }

  pub fn field_store(obj: Arg, field: FieldRef, val: Arg) -> Self {
    Self {
      t: InstTyp::FieldStore,
      args: vec![obj, val],
      field,
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

  pub fn array_len(tgt: u32, array: Arg, elem_layout: ArrayElemLayoutId) -> Self {
    Self {
      t: InstTyp::ArrayLen,
      tgts: vec![tgt],
      args: vec![array],
      value_type: ValueTypeSummary::NUMBER,
      elem_layout,
      checked: true,
      ..Default::default()
    }
  }

  pub fn array_load(
    tgt: u32,
    array: Arg,
    index: Arg,
    elem_layout: ArrayElemLayoutId,
    checked: bool,
  ) -> Self {
    Self {
      t: InstTyp::ArrayLoad,
      tgts: vec![tgt],
      args: vec![array, index],
      elem_layout,
      checked,
      ..Default::default()
    }
  }

  pub fn array_store(
    array: Arg,
    index: Arg,
    value: Arg,
    elem_layout: ArrayElemLayoutId,
    checked: bool,
  ) -> Self {
    Self {
      t: InstTyp::ArrayStore,
      args: vec![array, index, value],
      elem_layout,
      checked,
      ..Default::default()
    }
  }

  pub fn assume(cond: Arg) -> Self {
    Self {
      t: InstTyp::Assume,
      args: vec![cond],
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

  /// Throw and transfer control to an exception handler within the current body.
  ///
  /// This is used to model `throw` statements when they are caught by a `catch`
  /// clause or need to unwind through `finally` blocks inside the same function.
  pub fn throw_to(handler: u32, value: Arg) -> Self {
    Self {
      t: InstTyp::Throw,
      args: vec![value],
      labels: vec![handler],
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

  pub fn invoke(
    tgt: impl Into<Option<u32>>,
    callee: Arg,
    this: Arg,
    args: Vec<Arg>,
    spreads: Vec<usize>,
    normal: u32,
    exception: u32,
  ) -> Self {
    let total_args_len = args.len() + 2;
    assert!(spreads.iter().all(|&i| i >= 2 && i < total_args_len));
    Self {
      t: InstTyp::Invoke,
      tgts: tgt.into().into_iter().collect(),
      args: [callee, this].into_iter().chain(args).collect(),
      spreads,
      labels: vec![normal, exception],
      ..Default::default()
    }
  }

  pub fn catch(tgt: u32) -> Self {
    Self {
      t: InstTyp::Catch,
      tgts: vec![tgt],
      ..Default::default()
    }
  }

  #[cfg(feature = "semantic-ops")]
  pub fn known_api_call(tgt: impl Into<Option<u32>>, api: ApiId, args: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::KnownApiCall { api },
      tgts: tgt.into().into_iter().collect(),
      args,
      ..Default::default()
    }
  }

  pub fn array_lit(tgt: impl Into<Option<u32>>, args: Vec<Arg>, spreads: Vec<usize>) -> Self {
    let total_args_len = args.len();
    assert!(spreads.iter().all(|&i| i < total_args_len));
    Self {
      t: InstTyp::ArrayLit,
      tgts: tgt.into().into_iter().collect(),
      args,
      spreads,
      ..Default::default()
    }
  }

  pub fn object_lit(tgt: impl Into<Option<u32>>, args: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::ObjectLit,
      tgts: tgt.into().into_iter().collect(),
      args,
      ..Default::default()
    }
  }

  pub fn regex_lit(tgt: impl Into<Option<u32>>, regex: String) -> Self {
    Self {
      t: InstTyp::RegexLit,
      tgts: tgt.into().into_iter().collect(),
      args: vec![Arg::Const(Const::Str(regex))],
      ..Default::default()
    }
  }

  pub fn template_lit(tgt: impl Into<Option<u32>>, args: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::TemplateLit,
      tgts: tgt.into().into_iter().collect(),
      args,
      ..Default::default()
    }
  }

  pub fn tagged_template_lit(tgt: impl Into<Option<u32>>, args: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::TaggedTemplateLit,
      tgts: tgt.into().into_iter().collect(),
      args,
      ..Default::default()
    }
  }

  pub fn new_expr(tgt: impl Into<Option<u32>>, ctor: Arg, args: Vec<Arg>, spreads: Vec<usize>) -> Self {
    let total_args_len = args.len() + 1;
    assert!(spreads.iter().all(|&i| i >= 1 && i < total_args_len));
    Self {
      t: InstTyp::New,
      tgts: tgt.into().into_iter().collect(),
      args: [ctor].into_iter().chain(args).collect(),
      spreads,
      ..Default::default()
    }
  }

  pub fn delete(tgt: impl Into<Option<u32>>, obj: Arg, prop: Arg) -> Self {
    Self {
      t: InstTyp::Delete,
      tgts: tgt.into().into_iter().collect(),
      args: vec![obj, prop],
      ..Default::default()
    }
  }

  pub fn in_op(tgt: impl Into<Option<u32>>, left: Arg, right: Arg) -> Self {
    Self {
      t: InstTyp::In,
      tgts: tgt.into().into_iter().collect(),
      args: vec![left, right],
      ..Default::default()
    }
  }

  pub fn instanceof_op(tgt: impl Into<Option<u32>>, left: Arg, right: Arg) -> Self {
    Self {
      t: InstTyp::Instanceof,
      tgts: tgt.into().into_iter().collect(),
      args: vec![left, right],
      ..Default::default()
    }
  }

  #[cfg(feature = "native-async-ops")]
  pub fn await_(tgt: impl Into<Option<u32>>, value: Arg, known_resolved: bool) -> Self {
    let mut inst = Self {
      t: InstTyp::Await,
      tgts: tgt.into().into_iter().collect(),
      args: vec![value],
      ..Default::default()
    };
    inst.meta.await_known_resolved = known_resolved;
    inst
  }

  #[cfg(feature = "native-async-ops")]
  pub fn promise_all(tgt: impl Into<Option<u32>>, promises: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::PromiseAll,
      tgts: tgt.into().into_iter().collect(),
      args: promises,
      ..Default::default()
    }
  }

  #[cfg(feature = "native-async-ops")]
  pub fn promise_race(tgt: impl Into<Option<u32>>, promises: Vec<Arg>) -> Self {
    Self {
      t: InstTyp::PromiseRace,
      tgts: tgt.into().into_iter().collect(),
      args: promises,
      ..Default::default()
    }
  }

  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  pub fn array_chain(tgt: u32, base_array: Arg, ops: Vec<ArrayChainOpData>) -> Self {
    // Keep all SSA values in `args` so existing passes (SSA renaming, DCE, etc) that walk
    // `inst.args` continue to see the full def-use surface area.
    //
    // We store the structure (op kind + indices) separately so we don't duplicate `Arg`s that may
    // be rewritten in-place by later passes (e.g. const propagation).
    let mut args = Vec::new();
    args.push(base_array);
    let mut array_chain = Vec::with_capacity(ops.len());
    for op in ops {
      match op {
        ArrayChainOpData::Map { callback } => {
          let callback_idx = args.len();
          args.push(callback);
          array_chain.push(ArrayChainOp::Map { callback: callback_idx });
        }
        ArrayChainOpData::Filter { callback } => {
          let callback_idx = args.len();
          args.push(callback);
          array_chain.push(ArrayChainOp::Filter { callback: callback_idx });
        }
        ArrayChainOpData::Reduce { callback, init } => {
          let callback_idx = args.len();
          args.push(callback);
          let init_idx = init.map(|init| {
            let idx = args.len();
            args.push(init);
            idx
          });
          array_chain.push(ArrayChainOp::Reduce {
            callback: callback_idx,
            init: init_idx,
          });
        }
        ArrayChainOpData::Find { callback } => {
          let callback_idx = args.len();
          args.push(callback);
          array_chain.push(ArrayChainOp::Find { callback: callback_idx });
        }
        ArrayChainOpData::Every { callback } => {
          let callback_idx = args.len();
          args.push(callback);
          array_chain.push(ArrayChainOp::Every { callback: callback_idx });
        }
        ArrayChainOpData::Some { callback } => {
          let callback_idx = args.len();
          args.push(callback);
          array_chain.push(ArrayChainOp::Some { callback: callback_idx });
        }
      }
    }
    Self {
      t: InstTyp::ArrayChain,
      tgts: vec![tgt],
      args,
      array_chain,
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

  pub fn as_string_concat(&self) -> (u32, &[Arg]) {
    assert_eq!(self.t, InstTyp::StringConcat);
    (self.tgts[0], &self.args)
  }

  pub fn as_var_assign(&self) -> (u32, &Arg) {
    assert_eq!(self.t, InstTyp::VarAssign);
    (self.tgts[0], &self.args[0])
  }

  pub fn as_field_load(&self) -> (u32, &Arg, &FieldRef) {
    assert_eq!(self.t, InstTyp::FieldLoad);
    (self.tgts[0], &self.args[0], &self.field)
  }

  pub fn as_field_store(&self) -> (&Arg, &FieldRef, &Arg) {
    assert_eq!(self.t, InstTyp::FieldStore);
    (&self.args[0], &self.field, &self.args[1])
  }

  pub fn as_prop_assign(&self) -> (&Arg, &Arg, &Arg) {
    assert_eq!(self.t, InstTyp::PropAssign);
    (&self.args[0], &self.args[1], &self.args[2])
  }

  pub fn as_array_len(&self) -> (u32, &Arg, ArrayElemLayoutId) {
    assert_eq!(self.t, InstTyp::ArrayLen);
    (self.tgts[0], &self.args[0], self.elem_layout)
  }

  pub fn as_array_load(&self) -> (u32, &Arg, &Arg, ArrayElemLayoutId, bool) {
    assert_eq!(self.t, InstTyp::ArrayLoad);
    (
      self.tgts[0],
      &self.args[0],
      &self.args[1],
      self.elem_layout,
      self.checked,
    )
  }

  pub fn as_array_store(&self) -> (&Arg, &Arg, &Arg, ArrayElemLayoutId, bool) {
    assert_eq!(self.t, InstTyp::ArrayStore);
    (
      &self.args[0],
      &self.args[1],
      &self.args[2],
      self.elem_layout,
      self.checked,
    )
  }

  pub fn as_assume(&self) -> &Arg {
    assert_eq!(self.t, InstTyp::Assume);
    &self.args[0]
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

  pub fn as_invoke(&self) -> (Option<u32>, &Arg, &Arg, &[Arg], &[usize], u32, u32) {
    assert_eq!(self.t, InstTyp::Invoke);
    (
      self.tgts.get(0).copied(),
      &self.args[0],
      &self.args[1],
      &self.args[2..],
      &self.spreads,
      self.labels[0],
      self.labels[1],
    )
  }

  pub fn as_catch(&self) -> u32 {
    assert_eq!(self.t, InstTyp::Catch);
    self.tgts[0]
  }

  #[cfg(feature = "semantic-ops")]
  pub fn as_known_api_call(&self) -> (Option<u32>, ApiId, &[Arg]) {
    let InstTyp::KnownApiCall { api } = &self.t else {
      panic!("not a known api call");
    };
    (self.tgts.get(0).copied(), *api, &self.args)
  }

  #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
  pub fn as_array_chain(&self) -> (Option<u32>, &Arg, &[ArrayChainOp]) {
    assert_eq!(self.t, InstTyp::ArrayChain);
    assert!(
      !self.args.is_empty(),
      "ArrayChain convention requires args[0] to be the base array"
    );
    (self.tgts.get(0).copied(), &self.args[0], &self.array_chain)
  }

  pub fn as_null_check(&self) -> (Option<u32>, &Arg) {
    assert_eq!(self.t, InstTyp::NullCheck);
    (self.tgts.get(0).copied(), &self.args[0])
  }

  pub fn as_array_lit(&self) -> (Option<u32>, &[Arg], &[usize]) {
    assert_eq!(self.t, InstTyp::ArrayLit);
    (self.tgts.get(0).copied(), &self.args, &self.spreads)
  }

  pub fn as_object_lit(&self) -> (Option<u32>, &[Arg]) {
    assert_eq!(self.t, InstTyp::ObjectLit);
    (self.tgts.get(0).copied(), &self.args)
  }

  pub fn as_regex_lit(&self) -> (Option<u32>, &Arg) {
    assert_eq!(self.t, InstTyp::RegexLit);
    (self.tgts.get(0).copied(), &self.args[0])
  }

  pub fn as_template_lit(&self) -> (Option<u32>, &[Arg]) {
    assert_eq!(self.t, InstTyp::TemplateLit);
    (self.tgts.get(0).copied(), &self.args)
  }

  pub fn as_tagged_template_lit(&self) -> (Option<u32>, &[Arg]) {
    assert_eq!(self.t, InstTyp::TaggedTemplateLit);
    (self.tgts.get(0).copied(), &self.args)
  }

  pub fn as_new_expr(&self) -> (Option<u32>, &Arg, &[Arg], &[usize]) {
    assert_eq!(self.t, InstTyp::New);
    (
      self.tgts.get(0).copied(),
      &self.args[0],
      &self.args[1..],
      &self.spreads,
    )
  }

  pub fn as_delete(&self) -> (Option<u32>, &Arg, &Arg) {
    assert_eq!(self.t, InstTyp::Delete);
    (self.tgts.get(0).copied(), &self.args[0], &self.args[1])
  }

  pub fn as_in_op(&self) -> (Option<u32>, &Arg, &Arg) {
    assert_eq!(self.t, InstTyp::In);
    (self.tgts.get(0).copied(), &self.args[0], &self.args[1])
  }

  pub fn as_instanceof_op(&self) -> (Option<u32>, &Arg, &Arg) {
    assert_eq!(self.t, InstTyp::Instanceof);
    (self.tgts.get(0).copied(), &self.args[0], &self.args[1])
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
