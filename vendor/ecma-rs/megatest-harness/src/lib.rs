use anyhow::{anyhow, Context, Result};
use diagnostics::FileId;
use hir_js::FileKind as HirFileKind;
use parse_js::{Dialect, ParseOptions, SourceType};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const BASELINE_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Baseline {
  pub version: u32,
  pub files: BTreeMap<String, BaselineEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineEntry {
  pub source_sha256: String,
  pub parse: ParseSummary,
  pub hir: HirSummary,
  pub optimize: OptimizeOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseSummary {
  pub top_level_stmts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HirSummary {
  pub defs: usize,
  pub bodies: usize,
  pub exprs: usize,
  pub stmts: usize,
  pub pats: usize,
  pub type_exprs: usize,
  pub type_members: usize,
  pub type_params: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptimizeSummary {
  /// Total number of compiled functions, including the implicit top-level.
  pub functions: usize,
  pub instructions: usize,
  pub dom_calculations: usize,
  /// SHA256 of the emitted JS from `optimize-js`'s deterministic `program_to_js` decompiler.
  pub decompiled_js_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum OptimizeOutcome {
  Ok { summary: OptimizeSummary },
  Err { diagnostics: Vec<DiagnosticSummary> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticSummary {
  pub code: String,
  pub message: String,
  pub start: u32,
  pub end: u32,
}

#[derive(Clone, Debug)]
pub struct Fixture {
  /// Path relative to `vendor/ecma-rs/megatest/` (always uses `/` separators).
  pub name: String,
  pub path: PathBuf,
}

impl fmt::Display for Fixture {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.name)
  }
}

pub fn megatest_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../megatest")
}

pub fn baselines_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("baselines")
}

pub fn baseline_path() -> PathBuf {
  baselines_root().join("baseline.json")
}

pub fn discover_fixtures() -> Result<Vec<Fixture>> {
  let root = megatest_root();
  let mut fixtures: Vec<Fixture> = Vec::new();
  for entry in WalkDir::new(&root).follow_links(false) {
    let entry = entry.context("walk megatest/")?;
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("js") {
      continue;
    }
    let rel = path
      .strip_prefix(&root)
      .context("strip megatest root prefix")?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    fixtures.push(Fixture {
      name: rel,
      path: path.to_path_buf(),
    });
  }
  fixtures.sort_by(|a, b| a.name.cmp(&b.name));
  Ok(fixtures)
}

pub fn megatest_filter() -> Option<String> {
  std::env::var("MEGATEST_FILTER").ok().filter(|s| !s.is_empty())
}

pub fn filter_fixtures(fixtures: Vec<Fixture>, filter: Option<&str>) -> Vec<Fixture> {
  let Some(filter) = filter else {
    return fixtures;
  };
  fixtures
    .into_iter()
    .filter(|fixture| fixture.name.contains(filter))
    .collect()
}

pub fn load_baseline() -> Result<Baseline> {
  let path = baseline_path();
  let text = std::fs::read_to_string(&path)
    .with_context(|| format!("read baseline file at {}", path.display()))?;
  let baseline: Baseline =
    serde_json::from_str(&text).with_context(|| format!("parse JSON at {}", path.display()))?;
  if baseline.version != BASELINE_VERSION {
    return Err(anyhow!(
      "baseline version mismatch (expected {}, got {})",
      BASELINE_VERSION,
      baseline.version
    ));
  }
  Ok(baseline)
}

pub fn write_baseline(baseline: &Baseline) -> Result<()> {
  let path = baseline_path();
  let json = serde_json::to_string_pretty(baseline).context("serialize baseline JSON")?;
  std::fs::create_dir_all(baselines_root()).context("create baselines/ dir")?;
  std::fs::write(&path, format!("{json}\n"))
    .with_context(|| format!("write baseline file at {}", path.display()))?;
  Ok(())
}

pub fn read_source(path: &Path) -> Result<String> {
  std::fs::read_to_string(path).with_context(|| format!("read source {}", path.display()))
}

pub fn source_sha256(source: &str) -> String {
  let hash = sha2::Sha256::digest(source.as_bytes());
  hex_encode(&hash)
}

fn bytes_sha256(bytes: &[u8]) -> String {
  let hash = sha2::Sha256::digest(bytes);
  hex_encode(&hash)
}

fn hex_encode(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for &b in bytes {
    use std::fmt::Write;
    let _ = write!(&mut out, "{:02x}", b);
  }
  out
}

pub fn parse_and_lower(source: &str) -> Result<(ParseSummary, HirSummary)> {
  let parsed = parse_js::parse_with_options(
    source,
    ParseOptions {
      // The megatest corpus is `.js`; prefer strict ECMAScript parsing (no recovery) so we
      // catch parser drift early.
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    },
  )
  .map_err(|err| anyhow!("{err:?}"))?;

  let parse_summary = ParseSummary {
    top_level_stmts: parsed.stx.body.len(),
  };

  let lowered = hir_js::lower_file(FileId(0), HirFileKind::Js, &parsed);
  let (exprs, stmts, pats) = lowered
    .bodies
    .iter()
    .fold((0usize, 0usize, 0usize), |(exprs, stmts, pats), body| {
      (
        exprs + body.exprs.len(),
        stmts + body.stmts.len(),
        pats + body.pats.len(),
      )
    });

  let (type_exprs, type_members, type_params) = lowered.types.values().fold(
    (0usize, 0usize, 0usize),
    |(type_exprs, type_members, type_params), arenas| {
      (
        type_exprs + arenas.type_exprs.len(),
        type_members + arenas.type_members.len(),
        type_params + arenas.type_params.len(),
      )
    },
  );

  let hir_summary = HirSummary {
    defs: lowered.defs.len(),
    bodies: lowered.bodies.len(),
    exprs,
    stmts,
    pats,
    type_exprs,
    type_members,
    type_params,
  };

  Ok((parse_summary, hir_summary))
}

pub fn optimize(source: &str) -> Result<OptimizeOutcome> {
  match optimize_js::compile_source(source, optimize_js::TopLevelMode::Module, false) {
    Ok(program) => {
      let functions = 1 + program.functions.len();
      let instructions = count_insts(&program.top_level.body)
        + program
          .functions
          .iter()
          .map(|func| count_insts(&func.body))
          .sum::<usize>();
      let dom_calculations = program.top_level.stats.dom_calculations
        + program
          .functions
          .iter()
          .map(|func| func.stats.dom_calculations)
          .sum::<usize>();
      let decompiled = optimize_js::program_to_js(
        &program,
        &optimize_js::DecompileOptions::default(),
        emit_js::EmitOptions::minified(),
      )
      .map_err(|err| anyhow!("program_to_js failed: {err:?}"))?;
      let decompiled_js_sha256 = bytes_sha256(&decompiled);

      Ok(OptimizeOutcome::Ok {
        summary: OptimizeSummary {
          functions,
          instructions,
          dom_calculations,
          decompiled_js_sha256,
        },
      })
    }
    Err(mut diagnostics) => {
      diagnostics.sort_by(|a, b| {
        a.primary
          .file
          .cmp(&b.primary.file)
          .then(a.primary.range.start.cmp(&b.primary.range.start))
          .then(a.primary.range.end.cmp(&b.primary.range.end))
          .then(a.code.cmp(&b.code))
          .then(a.message.cmp(&b.message))
      });
      Ok(OptimizeOutcome::Err {
        diagnostics: diagnostics
          .into_iter()
          .map(|d| DiagnosticSummary {
            code: d.code.as_str().to_string(),
            message: d.message,
            start: d.primary.range.start,
            end: d.primary.range.end,
          })
          .collect(),
      })
    }
  }
}

fn count_insts(cfg: &optimize_js::cfg::cfg::Cfg) -> usize {
  cfg.bblocks.all().map(|(_, bblock)| bblock.len()).sum()
}

pub fn compute_baseline_entry(source: &str) -> Result<BaselineEntry> {
  let source_sha256 = source_sha256(source);
  let (parse, hir) = parse_and_lower(source)?;
  let optimize = optimize(source)?;
  Ok(BaselineEntry {
    source_sha256,
    parse,
    hir,
    optimize,
  })
}
