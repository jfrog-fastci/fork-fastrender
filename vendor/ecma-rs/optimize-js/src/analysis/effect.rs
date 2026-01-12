use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, BinOp, EffectLocation, EffectSet, Inst, InstTyp, ValueTypeSummary};
use crate::symbol::semantics::SymbolId;
#[cfg(feature = "typed")]
use crate::types::TypeId;
use crate::{FnId, Program};
use effect_model::{EffectFlags, ThrowBehavior};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "typed")]
use std::sync::Arc;

#[cfg(feature = "typed")]
use super::alias;
use super::value_types::ValueTypeSummaries;

/// Function-level effect summaries for every function in a [`crate::Program`].
///
/// `functions` is index-aligned with `Program::functions` and `FnId`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FnEffectMap {
  pub top_level: EffectSet,
  pub functions: Vec<EffectSet>,
  pub(crate) constant_foreign_fns: BTreeMap<SymbolId, FnId>,
}

impl FnEffectMap {
  pub fn get(&self, id: FnId) -> Option<&EffectSet> {
    self.functions.get(id)
  }

  pub(crate) fn constant_foreign_fns(&self) -> &BTreeMap<SymbolId, FnId> {
    &self.constant_foreign_fns
  }
}

#[derive(Clone, Debug)]
enum CalleeVarDef {
  Alias(u32),
  Fn(FnId),
  Phi(Vec<Arg>),
  Unknown,
}

fn collect_insts(cfg: &Cfg) -> Vec<&Inst> {
  // Only consider instructions reachable from the CFG entry. The optimizer may leave behind
  // unreachable blocks (e.g. implicit `return undefined` after an explicit return), and including
  // them would pessimistically degrade summaries.
  cfg
    .reverse_postorder()
    .into_iter()
    .flat_map(|label| {
      cfg
        .bblocks
        .maybe_get(label)
        .into_iter()
        .flat_map(|bb| bb.iter())
    })
    .collect()
}

// --- Strict-native, typed field-level effect modeling ------------------------------------------
//
// In untyped mode (or non-strict-native TypeScript), property access/assignment is modeled as a
// coarse Heap read/write because JavaScript semantics are extremely dynamic:
// - prototype lookups can observe prototype mutation,
// - assignment may create new properties and mutate hidden classes,
// - getters/setters/proxies can run arbitrary user code.
//
// For the strict-native subset, the type checker enforces invariants that make it sound to treat
// property operations as plain field loads/stores when:
// - the property key is a constant (string/number), and
// - the receiver has a known, finite set of object shapes (from `typecheck-ts` + `types-ts-interned`).
//
// Note: `native_strict` still permits computed property access with dynamic *numeric* keys when the
// receiver has a numeric index signature. This pass currently only refines constant-key accesses;
// dynamic indexer cases conservatively fall back to `EffectLocation::Heap`.
//
// When those conditions are met and alias analysis can identify concrete allocation sites for the
// receiver, we can model effects at field granularity via
// `EffectLocation::AllocField { alloc, key }`, enabling safe reordering and better load/store
// elimination.

#[cfg(feature = "typed")]
#[derive(Clone, Debug, Default)]
struct VarTypeIds {
  vars: BTreeMap<u32, Option<TypeId>>,
}

#[cfg(feature = "typed")]
impl VarTypeIds {
  fn new(cfg: &Cfg) -> Self {
    let mut vars: BTreeMap<u32, Option<TypeId>> = BTreeMap::new();
    for inst in collect_insts(cfg) {
      let Some(type_id) = inst.meta.type_id else {
        continue;
      };
      for &tgt in inst.tgts.iter() {
        vars
          .entry(tgt)
          .and_modify(|existing| {
            // Non-SSA CFGs may assign the same temp multiple times; only keep the type when we can
            // prove it is constant.
            *existing = match *existing {
              None => None,
              Some(prev) if prev == type_id => Some(prev),
              Some(_) => None,
            };
          })
          .or_insert(Some(type_id));
      }
    }
    Self { vars }
  }

  fn var(&self, var: u32) -> Option<TypeId> {
    self.vars.get(&var).copied().flatten()
  }

  fn arg(&self, arg: &Arg) -> Option<TypeId> {
    match arg {
      Arg::Var(v) => self.var(*v),
      _ => None,
    }
  }
}

#[cfg(feature = "typed")]
#[derive(Clone)]
struct PreciseEffectCtx<'a> {
  type_program: &'a typecheck_ts::Program,
  store: Arc<types_ts_interned::TypeStore>,
  var_types: VarTypeIds,
  strict_native: bool,
  alias: alias::AliasResult,
}

#[cfg(feature = "typed")]
impl<'a> PreciseEffectCtx<'a> {
  fn new(cfg: &Cfg, type_program: &'a typecheck_ts::Program) -> Self {
    let opts = type_program.compiler_options();
    let strict_native = opts.native_strict || opts.strict_native;
    Self {
      type_program,
      store: type_program.interned_type_store(),
      var_types: VarTypeIds::new(cfg),
      strict_native,
      alias: alias::calculate_alias(cfg),
    }
  }
}

#[cfg(feature = "typed")]
fn prop_key_from_const(
  store: &types_ts_interned::TypeStore,
  key: &Arg,
) -> Option<types_ts_interned::PropKey> {
  use crate::il::inst::Const;
  match key {
    Arg::Const(Const::Str(s)) => Some(types_ts_interned::PropKey::String(
      store.intern_name_ref(s),
    )),
    Arg::Const(Const::Num(n)) => {
      let value = n.0;
      if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
      {
        Some(types_ts_interned::PropKey::Number(value as i64))
      } else {
        None
      }
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn shape_has_own_property(
  store: &types_ts_interned::TypeStore,
  shape: types_ts_interned::ShapeId,
  key: &types_ts_interned::PropKey,
) -> bool {
  let shape = store.shape(shape);
  shape.properties.iter().any(|prop| &prop.key == key)
}

#[cfg(feature = "typed")]
fn key_is_array_index_or_length(key: &Arg) -> bool {
  use crate::il::inst::Const;
  match key {
    Arg::Const(Const::Str(s)) => {
      if s == "length" {
        return true;
      }
      let Ok(index) = s.parse::<u32>() else {
        return false;
      };
      // Array indices are canonical uint32 strings excluding 2^32-1.
      index != u32::MAX && index.to_string() == *s
    }
    Arg::Const(Const::Num(n)) => {
      let value = n.0;
      value.is_finite()
        && value.fract() == 0.0
        && value >= 0.0
        && value < (u32::MAX as f64)
    }
    _ => false,
  }
}

#[cfg(feature = "typed")]
fn type_to_object_shapes(
  program: &typecheck_ts::Program,
  store: &types_ts_interned::TypeStore,
  ty: TypeId,
  max_shapes: usize,
  depth: u8,
) -> Option<Vec<types_ts_interned::ShapeId>> {
  if depth >= 8 {
    return None;
  }

  use types_ts_interned::TypeKind as K;
  match program.interned_type_kind(ty) {
    K::Object(obj) => Some(vec![store.object(obj).shape]),
    K::Ref { def, .. } => {
      let declared = program.declared_type_of_def_interned(def);
      type_to_object_shapes(program, store, declared, max_shapes, depth + 1)
    }
    K::Union(members) => {
      let mut shapes: Vec<types_ts_interned::ShapeId> = Vec::new();
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let mut member_shapes =
          type_to_object_shapes(program, store, member, max_shapes, depth + 1)?;
        shapes.append(&mut member_shapes);
        if shapes.len() > max_shapes {
          return None;
        }
      }
      shapes.sort_unstable();
      shapes.dedup();
      if shapes.is_empty() {
        None
      } else {
        Some(shapes)
      }
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn type_to_array_elem_layouts(
  program: &typecheck_ts::Program,
  store: &types_ts_interned::TypeStore,
  ty: TypeId,
  max_layouts: usize,
  depth: u8,
) -> Option<Vec<types_ts_interned::LayoutId>> {
  if depth >= 8 {
    return None;
  }

  use types_ts_interned::TypeKind as K;
  fn type_is_number_like(program: &typecheck_ts::Program, ty: TypeId, depth: u8) -> bool {
    if depth >= 8 {
      return false;
    }
    use types_ts_interned::TypeKind as K;
    match program.interned_type_kind(ty) {
      K::Number | K::NumberLiteral(_) => true,
      K::Ref { def, .. } => {
        let declared = program.declared_type_of_def_interned(def);
        type_is_number_like(program, declared, depth + 1)
      }
      K::Union(members) => members.iter().all(|member| {
        matches!(program.interned_type_kind(*member), K::Never)
          || type_is_number_like(program, *member, depth + 1)
      }),
      _ => false,
    }
  }

  match program.interned_type_kind(ty) {
    K::Array { ty: elem_ty, .. } => Some(vec![program.layout_of_interned(elem_ty)]),
    K::Object(obj) => {
      // Treat structural objects with a numeric index signature as "array-like" for effect
      // purposes. This complements the strict-native rule that allows dynamic numeric keys when a
      // numeric indexer is present.
      let shape_id = store.object(obj).shape;
      let shape = store.shape(shape_id);
      let mut layouts: Vec<types_ts_interned::LayoutId> = Vec::new();
      for indexer in shape.indexers.iter() {
        if type_is_number_like(program, indexer.key_type, depth + 1) {
          layouts.push(program.layout_of_interned(indexer.value_type));
          if layouts.len() > max_layouts {
            return None;
          }
        }
      }
      layouts.sort_unstable();
      layouts.dedup();
      if layouts.is_empty() {
        None
      } else {
        Some(layouts)
      }
    }
    K::Ref { def, .. } => {
      let declared = program.declared_type_of_def_interned(def);
      type_to_array_elem_layouts(program, store, declared, max_layouts, depth + 1)
    }
    K::Union(members) => {
      let mut layouts: Vec<types_ts_interned::LayoutId> = Vec::new();
      for member in members {
        if matches!(program.interned_type_kind(member), K::Never) {
          continue;
        }
        let mut member_layouts =
          type_to_array_elem_layouts(program, store, member, max_layouts, depth + 1)?;
        layouts.append(&mut member_layouts);
        if layouts.len() > max_layouts {
          return None;
        }
      }
      layouts.sort_unstable();
      layouts.dedup();
      if layouts.is_empty() {
        None
      } else {
        Some(layouts)
      }
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn strict_native_field_locations(
  ctx: &PreciseEffectCtx<'_>,
  receiver: &Arg,
  key: &Arg,
) -> Option<Vec<EffectLocation>> {
  // Soundness gate: only enable field-level modeling when `typecheck-ts` is configured to enforce
  // the strict-native subset (no prototype mutation, no computed property access with dynamic keys,
  // etc). Without this, JS property operations are too dynamic to model as plain field accesses.
  if !ctx.strict_native {
    return None;
  }

  let prop_key = prop_key_from_const(&ctx.store, key)?;
  let recv_ty = ctx.var_types.arg(receiver)?;

  const MAX_SHAPES: usize = 4;
  let shapes = type_to_object_shapes(ctx.type_program, &ctx.store, recv_ty, MAX_SHAPES, 0)?;
  if shapes
    .iter()
    .any(|shape| !shape_has_own_property(&ctx.store, *shape, &prop_key))
  {
    return None;
  }

  use crate::il::inst::Const;
  let receiver_var = receiver.maybe_var()?;
  let points_to = ctx
    .alias
    .points_to
    .get(&receiver_var)
    .cloned()
    .unwrap_or_else(alias::PointsToSet::top);
  if points_to.is_top() || points_to.is_empty() {
    return None;
  }

  let key = match key {
    Arg::Const(Const::Str(s)) => s.clone(),
    Arg::Const(Const::Num(n)) => {
      let value = n.0;
      if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
      {
        (value as i64).to_string()
      } else {
        return None;
      }
    }
    _ => return None,
  };

  // Only refine to allocation-site field locations when alias analysis can prove the receiver is a
  // local allocation (no foreign/globals).
  let mut out = Vec::new();
  for loc in points_to.iter() {
    let alias::AbstractLoc::Alloc { .. } = loc else {
      return None;
    };
    out.push(EffectLocation::AllocField {
      alloc: loc.clone(),
      key: key.clone(),
    });
  }
  Some(out)
}

#[cfg(feature = "typed")]
fn strict_native_array_element_locations(
  ctx: &PreciseEffectCtx<'_>,
  receiver: &Arg,
  key: &Arg,
) -> Option<Vec<EffectLocation>> {
  if !ctx.strict_native {
    return None;
  }
  if !key_is_array_index_or_length(key) {
    return None;
  }
  let recv_ty = ctx.var_types.arg(receiver)?;
  const MAX_LAYOUTS: usize = 4;
  let elem_layouts =
    type_to_array_elem_layouts(ctx.type_program, &ctx.store, recv_ty, MAX_LAYOUTS, 0)?;
  Some(
    elem_layouts
      .into_iter()
      .map(|elem| EffectLocation::ArrayElements { elem })
      .collect(),
  )
}

#[cfg(feature = "typed")]
fn strict_native_prop_locations(
  ctx: &PreciseEffectCtx<'_>,
  receiver: &Arg,
  key: &Arg,
) -> Option<Vec<EffectLocation>> {
  strict_native_array_element_locations(ctx, receiver, key)
    .or_else(|| strict_native_field_locations(ctx, receiver, key))
}

fn collect_constant_foreign_fns(program: &Program) -> BTreeMap<SymbolId, FnId> {
  // Recover direct calls through captured constant function bindings.
  //
  // Inside a nested function, references to captured variables lower to:
  //   %tmp = ForeignLoad(sym)
  // and calls go through the loaded temp. If the captured symbol is only ever assigned a single
  // `Arg::Fn(id)`, treat loads from that symbol as that function ID for effect/purity purposes.
  let mut candidates = BTreeMap::<SymbolId, FnId>::new();
  let mut invalid = BTreeSet::<SymbolId>::new();

  let mut scan_cfg = |cfg: &Cfg| {
    for inst in collect_insts(cfg) {
      if inst.t != InstTyp::ForeignStore {
        continue;
      }
      let sym = inst.foreign;
      if invalid.contains(&sym) {
        continue;
      }
      match inst.args.get(0) {
        Some(Arg::Fn(id)) => match candidates.get(&sym).copied() {
          None => {
            candidates.insert(sym, *id);
          }
          Some(existing) if existing == *id => {}
          Some(_) => {
            candidates.remove(&sym);
            invalid.insert(sym);
          }
        },
        _ => {
          candidates.remove(&sym);
          invalid.insert(sym);
        }
      }
    }
  };

  scan_cfg(program.top_level.analyzed_cfg());
  for func in &program.functions {
    scan_cfg(func.analyzed_cfg());
  }

  candidates
}

fn build_callee_var_defs(
  cfg: &Cfg,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
) -> BTreeMap<u32, CalleeVarDef> {
  let mut defs = BTreeMap::<u32, CalleeVarDef>::new();
  for inst in collect_insts(cfg) {
    let Some(&tgt) = inst.tgts.get(0) else {
      continue;
    };

    let def = match inst.t {
      InstTyp::VarAssign => match &inst.args[0] {
        Arg::Var(src) => CalleeVarDef::Alias(*src),
        Arg::Fn(id) => CalleeVarDef::Fn(*id),
        _ => CalleeVarDef::Unknown,
      },
      InstTyp::Phi => CalleeVarDef::Phi(inst.args.clone()),
      InstTyp::ForeignLoad => foreign_fns
        .get(&inst.foreign)
        .copied()
        .map(CalleeVarDef::Fn)
        .unwrap_or(CalleeVarDef::Unknown),
      _ => CalleeVarDef::Unknown,
    };

    defs
      .entry(tgt)
      .and_modify(|existing| {
        // Non-SSA CFGs may assign the same temp multiple times. Only keep definitions when we can
        // prove the value is constant.
        if !matches!(existing, CalleeVarDef::Unknown) {
          *existing = CalleeVarDef::Unknown;
        }
      })
      .or_insert(def);
  }
  defs
}

fn resolve_fn_id(
  arg: &Arg,
  defs: &BTreeMap<u32, CalleeVarDef>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  match arg {
    Arg::Fn(id) => Some(*id),
    Arg::Var(v) => resolve_var_fn_id(*v, defs, visiting),
    _ => None,
  }
}

fn resolve_var_fn_id(
  var: u32,
  defs: &BTreeMap<u32, CalleeVarDef>,
  visiting: &mut Vec<u32>,
) -> Option<FnId> {
  if visiting.contains(&var) {
    return None;
  }
  visiting.push(var);

  let out = match defs.get(&var) {
    Some(CalleeVarDef::Fn(id)) => Some(*id),
    Some(CalleeVarDef::Alias(src)) => resolve_var_fn_id(*src, defs, visiting),
    Some(CalleeVarDef::Phi(args)) => {
      let mut merged: Option<FnId> = None;
      for arg in args {
        let Some(id) = resolve_fn_id(arg, defs, visiting) else {
          visiting.pop();
          return None;
        };
        merged = match merged {
          None => Some(id),
          Some(prev) if prev == id => Some(prev),
          _ => {
            visiting.pop();
            return None;
          }
        };
      }
      merged
    }
    _ => None,
  };

  visiting.pop();
  out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToNumberConversion {
  Pure,
  MaybeThrow,
  AlwaysThrow,
  Unknown,
}

fn to_number_conversion(ty: ValueTypeSummary) -> ToNumberConversion {
  if ty.is_unknown()
    || ty.contains(ValueTypeSummary::OBJECT)
    || ty.contains(ValueTypeSummary::FUNCTION)
  {
    return ToNumberConversion::Unknown;
  }
  if ty == ValueTypeSummary::BIGINT || ty == ValueTypeSummary::SYMBOL {
    return ToNumberConversion::AlwaysThrow;
  }
  if ty.contains(ValueTypeSummary::BIGINT) || ty.contains(ValueTypeSummary::SYMBOL) {
    return ToNumberConversion::MaybeThrow;
  }

  // Remaining possibilities are unions of {null, undefined, boolean, number, string}, which never
  // throw and do not invoke user code.
  ToNumberConversion::Pure
}

fn to_number_builtin_conversion(
  path: &str,
  args: &[Arg],
  value_types: Option<&ValueTypeSummaries>,
) -> Option<ToNumberConversion> {
  let arg_type = |idx: usize| match args.get(idx) {
    None => ValueTypeSummary::UNDEFINED,
    Some(arg) => match value_types {
      Some(types) => types.arg(arg).unwrap_or(ValueTypeSummary::UNKNOWN),
      None => match arg {
        Arg::Const(c) => ValueTypeSummary::from_const(c),
        Arg::Fn(_) => ValueTypeSummary::FUNCTION,
        _ => ValueTypeSummary::UNKNOWN,
      },
    },
  };

  let unary = matches!(
    path,
    "Math.abs"
      | "Math.acos"
      | "Math.asin"
      | "Math.atan"
      | "Math.ceil"
      | "Math.cos"
      | "Math.floor"
      | "Math.log"
      | "Math.log10"
      | "Math.log1p"
      | "Math.log2"
      | "Math.round"
      | "Math.sin"
      | "Math.sqrt"
      | "Math.tan"
      | "Math.trunc"
      | "Number"
  );
  if unary {
    return Some(to_number_conversion(arg_type(0)));
  }

  if path == "Math.pow" {
    let base = to_number_conversion(arg_type(0));
    let exp = to_number_conversion(arg_type(1));
    let combined = match base {
      ToNumberConversion::Unknown => ToNumberConversion::Unknown,
      ToNumberConversion::AlwaysThrow => ToNumberConversion::AlwaysThrow,
      ToNumberConversion::Pure => exp,
      ToNumberConversion::MaybeThrow => match exp {
        ToNumberConversion::Unknown => ToNumberConversion::Unknown,
        // If either conversion step always throws, the overall builtin always throws.
        ToNumberConversion::AlwaysThrow => ToNumberConversion::AlwaysThrow,
        ToNumberConversion::MaybeThrow | ToNumberConversion::Pure => ToNumberConversion::MaybeThrow,
      },
    };
    return Some(combined);
  }

  None
}

fn local_effect_for_tonumber_builtin(
  path: &str,
  args: &[Arg],
  value_types: Option<&ValueTypeSummaries>,
) -> Option<EffectSet> {
  let conversion = to_number_builtin_conversion(path, args, value_types)?;
  let mut effects = EffectSet::default();
  match conversion {
    ToNumberConversion::Pure => {}
    ToNumberConversion::MaybeThrow => {
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    ToNumberConversion::AlwaysThrow => {
      effects.summary.throws = ThrowBehavior::Always;
    }
    ToNumberConversion::Unknown => {
      effects.mark_unknown();
    }
  }
  Some(effects)
}

/// Classify the local effects of a single IL instruction.
///
/// This excludes interprocedural callee summaries for direct `Arg::Fn` calls;
/// those are incorporated by [`compute_program_effects`] (function summaries)
/// and [`annotate_cfg_effects`] (per-instruction metadata).
fn inst_local_effect_with_value_types(
  inst: &Inst,
  value_types: Option<&ValueTypeSummaries>,
) -> EffectSet {
  let mut effects = EffectSet::default();

  match inst.t {
    InstTyp::NullCheck => {
      // Null checks are read-only but may throw/trap when the value is `null` or `undefined`.
      //
      // Use value type information when available to avoid pessimistically marking checks on
      // statically non-nullish values as throwing.
      let value = inst.args.get(0);
      let value_ty = match (value, value_types) {
        (Some(arg), Some(types)) => types.arg(arg).unwrap_or(ValueTypeSummary::UNKNOWN),
        (Some(Arg::Const(c)), _) => ValueTypeSummary::from_const(c),
        (Some(Arg::Fn(_)), _) => ValueTypeSummary::FUNCTION,
        _ => ValueTypeSummary::UNKNOWN,
      };

      if value_ty.is_unknown() {
        effects.summary.throws = ThrowBehavior::Maybe;
      } else {
        let can_nullish =
          value_ty.contains(ValueTypeSummary::NULL) || value_ty.contains(ValueTypeSummary::UNDEFINED);
        let can_other = value_ty
          .contains(ValueTypeSummary::BOOLEAN)
          || value_ty.contains(ValueTypeSummary::NUMBER)
          || value_ty.contains(ValueTypeSummary::STRING)
          || value_ty.contains(ValueTypeSummary::BIGINT)
          || value_ty.contains(ValueTypeSummary::SYMBOL)
          || value_ty.contains(ValueTypeSummary::FUNCTION)
          || value_ty.contains(ValueTypeSummary::OBJECT);
        if can_nullish {
          effects.summary.throws = if can_other {
            ThrowBehavior::Maybe
          } else {
            ThrowBehavior::Always
          };
        }
      }
    }
    InstTyp::Bin => {
      if inst.bin_op == BinOp::GetProp {
        effects.reads.insert(EffectLocation::Heap);
        effects.summary.throws = ThrowBehavior::Maybe;
      }
    }
    InstTyp::StringConcat => {
      // String concatenation is treated as a pure allocation (same as the
      // internal `__optimize_js_template` marker call it replaces in typed
      // lowerings).
      effects.summary.flags |= EffectFlags::ALLOCATES;
    }
    InstTyp::FieldLoad => {
      effects.reads.insert(EffectLocation::Heap);
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::Throw => {
      effects.summary.throws = ThrowBehavior::Always;
    }
    InstTyp::PropAssign => {
      effects.writes.insert(EffectLocation::Heap);
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::FieldStore => {
      effects.writes.insert(EffectLocation::Heap);
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::ForeignLoad => {
      effects.reads.insert(EffectLocation::Foreign(inst.foreign));
    }
    InstTyp::ForeignStore => {
      effects.writes.insert(EffectLocation::Foreign(inst.foreign));
    }
    InstTyp::UnknownLoad => {
      effects
        .reads
        .insert(EffectLocation::Unknown(inst.unknown.clone()));
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::UnknownStore => {
      effects
        .writes
        .insert(EffectLocation::Unknown(inst.unknown.clone()));
      effects.summary.throws = ThrowBehavior::Maybe;
    }
    InstTyp::Call | InstTyp::Invoke => {
      let (_, callee, _, args, _) = match inst.t {
        InstTyp::Call => {
          let (tgt, callee, this, args, spreads) = inst.as_call();
          (tgt, callee, this, args, spreads)
        }
        InstTyp::Invoke => {
          let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
          (tgt, callee, this, args, spreads)
        }
        _ => unreachable!(),
      };
      match callee {
        Arg::Fn(_) => {
          // The callee effects are accounted for interprocedurally.
        }
        Arg::Builtin(path) => match path.as_str() {
          // Internal lowering helpers that construct literals / perform pure allocations.
          "__optimize_js_array"
          | "__optimize_js_object"
          | "__optimize_js_regex"
          | "__optimize_js_template" => {
            effects.summary.flags |= EffectFlags::ALLOCATES;
          }
          // Tagged templates call the tag function; we conservatively treat them as unknown.
          "__optimize_js_tagged_template" => {
            effects.summary.flags |= EffectFlags::ALLOCATES;
            effects.mark_unknown();
          }
          "__optimize_js_in" => {
            // Property existence checks read heap state and can throw on nullish RHS.
            effects.reads.insert(EffectLocation::Heap);
            effects.summary.throws = ThrowBehavior::Maybe;
          }
          "__optimize_js_instanceof" => {
            // `instanceof` can consult `Symbol.hasInstance` and invoke user code.
            effects.reads.insert(EffectLocation::Heap);
            effects.mark_unknown();
          }
          "__optimize_js_delete" => {
            effects.writes.insert(EffectLocation::Heap);
            effects.mark_unknown();
          }
          "__optimize_js_new" | "__optimize_js_await" | "import" => {
            effects.mark_unknown();
          }
          _ => match local_effect_for_tonumber_builtin(path, args, value_types) {
            Some(local) => effects = local,
            None => effects.mark_unknown(),
          },
        },
        _ => {
          effects.mark_unknown();
        }
      }
    }
    #[cfg(feature = "semantic-ops")]
    InstTyp::KnownApiCall { .. } => {
      // Until a knowledge-base integration can provide precise summaries for known API IDs,
      // treat these calls as fully unknown and heap-affecting.
      effects.reads.insert(EffectLocation::Heap);
      effects.writes.insert(EffectLocation::Heap);
      effects.mark_unknown();
    }
    #[cfg(feature = "native-async-ops")]
    InstTyp::Await | InstTyp::PromiseAll | InstTyp::PromiseRace => {
      // Async semantic ops are modeled conservatively for now: they may allocate (promises), may
      // throw, and may run user code (thenables / unhandled rejection tracking).
      //
      // Native backends can use the structured instruction kind to implement these precisely.
      effects.mark_unknown();
    }
    #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
    InstTyp::ArrayChain => {
      // Array semantic ops may allocate, may throw, and may invoke user callbacks. Model
      // conservatively until native lowering provides more precise summaries.
      effects.mark_unknown();
    }
    InstTyp::CondGoto
    | InstTyp::Assume
    | InstTyp::Return
    | InstTyp::Catch
    | InstTyp::Un
    | InstTyp::VarAssign
    | InstTyp::Phi
    | InstTyp::_Label => {}
    // These should not exist after CFG construction but are treated as no-ops for analysis.
    InstTyp::_Goto | InstTyp::_Dummy => {}
  }

  effects
}

pub fn inst_local_effect(inst: &Inst) -> EffectSet {
  inst_local_effect_with_value_types(inst, None)
}

#[cfg(feature = "typed")]
fn inst_local_effect_with_value_types_typed(
  inst: &Inst,
  value_types: Option<&ValueTypeSummaries>,
  ctx: &PreciseEffectCtx<'_>,
) -> EffectSet {
  let mut effects = inst_local_effect_with_value_types(inst, value_types);

  // Refine heap-level effects for property ops into field-level effects when strict-native typed
  // invariants make it sound (see the module comment near `PreciseEffectCtx`).
  match inst.t {
    InstTyp::Bin if inst.bin_op == BinOp::GetProp => {
      if let (Some(receiver), Some(key)) = (inst.args.get(0), inst.args.get(1)) {
        if let Some(locs) = strict_native_prop_locations(ctx, receiver, key) {
          effects.reads.remove(&EffectLocation::Heap);
          effects.reads.extend(locs);
        }
      }
    }
    InstTyp::PropAssign => {
      if let (Some(receiver), Some(key)) = (inst.args.get(0), inst.args.get(1)) {
        if let Some(locs) = strict_native_prop_locations(ctx, receiver, key) {
          effects.writes.remove(&EffectLocation::Heap);
          effects.writes.extend(locs);
        }
      }
    }
    _ => {}
  }

  effects
}

fn inst_total_effect(
  inst: &Inst,
  fn_summaries: &FnEffectMap,
  defs: &BTreeMap<u32, CalleeVarDef>,
  value_types: Option<&ValueTypeSummaries>,
) -> EffectSet {
  if !matches!(inst.t, InstTyp::Call | InstTyp::Invoke) {
    return inst_local_effect_with_value_types(inst, value_types);
  }

  let (_, callee, _, _, _) = match inst.t {
    InstTyp::Call => {
      let (tgt, callee, this, args, spreads) = inst.as_call();
      (tgt, callee, this, args, spreads)
    }
    InstTyp::Invoke => {
      let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
      (tgt, callee, this, args, spreads)
    }
    _ => unreachable!(),
  };
  if matches!(callee, Arg::Builtin(_)) {
    return inst_local_effect_with_value_types(inst, value_types);
  }

  let Some(id) = resolve_fn_id(callee, defs, &mut Vec::new()) else {
    return inst_local_effect_with_value_types(inst, value_types);
  };

  let mut effects = EffectSet::default();
  if let Some(summary) = fn_summaries.get(id) {
    effects.merge(summary);
  } else {
    // A resolved callee with no corresponding summary should be impossible, but if it happens we
    // must stay conservative.
    effects.mark_unknown();
  }
  effects
}

#[cfg(feature = "typed")]
fn inst_total_effect_typed(
  inst: &Inst,
  fn_summaries: &FnEffectMap,
  defs: &BTreeMap<u32, CalleeVarDef>,
  value_types: Option<&ValueTypeSummaries>,
  ctx: &PreciseEffectCtx<'_>,
) -> EffectSet {
  if inst.t != InstTyp::Call {
    return inst_local_effect_with_value_types_typed(inst, value_types, ctx);
  }

  let (_, callee, _, _, _) = inst.as_call();
  if matches!(callee, Arg::Builtin(_)) {
    return inst_local_effect_with_value_types_typed(inst, value_types, ctx);
  }

  let Some(id) = resolve_fn_id(callee, defs, &mut Vec::new()) else {
    return inst_local_effect_with_value_types_typed(inst, value_types, ctx);
  };

  let mut effects = EffectSet::default();
  if let Some(summary) = fn_summaries.get(id) {
    effects.merge(summary);
  } else {
    effects.mark_unknown();
  }
  effects
}

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels = cfg
    .bblocks
    .all()
    .map(|(label, _)| label)
    .collect::<Vec<_>>();
  labels.sort_unstable();
  labels
}

fn cfg_local_effects(cfg: &Cfg, foreign_fns: &BTreeMap<SymbolId, FnId>) -> EffectSet {
  let defs = build_callee_var_defs(cfg, foreign_fns);
  let value_types = ValueTypeSummaries::new(cfg);
  let mut effects = EffectSet::default();
  for inst in collect_insts(cfg) {
    if !matches!(inst.t, InstTyp::Call | InstTyp::Invoke) {
      effects.merge(&inst_local_effect_with_value_types(inst, Some(&value_types)));
      continue;
    }
    let (_, callee, _, _, _) = match inst.t {
      InstTyp::Call => {
        let (tgt, callee, this, args, spreads) = inst.as_call();
        (tgt, callee, this, args, spreads)
      }
      InstTyp::Invoke => {
        let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
        (tgt, callee, this, args, spreads)
      }
      _ => unreachable!(),
    };
    // Builtin calls have intrinsic local effects.
    if matches!(callee, Arg::Builtin(_)) {
      effects.merge(&inst_local_effect_with_value_types(
        inst,
        Some(&value_types),
      ));
      continue;
    }
    // Direct calls to nested functions are accounted for interprocedurally.
    if resolve_fn_id(callee, &defs, &mut Vec::new()).is_some() {
      continue;
    }
    effects.merge(&inst_local_effect_with_value_types(
      inst,
      Some(&value_types),
    ));
  }
  effects
}

#[cfg(feature = "typed")]
fn cfg_local_effects_typed(
  cfg: &Cfg,
  foreign_fns: &BTreeMap<SymbolId, FnId>,
  type_program: &typecheck_ts::Program,
) -> EffectSet {
  let defs = build_callee_var_defs(cfg, foreign_fns);
  let value_types = ValueTypeSummaries::new(cfg);
  let ctx = PreciseEffectCtx::new(cfg, type_program);
  let mut effects = EffectSet::default();
  for inst in collect_insts(cfg) {
    if inst.t != InstTyp::Call {
      effects.merge(&inst_local_effect_with_value_types_typed(
        inst,
        Some(&value_types),
        &ctx,
      ));
      continue;
    }
    let (_, callee, _, _, _) = inst.as_call();
    // Builtin calls have intrinsic local effects.
    if matches!(callee, Arg::Builtin(_)) {
      effects.merge(&inst_local_effect_with_value_types_typed(
        inst,
        Some(&value_types),
        &ctx,
      ));
      continue;
    }
    // Direct calls to nested functions are accounted for interprocedurally.
    if resolve_fn_id(callee, &defs, &mut Vec::new()).is_some() {
      continue;
    }
    effects.merge(&inst_local_effect_with_value_types_typed(
      inst,
      Some(&value_types),
      &ctx,
    ));
  }
  effects
}

fn cfg_direct_calls(cfg: &Cfg, foreign_fns: &BTreeMap<SymbolId, FnId>) -> BTreeSet<FnId> {
  let defs = build_callee_var_defs(cfg, foreign_fns);
  let mut callees = BTreeSet::new();
  for inst in collect_insts(cfg) {
    if !matches!(inst.t, InstTyp::Call | InstTyp::Invoke) {
      continue;
    }
    let (_, callee, _, _, _) = match inst.t {
      InstTyp::Call => {
        let (tgt, callee, this, args, spreads) = inst.as_call();
        (tgt, callee, this, args, spreads)
      }
      InstTyp::Invoke => {
        let (tgt, callee, this, args, spreads, _normal, _exception) = inst.as_invoke();
        (tgt, callee, this, args, spreads)
      }
      _ => unreachable!(),
    };
    if let Some(id) = resolve_fn_id(callee, &defs, &mut Vec::new()) {
      callees.insert(id);
    }
  }
  callees
}

/// Whole-program effect analysis over the current IL.
///
/// This computes a fixpoint of function summaries so that direct `Arg::Fn`
/// calls incorporate callee summaries (including recursion/cycles).
pub fn compute_program_effects(program: &Program) -> FnEffectMap {
  let foreign_fns = collect_constant_foreign_fns(program);
  let locals = FnEffectMap {
    top_level: cfg_local_effects(program.top_level.analyzed_cfg(), &foreign_fns),
    functions: program
      .functions
      .iter()
      .map(|f| cfg_local_effects(f.analyzed_cfg(), &foreign_fns))
      .collect(),
    constant_foreign_fns: foreign_fns.clone(),
  };

  let top_level_calls = cfg_direct_calls(program.top_level.analyzed_cfg(), &foreign_fns);
  let function_calls: Vec<_> = program
    .functions
    .iter()
    .map(|f| cfg_direct_calls(f.analyzed_cfg(), &foreign_fns))
    .collect();

  // Start with purely local effects; iteratively fold in callee summaries until a fixpoint.
  let mut summaries = locals.clone();

  loop {
    let mut changed = false;

    let mut new_top = locals.top_level.clone();
    for callee in top_level_calls.iter().copied() {
      if let Some(summary) = summaries.get(callee) {
        new_top.merge(summary);
      } else {
        new_top.mark_unknown();
      }
    }
    if new_top != summaries.top_level {
      summaries.top_level = new_top;
      changed = true;
    }

    for fn_id in 0..program.functions.len() {
      let mut new_summary = locals.functions[fn_id].clone();
      for callee in function_calls[fn_id].iter().copied() {
        if let Some(summary) = summaries.get(callee) {
          new_summary.merge(summary);
        } else {
          new_summary.mark_unknown();
        }
      }
      if new_summary != summaries.functions[fn_id] {
        summaries.functions[fn_id] = new_summary;
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  summaries
}

/// Whole-program effect analysis using `typecheck-ts` type information.
///
/// This refines heap-level property effects into field-level effects when strict-native invariants
/// make it sound (see module comment near `PreciseEffectCtx`).
#[cfg(feature = "typed")]
pub fn compute_program_effects_typed(
  program: &Program,
  type_program: &typecheck_ts::Program,
) -> FnEffectMap {
  let foreign_fns = collect_constant_foreign_fns(program);
  let locals = FnEffectMap {
    top_level: cfg_local_effects_typed(
      program.top_level.analyzed_cfg(),
      &foreign_fns,
      type_program,
    ),
    functions: program
      .functions
      .iter()
      .map(|f| cfg_local_effects_typed(f.analyzed_cfg(), &foreign_fns, type_program))
      .collect(),
    constant_foreign_fns: foreign_fns.clone(),
  };

  let top_level_calls = cfg_direct_calls(program.top_level.analyzed_cfg(), &foreign_fns);
  let function_calls: Vec<_> = program
    .functions
    .iter()
    .map(|f| cfg_direct_calls(f.analyzed_cfg(), &foreign_fns))
    .collect();

  let mut summaries = locals.clone();

  loop {
    let mut changed = false;

    let mut new_top = locals.top_level.clone();
    for callee in top_level_calls.iter().copied() {
      if let Some(summary) = summaries.get(callee) {
        new_top.merge(summary);
      } else {
        new_top.mark_unknown();
      }
    }
    if new_top != summaries.top_level {
      summaries.top_level = new_top;
      changed = true;
    }

    for fn_id in 0..program.functions.len() {
      let mut new_summary = locals.functions[fn_id].clone();
      for callee in function_calls[fn_id].iter().copied() {
        if let Some(summary) = summaries.get(callee) {
          new_summary.merge(summary);
        } else {
          new_summary.mark_unknown();
        }
      }
      if new_summary != summaries.functions[fn_id] {
        summaries.functions[fn_id] = new_summary;
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  summaries
}

/// Write per-instruction effects into [`crate::il::inst::InstMeta`].
///
/// This is intended to run on the finalized CFG (after `build_program_function`)
/// and does not attempt to preserve metadata through subsequent opt passes.
pub fn annotate_cfg_effects(cfg: &mut Cfg, fn_summaries: &FnEffectMap) {
  let defs = build_callee_var_defs(cfg, fn_summaries.constant_foreign_fns());
  let value_types = ValueTypeSummaries::new(cfg);
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get_mut(label) {
      inst.meta.effects = inst_total_effect(inst, fn_summaries, &defs, Some(&value_types));
    }
  }
}

/// Typed variant of [`annotate_cfg_effects`].
///
/// This enables field-level modeling in strict-native typed code, falling back to the existing
/// heap-level behavior otherwise.
#[cfg(feature = "typed")]
pub fn annotate_cfg_effects_typed(
  cfg: &mut Cfg,
  fn_summaries: &FnEffectMap,
  type_program: &typecheck_ts::Program,
) {
  let defs = build_callee_var_defs(cfg, fn_summaries.constant_foreign_fns());
  let value_types = ValueTypeSummaries::new(cfg);
  let ctx = PreciseEffectCtx::new(cfg, type_program);
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get_mut(label) {
      inst.meta.effects =
        inst_total_effect_typed(inst, fn_summaries, &defs, Some(&value_types), &ctx);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
  use crate::il::inst::Const;
  use crate::symbol::semantics::SymbolId;
  use crate::{OptimizationStats, ProgramFunction, TopLevelMode};
  use num_bigint::BigInt;

  const EXIT: u32 = u32::MAX;

  fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
    let mut graph = CfgGraph::default();
    graph.connect(0, EXIT);
    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, insts);
    bblocks.add(EXIT, Vec::new());
    Cfg {
      graph,
      bblocks,
      entry: 0,
    }
  }

  fn func(cfg: Cfg) -> ProgramFunction {
    ProgramFunction {
      debug: None,
      meta: Default::default(),
      body: cfg,
      params: Vec::new(),
      ssa_body: None,
      stats: OptimizationStats::default(),
    }
  }

  #[test]
  fn var_assign_is_pure() {
    let inst = Inst::var_assign(0, Arg::Var(1));
    let eff = inst_local_effect(&inst);
    assert!(eff.is_pure());
  }

  #[test]
  fn return_is_pure_and_never_throws() {
    let inst = Inst::ret(None);
    let eff = inst_local_effect(&inst);
    assert!(eff.is_pure());
    assert_eq!(eff.summary.throws, ThrowBehavior::Never);
    assert!(eff.reads.is_empty());
    assert!(eff.writes.is_empty());
    assert!(!eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!eff.unknown);
  }

  #[test]
  fn prop_assign_writes_heap_and_may_throw() {
    let inst = Inst::prop_assign(
      Arg::Var(0),
      Arg::Const(Const::Str("k".to_string())),
      Arg::Var(1),
    );
    let eff = inst_local_effect(&inst);
    assert!(eff.writes.contains(&EffectLocation::Heap));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn unknown_load_reads_global_and_may_throw() {
    let inst = Inst::unknown_load(0, "mystery".to_string());
    let eff = inst_local_effect(&inst);
    assert!(eff
      .reads
      .contains(&EffectLocation::Unknown("mystery".to_string())));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn foreign_load_store_are_classified() {
    let sym = SymbolId(1);

    let load = Inst::foreign_load(0, sym);
    let load_eff = inst_local_effect(&load);
    assert!(load_eff.reads.contains(&EffectLocation::Foreign(sym)));
    assert!(load_eff.writes.is_empty());
    assert!(!load_eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!load_eff.unknown);

    let store = Inst::foreign_store(sym, Arg::Const(Const::Undefined));
    let store_eff = inst_local_effect(&store);
    assert!(store_eff.writes.contains(&EffectLocation::Foreign(sym)));
    assert!(store_eff.reads.is_empty());
    assert!(!store_eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!store_eff.unknown);
  }

  #[test]
  fn internal_literal_builtins_allocate_without_unknown() {
    for builtin in [
      "__optimize_js_array",
      "__optimize_js_object",
      "__optimize_js_regex",
      "__optimize_js_template",
    ] {
      let call = Inst::call(
        0,
        Arg::Builtin(builtin.to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      );
      let eff = inst_local_effect(&call);
      assert!(
        eff.summary.flags.contains(EffectFlags::ALLOCATES),
        "{builtin} should allocate but got {eff:?}"
      );
      assert!(
        !eff.unknown,
        "{builtin} should not be marked unknown but got {eff:?}"
      );
      assert!(
        eff.summary.throws == ThrowBehavior::Never,
        "{builtin} should not be marked as throwing but got {eff:?}"
      );
      assert!(eff.reads.is_empty());
      assert!(eff.writes.is_empty());
    }
  }

  #[test]
  fn tagged_template_is_unknown() {
    let call = Inst::call(
      0,
      Arg::Builtin("__optimize_js_tagged_template".to_string()),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let eff = inst_local_effect(&call);
    assert!(eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(eff.unknown);
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn unknown_call_is_unknown() {
    let call = Inst::call(
      None::<u32>,
      Arg::Var(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let eff = inst_local_effect(&call);
    assert!(eff.unknown);
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn tonumber_builtin_call_with_bigint_const_is_always_throwing() {
    let call = Inst::call(
      None::<u32>,
      Arg::Builtin("Math.abs".to_string()),
      Arg::Const(Const::Undefined),
      vec![Arg::Const(Const::BigInt(BigInt::from(1)))],
      Vec::new(),
    );
    let eff = inst_local_effect(&call);
    assert_eq!(eff.summary.throws, ThrowBehavior::Always);
    assert!(!eff.unknown);
    assert!(eff.reads.is_empty());
    assert!(eff.writes.is_empty());
  }

  #[test]
  fn tonumber_builtin_call_uses_value_type_summaries() {
    let call = Inst::call(
      None::<u32>,
      Arg::Builtin("Math.abs".to_string()),
      Arg::Const(Const::Undefined),
      vec![Arg::Var(0)],
      Vec::new(),
    );

    let naive = inst_local_effect(&call);
    assert!(
      naive.unknown,
      "expected no-type-context effect to be unknown"
    );

    let cfg = cfg_single_block(vec![
      Inst::var_assign(0, Arg::Const(Const::Str("1".to_string()))),
      call.clone(),
    ]);
    let value_types = ValueTypeSummaries::new(&cfg);
    let refined = inst_local_effect_with_value_types(&call, Some(&value_types));
    assert!(refined.is_pure());
    assert!(!refined.unknown);
  }

  #[test]
  fn throw_is_always_throwing() {
    let inst = Inst::throw(Arg::Const(Const::Undefined));
    let eff = inst_local_effect(&inst);
    assert_eq!(eff.summary.throws, ThrowBehavior::Always);
    assert!(eff.reads.is_empty());
    assert!(eff.writes.is_empty());
    assert!(!eff.summary.flags.contains(EffectFlags::ALLOCATES));
    assert!(!eff.unknown);
  }

  #[test]
  fn getprop_reads_heap_and_may_throw() {
    let inst = Inst::bin(
      0,
      Arg::Var(0),
      BinOp::GetProp,
      Arg::Const(Const::Str("prop".to_string())),
    );
    let eff = inst_local_effect(&inst);
    assert!(eff.reads.contains(&EffectLocation::Heap));
    assert_eq!(eff.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn interprocedural_propagation_includes_direct_callee_effects() {
    let sym = SymbolId(7);

    // Fn0 writes a foreign symbol.
    let callee = func(cfg_single_block(vec![Inst::foreign_store(
      sym,
      Arg::Const(Const::Undefined),
    )]));

    // Fn1 calls Fn0 directly.
    let call_inst = Inst::call(
      None::<u32>,
      Arg::Fn(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let caller = func(cfg_single_block(vec![call_inst]));

    let mut program = Program {
      source_file: crate::FileId(0),
      source_len: 0,
      functions: vec![callee, caller],
      top_level: func(cfg_single_block(Vec::new())),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let summaries = compute_program_effects(&program);
    assert!(summaries.functions[0]
      .writes
      .contains(&EffectLocation::Foreign(sym)));
    assert!(summaries.functions[1]
      .writes
      .contains(&EffectLocation::Foreign(sym)));

    // Per-instruction annotation should reflect the callee's summary on the call instruction.
    annotate_cfg_effects(&mut program.functions[1].body, &summaries);
    let call_effects = &program.functions[1].body.bblocks.get(0)[0].meta.effects;
    assert!(call_effects.writes.contains(&EffectLocation::Foreign(sym)));
  }

  #[test]
  fn interprocedural_propagation_includes_may_throw() {
    // Fn0 reads an unknown global, which can throw (e.g. ReferenceError in global mode).
    let callee = func(cfg_single_block(vec![Inst::unknown_load(
      0,
      "missingGlobal".to_string(),
    )]));

    // Fn1 calls Fn0 directly.
    let call_inst = Inst::call(
      None::<u32>,
      Arg::Fn(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let mut program = Program {
      source_file: crate::FileId(0),
      source_len: 0,
      functions: vec![callee, func(cfg_single_block(vec![call_inst]))],
      top_level: func(cfg_single_block(Vec::new())),
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let summaries = compute_program_effects(&program);
    assert_eq!(summaries.functions[0].summary.throws, ThrowBehavior::Maybe);
    assert_eq!(summaries.functions[1].summary.throws, ThrowBehavior::Maybe);

    annotate_cfg_effects(&mut program.functions[1].body, &summaries);
    let call_effects = &program.functions[1].body.bblocks.get(0)[0].meta.effects;
    assert_eq!(call_effects.summary.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn interprocedural_propagation_includes_captured_constant_callee_effects() {
    let callee_sym = SymbolId(11);
    let effect_sym = SymbolId(12);

    // Fn0 writes a foreign symbol.
    let callee = func(cfg_single_block(vec![Inst::foreign_store(
      effect_sym,
      Arg::Const(Const::Undefined),
    )]));

    // Top-level initializes the captured callee binding.
    let top_level = func(cfg_single_block(vec![Inst::foreign_store(
      callee_sym,
      Arg::Fn(0),
    )]));

    // Fn1 loads the captured binding then calls it indirectly.
    let call_inst = Inst::call(
      None::<u32>,
      Arg::Var(0),
      Arg::Const(Const::Undefined),
      Vec::new(),
      Vec::new(),
    );
    let caller = func(cfg_single_block(vec![
      Inst::foreign_load(0, callee_sym),
      call_inst,
    ]));

    let mut program = Program {
      source_file: crate::FileId(0),
      source_len: 0,
      functions: vec![callee, caller],
      top_level,
      top_level_mode: TopLevelMode::Module,
      symbols: None,
    };

    let summaries = compute_program_effects(&program);
    assert!(summaries.functions[1]
      .writes
      .contains(&EffectLocation::Foreign(effect_sym)));
    assert!(!summaries.functions[1].unknown);

    annotate_cfg_effects(&mut program.functions[1].body, &summaries);
    let call_effects = &program.functions[1].body.bblocks.get(0)[1].meta.effects;
    assert!(call_effects
      .writes
      .contains(&EffectLocation::Foreign(effect_sym)));
    assert!(!call_effects.unknown);
  }
}
