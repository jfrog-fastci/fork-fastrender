use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use diagnostics::Diagnostic;
use effect_model::EffectSummary;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::dump::{compile_source_to_dump, CompileDumpOptions, ProgramDump as OptimizeProgramDump};
use optimize_js::il::inst::{
  Arg, BinOp, Const, EffectLocation, EffectSet, Inst, InstMeta, InstTyp, Purity, UnOp,
};
use optimize_js::{
  compile_source, ProgramFunction, ProgramScope, ProgramScopeKind, ProgramSymbols, TopLevelMode,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use tower_http::cors::{Any, CorsLayer};

/// MessagePack request/response wrapper.
///
/// We implement this internally rather than depending on `axum-msgpack` to keep
/// the server's dependency graph (and compile time) minimal.
#[derive(Debug)]
pub struct MsgPack<T>(pub T);

#[axum::async_trait]
impl<S, T> axum::extract::FromRequest<S> for MsgPack<T>
where
  S: Send + Sync,
  T: serde::de::DeserializeOwned,
{
  type Rejection = (StatusCode, String);

  async fn from_request(
    req: axum::http::Request<axum::body::Body>,
    _state: &S,
  ) -> Result<Self, Self::Rejection> {
    let (parts, body) = req.into_parts();
    if let Some(content_type) = parts.headers.get(axum::http::header::CONTENT_TYPE) {
      let content_type = content_type.as_bytes();
      let ok =
        content_type.starts_with(b"application/msgpack") || content_type.starts_with(b"application/x-msgpack");
      if !ok {
        return Err((
          StatusCode::UNSUPPORTED_MEDIA_TYPE,
          "expected application/msgpack".to_string(),
        ));
      }
    }

    let bytes = axum::body::to_bytes(body, usize::MAX)
      .await
      .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    rmp_serde::from_slice(&bytes).map(MsgPack).map_err(|err| {
      (
        StatusCode::BAD_REQUEST,
        format!("invalid msgpack payload: {err}"),
      )
    })
  }
}

impl<T> axum::response::IntoResponse for MsgPack<T>
where
  T: Serialize,
{
  fn into_response(self) -> axum::response::Response {
    match rmp_serde::to_vec_named(&self.0) {
      Ok(buf) => (
        [(axum::http::header::CONTENT_TYPE, "application/msgpack")],
        buf,
      )
        .into_response(),
      Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
  }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PostCompileReq {
  pub source: String,
  pub is_global: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PostCompileDumpReq {
  pub source: String,
  pub is_global: bool,
  #[serde(default)]
  pub typed: bool,
  #[serde(default)]
  pub semantic_ops: bool,
  #[serde(default = "default_true")]
  pub run_analyses: bool,
}

fn default_true() -> bool {
  true
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PostCompileErrorRes {
  pub ok: bool,
  pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum StableId {
  Number(String),
  Text(String),
}

impl StableId {
  fn number<T: Into<u128>>(value: T) -> Self {
    StableId::Number(value.into().to_string())
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum StableConst {
  Null,
  Undefined,
  BigInt(String),
  Bool(bool),
  Num(f64),
  Str(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StableArg {
  Builtin { value: String },
  Const { value: StableConst },
  Fn { value: u64 },
  Var { value: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StableEffectLocation {
  Heap,
  Foreign { id: StableId },
  Unknown { name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StableEffects {
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub reads: Vec<StableEffectLocation>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub writes: Vec<StableEffectLocation>,
  pub summary: EffectSummary,
  #[serde(default, skip_serializing_if = "is_false")]
  pub unknown: bool,
}

fn is_false(value: &bool) -> bool {
  !*value
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StablePurity {
  Pure,
  ReadOnly,
  Allocating,
  Impure,
}

impl StablePurity {
  fn is_pure(&self) -> bool {
    matches!(self, Self::Pure)
  }

  fn is_impure(&self) -> bool {
    matches!(self, Self::Impure)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StableOwnershipState {
  Owned,
  Borrowed,
  Shared,
  Unknown,
}

impl StableOwnershipState {
  fn is_unknown(&self) -> bool {
    matches!(self, Self::Unknown)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum StableEscapeState {
  NoEscape,
  ArgEscape(usize),
  ReturnEscape,
  GlobalEscape,
  Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StableInstMeta {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub effects: Option<StableEffects>,
  /// Purity of this instruction as inferred from its effect set.
  ///
  /// When omitted, treat as `pure`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub purity: Option<StablePurity>,
  /// For call sites, the inferred purity classification of the callee.
  ///
  /// When omitted, treat as `impure`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub callee_purity: Option<StablePurity>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub result_escape: Option<StableEscapeState>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub ownership: Option<StableOwnershipState>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub type_id: Option<StableId>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub native_layout: Option<StableId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StableInst {
  pub t: String,
  pub tgts: Vec<u32>,
  pub args: Vec<StableArg>,
  pub spreads: Vec<u32>,
  pub labels: Vec<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub bin_op: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub un_op: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub foreign: Option<StableId>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub unknown: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub meta: Option<StableInstMeta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableDebugStep {
  pub name: String,
  #[serde(rename = "bblockOrder")]
  pub bblock_order: Vec<u32>,
  pub bblocks: BTreeMap<u32, Vec<StableInst>>,
  #[serde(rename = "cfgChildren")]
  pub cfg_children: BTreeMap<u32, Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableDebug {
  pub steps: Vec<StableDebugStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StableCfg {
  pub bblock_order: Vec<u32>,
  pub bblocks: BTreeMap<u32, Vec<StableInst>>,
  pub cfg_children: BTreeMap<u32, Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StableFunction {
  pub debug: StableDebug,
  pub cfg: StableCfg,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableProgramSymbol {
  pub id: StableId,
  pub name: String,
  pub scope: StableId,
  pub captured: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableFreeSymbols {
  pub top_level: Vec<StableId>,
  pub functions: Vec<Vec<StableId>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableScope {
  pub id: StableId,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub parent: Option<StableId>,
  pub kind: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub symbols: Vec<StableId>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub children: Vec<StableId>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tdz_bindings: Vec<StableId>,
  pub is_dynamic: bool,
  pub has_direct_eval: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableProgramSymbols {
  pub symbols: Vec<StableProgramSymbol>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub free_symbols: Option<StableFreeSymbols>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub names: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub scopes: Vec<StableScope>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
/// Versioned optimizer snapshot payload.
///
/// This is intentionally an *adjacently tagged* enum (`{"version":"v1","program":{...}}`)
/// instead of an internally tagged enum because `serde_json` can otherwise trip up on
/// our `BTreeMap<u32, _>` fields (JSON object keys are strings).
#[serde(tag = "version", content = "program", rename_all = "snake_case")]
pub enum ProgramDump {
  V1(ProgramDumpV1),
}

impl ProgramDump {
  pub fn into_v1(self) -> ProgramDumpV1 {
    match self {
      ProgramDump::V1(dump) => dump,
    }
  }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ProgramDumpV1 {
  pub functions: Vec<StableFunction>,
  pub top_level: StableFunction,
  pub symbols: Option<StableProgramSymbols>,
}

pub fn compile_program_dump(source: &str, mode: TopLevelMode) -> Result<ProgramDump, Vec<Diagnostic>> {
  let mut program = compile_source(source, mode, true)?;
  // Attach per-instruction metadata (effects/escape/ownership/etc) so the debugger can display it.
  optimize_js::analysis::annotate_program(&mut program);
  Ok(build_dump(program))
}

fn stable_const(value: &Const) -> StableConst {
  match value {
    Const::Null => StableConst::Null,
    Const::Undefined => StableConst::Undefined,
    Const::Bool(v) => StableConst::Bool(*v),
    Const::Num(num) => StableConst::Num(num.0),
    Const::Str(s) => StableConst::Str(s.clone()),
    Const::BigInt(v) => StableConst::BigInt(v.to_string()),
  }
}

fn stable_arg(arg: &Arg) -> StableArg {
  match arg {
    Arg::Builtin(path) => StableArg::Builtin {
      value: path.clone(),
    },
    Arg::Const(value) => StableArg::Const {
      value: stable_const(value),
    },
    Arg::Fn(idx) => StableArg::Fn { value: *idx as u64 },
    Arg::Var(id) => StableArg::Var { value: *id },
  }
}

fn stable_purity(purity: Purity) -> StablePurity {
  match purity {
    Purity::Pure => StablePurity::Pure,
    Purity::ReadOnly => StablePurity::ReadOnly,
    Purity::Allocating => StablePurity::Allocating,
    Purity::Impure => StablePurity::Impure,
  }
}

fn stable_ownership(ownership: optimize_js::il::inst::OwnershipState) -> StableOwnershipState {
  use optimize_js::il::inst::OwnershipState::*;
  match ownership {
    Owned => StableOwnershipState::Owned,
    Borrowed => StableOwnershipState::Borrowed,
    Shared => StableOwnershipState::Shared,
    Unknown => StableOwnershipState::Unknown,
  }
}

fn stable_escape(state: optimize_js::analysis::escape::EscapeState) -> StableEscapeState {
  use optimize_js::analysis::escape::EscapeState::*;
  match state {
    NoEscape => StableEscapeState::NoEscape,
    ArgEscape(i) => StableEscapeState::ArgEscape(i),
    ReturnEscape => StableEscapeState::ReturnEscape,
    GlobalEscape => StableEscapeState::GlobalEscape,
    Unknown => StableEscapeState::Unknown,
  }
}

fn stable_effect_location(loc: &EffectLocation) -> StableEffectLocation {
  match loc {
    EffectLocation::Heap => StableEffectLocation::Heap,
    EffectLocation::Foreign(sym) => StableEffectLocation::Foreign {
      id: StableId::number(sym.raw_id()),
    },
    EffectLocation::Unknown(name) => StableEffectLocation::Unknown { name: name.clone() },
    other => StableEffectLocation::Unknown {
      name: format!("{other:?}"),
    },
  }
}

fn stable_effects(effects: &EffectSet) -> StableEffects {
  StableEffects {
    reads: effects.reads.iter().map(stable_effect_location).collect(),
    writes: effects.writes.iter().map(stable_effect_location).collect(),
    summary: effects.summary,
    unknown: effects.unknown,
  }
}

fn stable_type_id(meta: &InstMeta) -> Option<StableId> {
  #[cfg(feature = "typed")]
  {
    meta.type_id.map(|id| StableId::number(id.0))
  }
  #[cfg(not(feature = "typed"))]
  {
    let _ = meta;
    None
  }
}

fn stable_native_layout(meta: &InstMeta) -> Option<StableId> {
  #[cfg(feature = "typed")]
  {
    meta.native_layout.map(|id| StableId::number(id.0))
  }
  #[cfg(not(feature = "typed"))]
  {
    let _ = meta;
    None
  }
}

fn stable_meta(meta: &InstMeta) -> Option<StableInstMeta> {
  let effects = (!meta.effects.is_default()).then(|| stable_effects(&meta.effects));
  let purity = stable_purity(Purity::from_effects(&meta.effects));
  let purity = (!purity.is_pure()).then_some(purity);
  let callee_purity = stable_purity(meta.callee_purity);
  let callee_purity = (!callee_purity.is_impure()).then_some(callee_purity);
  let result_escape = meta.result_escape.map(stable_escape);
  let ownership = stable_ownership(meta.ownership);
  let ownership = (!ownership.is_unknown()).then_some(ownership);
  let type_id = stable_type_id(meta);
  let native_layout = stable_native_layout(meta);

  let out = StableInstMeta {
    effects,
    purity,
    callee_purity,
    result_escape,
    ownership,
    type_id,
    native_layout,
  };

  (out.effects.is_some()
    || out.purity.is_some()
    || out.callee_purity.is_some()
    || out.result_escape.is_some()
    || out.ownership.is_some()
    || out.type_id.is_some()
    || out.native_layout.is_some())
  .then_some(out)
}

fn stable_inst(inst: &Inst) -> StableInst {
  let bin_op = match inst.t {
    InstTyp::Bin if !matches!(inst.bin_op, BinOp::_Dummy) => Some(format!("{:?}", inst.bin_op)),
    _ => None,
  };
  let un_op = match inst.t {
    InstTyp::Un if !matches!(inst.un_op, UnOp::_Dummy) => Some(format!("{:?}", inst.un_op)),
    _ => None,
  };
  let foreign = match inst.t {
    InstTyp::ForeignLoad | InstTyp::ForeignStore => Some(StableId::number(inst.foreign.raw_id())),
    _ => None,
  };
  let unknown = match inst.t {
    InstTyp::UnknownLoad | InstTyp::UnknownStore if !inst.unknown.is_empty() => {
      Some(inst.unknown.clone())
    }
    _ => None,
  };

  StableInst {
    t: format!("{:?}", inst.t),
    tgts: inst.tgts.clone(),
    args: inst.args.iter().map(stable_arg).collect(),
    spreads: inst.spreads.iter().map(|s| *s as u32).collect(),
    labels: inst.labels.clone(),
    bin_op,
    un_op,
    foreign,
    unknown,
    meta: stable_meta(&inst.meta),
  }
}

fn stable_bblocks<'a, I>(blocks: I) -> BTreeMap<u32, Vec<StableInst>>
where
  I: IntoIterator<Item = (u32, &'a Vec<Inst>)>,
{
  blocks
    .into_iter()
    .map(|(label, insts)| (label, insts.iter().map(stable_inst).collect::<Vec<StableInst>>()))
    .collect()
}

fn stable_cfg(cfg: &Cfg) -> StableCfg {
  StableCfg {
    bblock_order: cfg.graph.calculate_postorder(cfg.entry).0,
    bblocks: stable_bblocks(cfg.bblocks.all()),
    cfg_children: cfg
      .graph
      .labels_sorted()
      .into_iter()
      .filter_map(|label| {
        let children = cfg.graph.children_sorted(label);
        (!children.is_empty()).then_some((label, children))
      })
      .collect(),
  }
}

fn stable_step(name: impl Into<String>, cfg: &Cfg) -> StableDebugStep {
  StableDebugStep {
    name: name.into(),
    bblock_order: cfg.graph.calculate_postorder(cfg.entry).0,
    bblocks: stable_bblocks(cfg.bblocks.all()),
    cfg_children: cfg
      .graph
      .labels_sorted()
      .into_iter()
      .filter_map(|label| {
        let children = cfg.graph.children_sorted(label);
        (!children.is_empty()).then_some((label, children))
      })
      .collect(),
  }
}

fn stable_debug(
  debug: Option<&optimize_js::util::debug::OptimizerDebug>,
  cfg: &Cfg,
) -> StableDebug {
  let mut steps = if let Some(debug) = debug {
    debug
      .steps()
      .iter()
      .map(|step| StableDebugStep {
        name: step.name.clone(),
        bblock_order: step.bblock_order.clone(),
        bblocks: step
          .bblocks
          .iter()
          .map(|(label, insts)| (*label, insts.iter().map(stable_inst).collect()))
          .collect(),
        cfg_children: step.cfg_children.clone(),
      })
      .collect()
  } else {
    Vec::new()
  };

  // Add a final step after the analysis driver so metadata overlays can be inspected.
  steps.push(stable_step("analysis", cfg));

  StableDebug { steps }
}

fn stable_function(func: &ProgramFunction) -> StableFunction {
  StableFunction {
    debug: stable_debug(func.debug.as_ref(), &func.body),
    cfg: stable_cfg(&func.body),
  }
}

fn scope_kind_string(kind: &ProgramScopeKind) -> &'static str {
  match kind {
    ProgramScopeKind::Global => "global",
    ProgramScopeKind::Module => "module",
    ProgramScopeKind::Class => "class",
    ProgramScopeKind::StaticBlock => "static_block",
    ProgramScopeKind::NonArrowFunction => "non_arrow_function",
    ProgramScopeKind::ArrowFunction => "arrow_function",
    ProgramScopeKind::Block => "block",
    ProgramScopeKind::FunctionExpressionName => "function_expression_name",
  }
}

fn stable_scope(scope: &ProgramScope) -> StableScope {
  StableScope {
    id: StableId::number(scope.id.raw_id()),
    parent: scope.parent.map(|p| StableId::number(p.raw_id())),
    kind: scope_kind_string(&scope.kind).to_string(),
    symbols: scope
      .symbols
      .iter()
      .map(|id| StableId::number(id.raw_id()))
      .collect(),
    children: scope
      .children
      .iter()
      .map(|id| StableId::number(id.raw_id()))
      .collect(),
    tdz_bindings: scope
      .tdz_bindings
      .iter()
      .map(|id| StableId::number(id.raw_id()))
      .collect(),
    is_dynamic: scope.is_dynamic,
    has_direct_eval: scope.has_direct_eval,
  }
}

fn stable_symbols(symbols: &ProgramSymbols) -> StableProgramSymbols {
  let mut stable = StableProgramSymbols {
    symbols: symbols
      .symbols
      .iter()
      .map(|symbol| StableProgramSymbol {
        id: StableId::number(symbol.id.raw_id()),
        name: symbol.name.clone(),
        scope: StableId::number(symbol.scope.raw_id()),
        captured: symbol.captured,
      })
      .collect(),
    free_symbols: symbols.free_symbols.as_ref().map(|free| StableFreeSymbols {
      top_level: free
        .top_level
        .iter()
        .map(|id| StableId::number(id.raw_id()))
        .collect(),
      functions: free
        .functions
        .iter()
        .map(|func| func.iter().map(|id| StableId::number(id.raw_id())).collect())
        .collect(),
    }),
    names: symbols.names.clone(),
    scopes: symbols.scopes.iter().map(stable_scope).collect(),
  };

  stable.symbols.sort_by(|a, b| {
    (
      &a.scope,
      &a.name,
      &a.id,
      a.captured.then_some(1usize).unwrap_or(0usize),
    )
      .cmp(&(
        &b.scope,
        &b.name,
        &b.id,
        b.captured.then_some(1usize).unwrap_or(0usize),
      ))
  });
  stable.scopes.sort_by(|a, b| a.id.cmp(&b.id));
  stable
}

fn build_dump(program: optimize_js::Program) -> ProgramDump {
  let functions = program.functions.iter().map(stable_function).collect();
  let top_level = stable_function(&program.top_level);
  let symbols = program.symbols.as_ref().map(stable_symbols);

  ProgramDump::V1(ProgramDumpV1 {
    functions,
    top_level,
    symbols,
  })
}

/// Helper for CLI consumers that accept `TopLevelMode` as a string.
pub fn parse_top_level_mode(value: &str) -> Option<TopLevelMode> {
  TopLevelMode::from_str(value).ok()
}

pub async fn handle_post_compile(
  MsgPack(PostCompileReq { source, is_global }): MsgPack<PostCompileReq>,
) -> Result<MsgPack<ProgramDump>, (StatusCode, MsgPack<PostCompileErrorRes>)> {
  let top_level_mode = if is_global {
    TopLevelMode::Global
  } else {
    TopLevelMode::Module
  };
  match compile_program_dump(&source, top_level_mode) {
    Ok(dump) => Ok(MsgPack(dump)),
    Err(diagnostics) => Err((
      StatusCode::BAD_REQUEST,
      MsgPack(PostCompileErrorRes {
        ok: false,
        diagnostics,
      }),
    )),
  }
}

pub async fn handle_post_compile_dump(
  MsgPack(PostCompileDumpReq {
    source,
    is_global,
    typed,
    semantic_ops,
    run_analyses,
  }): MsgPack<PostCompileDumpReq>,
) -> Result<MsgPack<OptimizeProgramDump>, (StatusCode, MsgPack<PostCompileErrorRes>)> {
  let top_level_mode = if is_global {
    TopLevelMode::Global
  } else {
    TopLevelMode::Module
  };
  let opts = CompileDumpOptions {
    typed,
    semantic_ops,
    run_analyses,
    include_symbols: true,
    include_analyses: false,
    debug: true,
  };
  match compile_source_to_dump(&source, top_level_mode, opts) {
    Ok(dump) => Ok(MsgPack(dump)),
    Err(diagnostics) => Err((
      StatusCode::BAD_REQUEST,
      MsgPack(PostCompileErrorRes {
        ok: false,
        diagnostics,
      }),
    )),
  }
}

pub fn build_app() -> Router {
  Router::new()
    .route("/compile", post(handle_post_compile))
    .route("/compile_dump", post(handle_post_compile_dump))
    .layer(
      CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any),
    )
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
  let mut iter = args.iter();
  while let Some(arg) = iter.next() {
    if arg == flag {
      return iter.next().map(|v| v.to_string());
    }
    if let Some(value) = arg.strip_prefix(&(flag.to_owned() + "=")) {
      return Some(value.to_string());
    }
  }
  None
}

fn read_source(path: Option<PathBuf>) -> io::Result<String> {
  if let Some(path) = path {
    fs::read_to_string(path)
  } else {
    let mut src = String::new();
    io::stdin().read_to_string(&mut src)?;
    Ok(src)
  }
}

pub fn run_snapshot_mode(args: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
  if !args.iter().any(|arg| arg == "--snapshot") {
    return Ok(false);
  }
  let input = arg_value(args, "--input").map(PathBuf::from);
  let output = arg_value(args, "--output").map(PathBuf::from);
  let mode = arg_value(args, "--mode")
    .and_then(|m| TopLevelMode::from_str(&m).ok())
    .unwrap_or(TopLevelMode::Module);

  let source = read_source(input)?;
  match compile_program_dump(&source, mode) {
    Ok(snapshot) => {
      let json = serde_json::to_string_pretty(&snapshot)?;
      if let Some(path) = output {
        fs::write(path, json)?;
      } else {
        println!("{json}");
      }
    }
    Err(diags) => {
      let json = serde_json::to_string_pretty(&PostCompileErrorRes {
        ok: false,
        diagnostics: diags,
      })?;
      let mut stderr = io::stderr();
      stderr.write_all(json.as_bytes())?;
      stderr.write_all(b"\n")?;
      std::process::exit(1);
    }
  }

  Ok(true)
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body;
  use axum::body::Body;
  use axum::http::Request;
  use optimize_js::cfg::cfg::{CfgBBlocks, CfgGraph};
  use rmp_serde::{from_slice, to_vec};
  use tower::ServiceExt;

  #[tokio::test]
  async fn handle_post_compile_succeeds() {
    let MsgPack(res) = handle_post_compile(MsgPack(PostCompileReq {
      source: "let x = 1; let y = x + 2; y;".to_string(),
      is_global: false,
    }))
    .await
    .expect("compile should succeed");

    let res = res.into_v1();
    assert!(res.symbols.is_some(), "symbols should be present");
    assert!(
      !res.top_level.debug.steps.is_empty(),
      "debug steps should be present"
    );
  }

  #[tokio::test]
  async fn symbols_output_is_deterministic() {
    let req = PostCompileReq {
      source: r#"
        let x = 1;
        {
          let y = x + 1;
          y + x;
        }
        let z = x + 3;
        z + x;
      "#
      .to_string(),
      is_global: false,
    };

    let MsgPack(first) = handle_post_compile(MsgPack(req.clone()))
      .await
      .expect("first compile");
    let MsgPack(second) = handle_post_compile(MsgPack(req))
      .await
      .expect("second compile");

    assert_eq!(
      serde_json::to_string(&first).expect("serialize first symbols"),
      serde_json::to_string(&second).expect("serialize second symbols"),
      "symbol output should be deterministic"
    );
  }

  fn build_http_request(uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
      .uri(uri)
      .method("POST")
      .header("content-type", "application/msgpack")
      .body(Body::from(body))
      .unwrap()
  }

  #[tokio::test]
  async fn optimizer_output_matches_snapshot_fixture() {
    let app = build_app();
    let body = to_vec(&PostCompileReq {
      source: include_str!("../tests/fixtures/debug_input.js").to_string(),
      is_global: false,
    })
    .expect("serialize request");
    let response = app
      .oneshot(build_http_request("/compile", body))
      .await
      .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
      .await
      .expect("read body");
    let parsed: ProgramDump = from_slice(&bytes).expect("decode msgpack body");

    // `optimize-js` emits additional `VarAssign` copies in `--features typed` builds to preserve
    // per-expression metadata (see `InstMeta.preserve_var_assign`). This intentionally changes the
    // CFG/IL snapshot emitted by the debugger, so we keep separate fixtures for typed vs untyped
    // builds.
    let snapshot_path = if cfg!(feature = "typed") {
      "tests/fixtures/debug_input.typed.snapshot.json"
    } else {
      "tests/fixtures/debug_input.snapshot.json"
    };
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
      std::fs::write(
        snapshot_path,
        serde_json::to_string_pretty(&parsed).expect("serialize snapshot"),
      )
      .expect("write snapshot");
      return;
    }
    let expected_json = if cfg!(feature = "typed") {
      include_str!("../tests/fixtures/debug_input.typed.snapshot.json")
    } else {
      include_str!("../tests/fixtures/debug_input.snapshot.json")
    };
    let expected: ProgramDump = serde_json::from_str(expected_json).expect("parse snapshot");
    assert_eq!(
      parsed, expected,
      "debugger response should match recorded snapshot"
    );
  }

  #[tokio::test]
  async fn snapshot_endpoint_is_deterministic_over_http() {
    let app = build_app();
    let req = PostCompileReq {
      source: "let a = 1; const b = a + 1; b;".to_string(),
      is_global: false,
    };
    let body = to_vec(&req).expect("serialize request");
    let first = app
      .clone()
      .oneshot(build_http_request("/compile", body.clone()))
      .await
      .expect("first response");
    let second = app
      .oneshot(build_http_request("/compile", body))
      .await
      .expect("second response");

    let first_parsed: ProgramDump =
      from_slice(&body::to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let second_parsed: ProgramDump = from_slice(
      &body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first_parsed, second_parsed);
  }

  #[test]
  fn stable_cfg_uses_cfg_entry_for_bblock_order() {
    let mut graph = CfgGraph::default();
    // Insert two disconnected nodes (0 and 1) so we can verify we start traversal from `cfg.entry`.
    graph.ensure_label(0);
    graph.ensure_label(1);

    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, vec![]);
    bblocks.add(1, vec![]);

    let cfg = Cfg {
      graph,
      bblocks,
      entry: 1,
    };

    let stable = stable_cfg(&cfg);
    assert_eq!(stable.bblock_order, vec![1]);
    // Leaf nodes with no successors are omitted from cfg_children.
    assert!(stable.cfg_children.is_empty());
  }

  #[test]
  fn serde_json_can_roundtrip_u32_map_keys() {
    let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    map.insert(0, vec![]);
    let json = serde_json::to_string(&map).expect("serialize map");
    let parsed: BTreeMap<u32, Vec<u32>> = serde_json::from_str(&json).expect("deserialize map");
    assert_eq!(map, parsed);
  }

  #[test]
  fn program_dump_json_roundtrip_smoke() {
    let dump =
      compile_program_dump("let x = 1; let y = x + 2; y + x;", TopLevelMode::Module)
        .expect("dump");
    let json = serde_json::to_string(&dump).expect("serialize dump");
    // Use `serde_json::Value` first so test failures can be attributed to a specific sub-structure.
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse json");
    let obj = value
      .as_object()
      .expect("ProgramDump must serialize to a JSON object");
    assert_eq!(obj.get("version").and_then(|v| v.as_str()), Some("v1"));
    let program = obj
      .get("program")
      .and_then(|v| v.as_object())
      .expect("ProgramDump.program must be a JSON object");

    // Deserialize subtrees first, then the whole thing.
    let _top_level: StableFunction =
      serde_json::from_value(program.get("top_level").expect("top_level").clone())
        .expect("deserialize top_level");
    let _functions: Vec<StableFunction> =
      serde_json::from_value(program.get("functions").expect("functions").clone())
        .expect("deserialize functions");
    let _symbols: Option<StableProgramSymbols> =
      serde_json::from_value(program.get("symbols").expect("symbols").clone())
        .expect("deserialize symbols");

    let parsed: ProgramDump = serde_json::from_str(&json).expect("deserialize dump");
    assert_eq!(dump, parsed);
  }

  #[test]
  fn serde_json_can_roundtrip_u32_bblocks_map() {
    let mut map: BTreeMap<u32, Vec<StableInst>> = BTreeMap::new();
    map.insert(
      0,
      vec![StableInst {
        t: "VarAssign".to_string(),
        tgts: vec![0],
        args: vec![StableArg::Const {
          value: StableConst::Num(1.0),
        }],
        spreads: vec![],
        labels: vec![],
        bin_op: None,
        un_op: None,
        foreign: None,
        unknown: None,
        meta: None,
      }],
    );
    let json = serde_json::to_string(&map).expect("serialize map");
    let parsed: BTreeMap<u32, Vec<StableInst>> =
      serde_json::from_str(&json).expect("deserialize map");
    assert_eq!(map, parsed);
  }
}
