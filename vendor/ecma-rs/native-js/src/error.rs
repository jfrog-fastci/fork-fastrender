use diagnostics::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NativeJsError {
  #[error("type checking failed")]
  TypecheckFailed { diagnostics: Vec<Diagnostic> },

  #[error("native-js rejected the program")]
  Rejected { diagnostics: Vec<Diagnostic> },

  /// Backwards-compatible error variant for early callers.
  ///
  /// Prefer [`NativeJsError::Unsupported`] when you have source-context diagnostics
  /// to report.
  #[error("unsupported feature: {0}")]
  UnsupportedFeature(String),

  #[error("native-js unsupported feature: {message}")]
  Unsupported {
    message: String,
    diagnostics: Vec<Diagnostic>,
  },

  /// Backwards-compatible error variant for clearer reporting when LLVM is
  /// missing/misconfigured.
  ///
  /// Note: when using `llvm-sys`/`inkwell`, missing LLVM is often detected during
  /// compilation via their build scripts; this variant is primarily intended for
  /// callers that want to surface a more actionable runtime error.
  #[error("{0}")]
  LlvmNotAvailable(String),

  /// Backwards-compatible internal error.
  #[error("internal compiler error: {0}")]
  Internal(String),

  #[error(transparent)]
  Parse(#[from] parse_js::error::SyntaxError),

  #[error(transparent)]
  Codegen(#[from] crate::codegen::CodegenError),

  #[error("failed to load source for {file}: {reason}")]
  FileText { file: String, reason: String },

  #[error("missing HIR lowering for {file} (did you call `Program::check()`?)")]
  MissingHirLowering { file: String },

  #[error("module resolution failed: {from} -> {specifier}")]
  UnresolvedImport { from: String, specifier: String },

  #[error(
    "unsupported import syntax: {from} -> {specifier} (only `import {{ ... }} from` is supported)"
  )]
  UnsupportedImportSyntax { from: String, specifier: String },

  #[error("missing export `{export}` in {file}")]
  MissingExport { file: String, export: String },

  #[error(
    "export `{export}` in {file} is not supported by native-js right now"
  )]
  UnsupportedExport { file: String, export: String },

  #[error("cyclic module dependency detected: {cycle}")]
  ModuleCycle { cycle: String },

  #[error(
    "native-js currently only supports linux for AOT executable emission (target_os={target_os})"
  )]
  UnsupportedPlatform { target_os: String },

  #[error("failed to write output to {path}: {source}")]
  Io {
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },

  #[error("failed to create temporary directory: {0}")]
  TempDirCreateFailed(#[source] std::io::Error),

  #[error("failed to create temporary file: {0}")]
  TempFileCreateFailed(#[source] std::io::Error),

  #[error("failed to persist temporary file: {0}")]
  TempfilePersist(#[from] tempfile::PersistError),

  #[error("failed to spawn linker tool: {0}")]
  LinkerSpawnFailed(#[source] std::io::Error),

  #[error("linker failed: {cmd}\n{stderr}")]
  LinkerFailed { cmd: String, stderr: String },

  #[error("required tool not found in PATH: {0}")]
  ToolNotFound(&'static str),

  #[error("LLVM error: {0}")]
  Llvm(String),
}

impl NativeJsError {
  /// Diagnostics to render for user-facing errors (type errors, strict subset
  /// rejections, etc.).
  pub fn diagnostics(&self) -> Option<&[Diagnostic]> {
    match self {
      NativeJsError::TypecheckFailed { diagnostics } => Some(diagnostics.as_slice()),
      NativeJsError::Rejected { diagnostics } => Some(diagnostics.as_slice()),
      NativeJsError::Unsupported { diagnostics, .. } => Some(diagnostics.as_slice()),
      _ => None,
    }
  }
}
