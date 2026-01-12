use crate::output;
use diagnostics::render::{RenderOptions, SourceProvider};
use diagnostics::{host_error, Diagnostic, FileId, Severity, Span, TextRange};
use native_js::{codegen::CodegenError, codes, NativeJsError};
use typecheck_ts::Program;

#[derive(Clone, Copy, Debug)]
pub struct DiagFlags {
  pub json: bool,
  pub color: bool,
  pub no_color: bool,
}

impl DiagFlags {
  pub fn render_options(&self) -> RenderOptions {
    output::render_options(self.color, self.no_color)
  }
}

pub fn exit_code_for_diagnostics(diagnostics: &[Diagnostic]) -> u8 {
  let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
  if !has_errors {
    return 0;
  }

  let has_internal = diagnostics.iter().any(|d| {
    d.severity == Severity::Error && (d.code.as_str().starts_with("ICE") || d.code.as_str().starts_with("HOST"))
  });
  if has_internal { 2 } else { 1 }
}

pub fn emit_diagnostics_for_program(
  program: &Program,
  diagnostics: Vec<Diagnostic>,
  flags: DiagFlags,
) -> u8 {
  let render = flags.render_options();
  if let Err(err) = output::emit_diagnostics(program, diagnostics.clone(), flags.json, render) {
    // Best-effort fallback: if we can't write diagnostics, at least return the
    // right exit code. Avoid printing to stderr in JSON mode.
    if !flags.json {
      eprintln!("failed to write diagnostics: {err}");
    }
  }
  exit_code_for_diagnostics(&diagnostics)
}

pub fn emit_diagnostics_for_source(
  source: &impl SourceProvider,
  diagnostics: Vec<Diagnostic>,
  flags: DiagFlags,
) -> u8 {
  let render = flags.render_options();
  if let Err(err) =
    output::emit_diagnostics_with_source(source, diagnostics.clone(), flags.json, render)
  {
    if !flags.json {
      eprintln!("failed to write diagnostics: {err}");
    }
  }
  exit_code_for_diagnostics(&diagnostics)
}

pub fn emit_success_json() {
  let _ = output::emit_json_diagnostics(None, Vec::new());
}

#[derive(Default)]
pub struct SingleFileSource {
  pub name: Option<String>,
  pub text: Option<String>,
}

impl SourceProvider for SingleFileSource {
  fn file_name(&self, file: FileId) -> Option<&str> {
    if file != FileId(0) {
      return None;
    }
    self.name.as_deref()
  }

  fn file_text(&self, file: FileId) -> Option<&str> {
    if file != FileId(0) {
      return None;
    }
    self.text.as_deref()
  }
}

pub fn diagnostics_from_native_js_error(err: &NativeJsError, default_file: FileId) -> Vec<Diagnostic> {
  if let Some(diags) = err.diagnostics() {
    return diags.to_vec();
  }

  match err {
    NativeJsError::Parse(parse) => vec![syntax_error_to_diagnostic(default_file, parse)],
    NativeJsError::ParseFile {
      file_id, error, ..
    } => vec![syntax_error_to_diagnostic(*file_id, error)],
    NativeJsError::Codegen(codegen) => vec![codegen_error_to_diagnostic(default_file, codegen)],
    NativeJsError::CodegenFile {
      file_id, error, ..
    } => vec![codegen_error_to_diagnostic(*file_id, error)],

    // Host/environment errors.
    NativeJsError::Io { .. }
    | NativeJsError::TempDirCreateFailed(_)
    | NativeJsError::TempFileCreateFailed(_)
    | NativeJsError::TempfilePersist(_)
    | NativeJsError::LinkerSpawnFailed(_)
    | NativeJsError::LinkerFailed { .. }
    | NativeJsError::ToolNotFound(_)
    | NativeJsError::LlvmNotAvailable(_)
    | NativeJsError::Llvm(_)
    | NativeJsError::UnsupportedPlatform { .. }
    | NativeJsError::FileText { .. } => vec![host_error(None, err.to_string())],

    NativeJsError::Internal(_) => vec![host_error(None, err.to_string())],

    // Everything else is user-facing but may not have spans/codes yet.
    other => vec![Diagnostic::error(
      codes::UNSUPPORTED_EXPR.as_str(),
      other.to_string(),
      Span::new(default_file, TextRange::new(0, 0)),
    )],
  }
}

fn syntax_error_to_diagnostic(file: FileId, err: &parse_js::error::SyntaxError) -> Diagnostic {
  let start = err.loc.start_u32();
  let end = err.loc.end_u32();
  let primary = Span::new(file, TextRange::new(start, end));
  let mut diagnostic = Diagnostic::error(err.typ.code(), err.typ.message(err.actual_token), primary);

  // Replicate parse-js's optional "expected ..." note for a few variants.
  match err.typ {
    parse_js::error::SyntaxErrorType::ExpectedNotFound => {
      diagnostic.push_note("expected a token here");
    }
    parse_js::error::SyntaxErrorType::ExpectedSyntax(expected) => {
      diagnostic.push_note(format!("expected {expected}"));
    }
    parse_js::error::SyntaxErrorType::RequiredTokenNotFound(token) => {
      diagnostic.push_note(format!("expected token {token:?}"));
    }
    _ => {}
  }

  if let Some(actual) = err.actual_token {
    diagnostic.push_note(format!("found token: {actual:?}"));
  }
  diagnostic
}

fn codegen_error_to_diagnostic(file: FileId, err: &CodegenError) -> Diagnostic {
  match err {
    CodegenError::UnsupportedStmt { loc } => codes::LEGACY_UNSUPPORTED_STMT.error(
      "unsupported statement in native-js legacy emitter",
      Span::new(file, TextRange::new(loc.start_u32(), loc.end_u32())),
    ),
    CodegenError::UnsupportedExpr { loc } => codes::UNSUPPORTED_EXPR.error(
      "unsupported expression in native-js legacy emitter",
      Span::new(file, TextRange::new(loc.start_u32(), loc.end_u32())),
    ),
    CodegenError::UnsupportedOperator { op, loc } => codes::LEGACY_UNSUPPORTED_OPERATOR.error(
      format!("unsupported operator `{op:?}` in native-js legacy emitter"),
      Span::new(file, TextRange::new(loc.start_u32(), loc.end_u32())),
    ),
    CodegenError::BuiltinsDisabled { loc } => codes::BUILTINS_DISABLED.error(
      "builtins are disabled by compiler options",
      Span::new(file, TextRange::new(loc.start_u32(), loc.end_u32())),
    ),
    CodegenError::TypeError { message, loc } => codes::LEGACY_TYPE_ERROR.error(
      message.clone(),
      Span::new(file, TextRange::new(loc.start_u32(), loc.end_u32())),
    ),
  }
}
