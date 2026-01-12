use clap::ValueEnum;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use typecheck_ts::{FileId, Program};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum)]
pub enum EmitKindArg {
  #[value(name = "llvm", alias = "ll", alias = "ir", alias = "llvm-ir")]
  LlvmIr,
  #[value(name = "bc", alias = "bitcode")]
  Bitcode,
  #[value(name = "obj", alias = "o", alias = "object")]
  Object,
  #[value(name = "asm", alias = "s", alias = "assembly")]
  Assembly,
  #[value(
    name = "exe",
    alias = "out",
    alias = "bin",
    alias = "exec",
    alias = "executable"
  )]
  Executable,
  #[value(name = "hir")]
  Hir,
}

impl EmitKindArg {
  pub fn as_native_emit_kind(self) -> Option<native_js::EmitKind> {
    match self {
      EmitKindArg::LlvmIr => Some(native_js::EmitKind::LlvmIr),
      EmitKindArg::Bitcode => Some(native_js::EmitKind::Bitcode),
      EmitKindArg::Object => Some(native_js::EmitKind::Object),
      EmitKindArg::Assembly => Some(native_js::EmitKind::Assembly),
      EmitKindArg::Executable => Some(native_js::EmitKind::Executable),
      EmitKindArg::Hir => None,
    }
  }

  pub fn extension(self) -> &'static str {
    match self {
      EmitKindArg::LlvmIr => "ll",
      EmitKindArg::Bitcode => "bc",
      EmitKindArg::Object => "o",
      EmitKindArg::Assembly => "s",
      EmitKindArg::Executable => "",
      EmitKindArg::Hir => "hir.txt",
    }
  }

  pub fn file_name(self, stem: &str) -> String {
    let ext = self.extension();
    if ext.is_empty() {
      stem.to_string()
    } else {
      format!("{stem}.{ext}")
    }
  }
}

pub fn output_stem_from_path(path: &Path) -> Result<String, String> {
  let stem = path
    .file_stem()
    .or_else(|| path.file_name())
    .ok_or_else(|| format!("failed to derive output stem from {}", path.display()))?;
  Ok(stem.to_string_lossy().to_string())
}

/// Compute output paths for a set of emits.
///
/// Rules:
/// - When multiple emits are requested, `out_dir` must be provided and output paths are written
///   into that directory.
/// - When `out_dir` is provided, `output` is interpreted as the *stem* (filename without
///   extension) to use when naming outputs.
/// - When `out_dir` is not provided, exactly one emit must be requested and `output` is used as the
///   full output path.
pub fn compute_emit_paths(
  emits: &[EmitKindArg],
  out_dir: Option<&Path>,
  output: Option<&Path>,
  default_stem: &str,
) -> Result<BTreeMap<EmitKindArg, PathBuf>, String> {
  let unique: BTreeSet<EmitKindArg> = emits.iter().copied().collect();
  if unique.is_empty() {
    return Err("expected at least one --emit kind".to_string());
  }

  if unique.len() > 1 && out_dir.is_none() {
    return Err("multiple --emit kinds require --out-dir <DIR>".to_string());
  }

  let mut out = BTreeMap::new();
  if let Some(dir) = out_dir {
    let stem = match output {
      Some(path) => output_stem_from_path(path)?,
      None => default_stem.to_string(),
    };
    for kind in unique {
      out.insert(kind, dir.join(kind.file_name(&stem)));
    }
    return Ok(out);
  }

  let kind = *unique.iter().next().expect("unique non-empty");
  let Some(output) = output else {
    return Err("missing -o/--output (or --out-dir) for emit output".to_string());
  };
  out.insert(kind, output.to_path_buf());
  Ok(out)
}

pub fn format_hir_dump(program: &Program) -> String {
  // Reachable file IDs are deterministic, but for human readability we sort by the normalized file
  // key (which is a normalized fs path for disk-backed hosts).
  let mut files: Vec<(String, FileId)> = program
    .reachable_files()
    .into_iter()
    .map(|file| {
      let key = program
        .file_key(file)
        .map(|key| key.as_str().to_string())
        .unwrap_or_else(|| format!("<unknown file {file:?}>"));
      (key, file)
    })
    .collect();
  files.sort_by(|(a, _), (b, _)| a.cmp(b));

  let mut out = String::new();
  for (key, file) in files {
    let _ = writeln!(&mut out, "===== {key} =====");
    match program.hir_lowered(file) {
      Some(lowered) => {
        let _ = writeln!(&mut out, "{lowered:#?}");
      }
      None => {
        let _ = writeln!(&mut out, "<missing HIR lowering>");
      }
    }
    out.push('\n');
  }
  out
}
