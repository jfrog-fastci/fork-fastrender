use diagnostics::render::{render_diagnostic_with_options, RenderOptions, SourceProvider};
use diagnostics::{Diagnostic, FileId, Severity};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Write};
use typecheck_ts::Program;

pub const JSON_SCHEMA_VERSION: u32 = 1;

pub fn render_options(color: bool, no_color: bool) -> RenderOptions {
  let color = if color {
    true
  } else if no_color {
    false
  } else {
    std::io::stderr().is_terminal()
  };

  RenderOptions {
    context_lines: 1,
    color,
    ..RenderOptions::default()
  }
}

#[derive(Serialize)]
struct JsonFileEntry {
  id: FileId,
  path: String,
}

#[derive(Serialize)]
struct JsonDiagnosticsOutput {
  schema_version: u32,
  files: Vec<JsonFileEntry>,
  diagnostics: Vec<Diagnostic>,
}

fn collect_json_files(program: Option<&Program>, diagnostics: &[Diagnostic]) -> Vec<JsonFileEntry> {
  fn insert_file(map: &mut BTreeMap<FileId, String>, program: Option<&Program>, file: FileId) {
    if map.contains_key(&file) {
      return;
    }

    let path = program
      .and_then(|program| program.file_key(file))
      .map(|key| key.to_string())
      .unwrap_or_else(|| "<unknown file>".to_string());
    map.insert(file, path);
  }

  let mut map: BTreeMap<FileId, String> = BTreeMap::new();

  if let Some(program) = program {
    for file in program.files() {
      insert_file(&mut map, Some(program), file);
    }
  }

  for diagnostic in diagnostics {
    insert_file(&mut map, program, diagnostic.primary.file);
    for label in &diagnostic.labels {
      insert_file(&mut map, program, label.span.file);
    }
  }

  map
    .into_iter()
    .map(|(id, path)| JsonFileEntry { id, path })
    .collect()
}

pub fn emit_json_diagnostics(
  program: Option<&Program>,
  mut diagnostics: Vec<Diagnostic>,
) -> std::io::Result<bool> {
  diagnostics::sort_diagnostics(&mut diagnostics);
  let has_errors = diagnostics
    .iter()
    .any(|diagnostic| diagnostic.severity == Severity::Error);

  let payload = JsonDiagnosticsOutput {
    schema_version: JSON_SCHEMA_VERSION,
    files: collect_json_files(program, &diagnostics),
    diagnostics,
  };
  let stdout = std::io::stdout();
  let mut handle = stdout.lock();
  serde_json::to_writer_pretty(&mut handle, &payload)
    .map_err(std::io::Error::other)
    .and_then(|()| writeln!(&mut handle))?;
  Ok(has_errors)
}

pub fn emit_diagnostics(
  program: &Program,
  mut diagnostics: Vec<Diagnostic>,
  json: bool,
  render: RenderOptions,
) -> std::io::Result<bool> {
  if json {
    return emit_json_diagnostics(Some(program), diagnostics);
  }

  diagnostics::sort_diagnostics(&mut diagnostics);
  let has_errors = diagnostics
    .iter()
    .any(|diagnostic| diagnostic.severity == Severity::Error);

  let snapshot = snapshot_from_program(program);
  for diagnostic in diagnostics {
    eprintln!(
      "{}",
      render_diagnostic_with_options(&snapshot, &diagnostic, render)
    );
  }

  Ok(has_errors)
}

struct ProgramSourceSnapshot {
  names: HashMap<FileId, String>,
  texts: HashMap<FileId, String>,
}

impl SourceProvider for ProgramSourceSnapshot {
  fn file_name(&self, file: FileId) -> Option<&str> {
    self.names.get(&file).map(|s| s.as_str())
  }

  fn file_text(&self, file: FileId) -> Option<&str> {
    self.texts.get(&file).map(|text| text.as_str())
  }
}

fn snapshot_from_program(program: &Program) -> ProgramSourceSnapshot {
  let mut names = HashMap::new();
  let mut texts = HashMap::new();
  for file in program.files() {
    if let Some(key) = program.file_key(file) {
      names.insert(file, key.to_string());
    }
    if let Some(text) = program.file_text(file) {
      texts.insert(file, text.to_string());
    }
  }
  ProgramSourceSnapshot { names, texts }
}
