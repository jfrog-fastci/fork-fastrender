use crate::tsc::apply_default_tsc_options;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use typecheck_ts::lib_support::{CompilerOptions, JsxMode, LibName, ModuleKind, ScriptTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveSource {
  Line,
  Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDirective {
  /// Canonical, lower-cased directive name (e.g. `filename`, `module`, `target`).
  pub name: String,
  /// Raw value after the colon, trimmed; `None` if omitted or empty.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub value: Option<String>,
  /// Whether the directive came from a line or block comment.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<DirectiveSource>,
  /// 1-based line number within the original harness file.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub line: Option<usize>,
}

/// Normalized view of which `@directives` were seen by the harness.
///
/// This is used to surface when the harness ignores directives (either because
/// they are unknown, or known-but-unsupported).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HarnessDirectiveInfo {
  /// Directive names that are recognized and applied by the harness.
  pub applied: Vec<String>,
  /// Directive names that are not recognized by the harness.
  pub unknown: Vec<String>,
  /// Directive names that are recognized, but not currently supported by the harness.
  pub unsupported: Vec<String>,
}

impl HarnessDirectiveInfo {
  pub fn has_unknown(&self) -> bool {
    !self.unknown.is_empty()
  }

  pub fn has_ignored(&self) -> bool {
    !self.unknown.is_empty() || !self.unsupported.is_empty()
  }

  pub fn ignored_directives_note(&self) -> Option<String> {
    if !self.has_ignored() {
      return None;
    }

    let mut ignored = Vec::new();
    ignored.extend(self.unknown.iter().cloned());
    ignored.extend(self.unsupported.iter().cloned());
    ignored.sort();
    ignored.dedup();

    let displayed: Vec<String> = ignored
      .iter()
      .take(DEFAULT_DIRECTIVE_SAMPLE_LIMIT)
      .map(|name| format!("@{name}"))
      .collect();

    let mut note = format!("ignored directives: {}", displayed.join(", "));
    if ignored.len() > displayed.len() {
      note.push_str(" (…)"); // truncated
    }
    Some(note)
  }
}

pub const DEFAULT_DIRECTIVE_SAMPLE_LIMIT: usize = 10;

/// Aggregated view of ignored harness directives for a full conformance/difftsc run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IgnoredDirectiveSummary {
  pub unknown_count: usize,
  pub unsupported_count: usize,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub unknown_directives: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub unsupported_directives: Vec<String>,
}

impl IgnoredDirectiveSummary {
  pub fn is_empty(&self) -> bool {
    self.unknown_count == 0 && self.unsupported_count == 0
  }

  pub fn human_summary(&self) -> Option<String> {
    if self.is_empty() {
      return None;
    }

    let mut names = Vec::new();
    names.extend(self.unknown_directives.iter().cloned());
    names.extend(self.unsupported_directives.iter().cloned());
    names.sort();
    names.dedup();

    let mut line = format!(
      "ignored directives (unknown={}, unsupported={}): {}",
      self.unknown_count,
      self.unsupported_count,
      names.join(", ")
    );
    let total = self.unknown_count.saturating_add(self.unsupported_count);
    if total > names.len() {
      line.push_str(" (…)"); // truncated
    }
    Some(line)
  }

  pub fn from_harness_options<'a>(
    options: impl IntoIterator<Item = &'a HarnessOptions>,
  ) -> IgnoredDirectiveSummary {
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    let mut unsupported: BTreeSet<String> = BTreeSet::new();
    for opt in options {
      unknown.extend(opt.directives.unknown.iter().cloned());
      unsupported.extend(opt.directives.unsupported.iter().cloned());
    }

    let unknown_count = unknown.len();
    let unsupported_count = unsupported.len();

    let unknown_directives = unknown
      .iter()
      .take(DEFAULT_DIRECTIVE_SAMPLE_LIMIT)
      .map(|name| format!("@{name}"))
      .collect();
    let unsupported_directives = unsupported
      .iter()
      .take(DEFAULT_DIRECTIVE_SAMPLE_LIMIT)
      .map(|name| format!("@{name}"))
      .collect();

    IgnoredDirectiveSummary {
      unknown_count,
      unsupported_count,
      unknown_directives,
      unsupported_directives,
    }
  }
}

/// Parse a harness directive from a single line of text.
///
/// Recognizes both line comments (`// @name: value`) and block comments
/// (`/* @name: value */`). Leading whitespace is ignored. The directive name is
/// lower-cased; the value is trimmed and returned as-is (or `None` if missing).
pub fn parse_directive(raw_line: &str, line_number: usize) -> Option<HarnessDirective> {
  parse_line_comment(raw_line, line_number).or_else(|| parse_block_comment(raw_line, line_number))
}

fn parse_line_comment(raw_line: &str, line_number: usize) -> Option<HarnessDirective> {
  let trimmed = raw_line.trim_start();
  if !trimmed.starts_with("//") {
    return None;
  }

  let content = trimmed.trim_start_matches('/').trim_start();
  parse_directive_content(content, DirectiveSource::Line, line_number)
}

fn parse_block_comment(raw_line: &str, line_number: usize) -> Option<HarnessDirective> {
  let trimmed = raw_line.trim_start();
  if !trimmed.starts_with("/*") {
    return None;
  }

  let mut content = trimmed.trim_start_matches("/*").trim_start();
  if let Some(stripped) = content.strip_suffix("*/") {
    content = stripped.trim_end();
  }

  parse_directive_content(content, DirectiveSource::Block, line_number)
}

fn parse_directive_content(
  content: &str,
  source: DirectiveSource,
  line_number: usize,
) -> Option<HarnessDirective> {
  let content = content.trim_start();
  if !content.starts_with('@') {
    return None;
  }

  // Require the `@name: value` shape to avoid accidentally treating other @tags
  // (like `@ts-ignore`) as harness directives.
  let colon_index = content.find(':')?;
  let (raw_name, raw_value) = content.split_at(colon_index);

  let name = raw_name.trim_start_matches('@').trim();
  if name.is_empty() {
    return None;
  }
  if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
    return None;
  }

  let value = raw_value.trim_start_matches(':').trim();
  let value = if value.is_empty() {
    None
  } else {
    Some(value.to_string())
  };

  Some(HarnessDirective {
    name: name.to_ascii_lowercase(),
    value,
    source: Some(source),
    line: Some(line_number),
  })
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HarnessOptions {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub target: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub module: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub jsx: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub jsx_import_source: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub strict: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub no_implicit_any: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub strict_null_checks: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub strict_function_types: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub exact_optional_property_types: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub no_unchecked_indexed_access: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub no_lib: Option<bool>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub lib: Vec<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub skip_lib_check: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub no_emit: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub use_define_for_class_fields: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub no_emit_on_error: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub declaration: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub module_resolution: Option<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub type_roots: Vec<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub base_url: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub paths: Option<Value>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub types: Vec<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub es_module_interop: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_synthetic_default_imports: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub resolve_json_module: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub experimental_decorators: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub emit_decorator_metadata: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_js: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub check_js: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub module_detection: Option<String>,

  /// Directive names seen while parsing this test case.
  ///
  /// This field is intentionally excluded from JSON output to avoid bloating
  /// large conformance reports. The aggregated, truncated view is surfaced on
  /// the report metadata instead.
  #[serde(skip)]
  pub directives: HarnessDirectiveInfo,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirectiveParseOptions {
  /// When enabled, unknown directives are surfaced as notes for easier triage.
  pub strict: bool,
}

impl DirectiveParseOptions {
  pub const STRICT_DIRECTIVES_ENV: &str = "TYPECHECK_TS_HARNESS_STRICT_DIRECTIVES";

  pub fn from_env() -> Self {
    let strict = std::env::var(Self::STRICT_DIRECTIVES_ENV)
      .ok()
      .and_then(|raw| parse_bool(Some(raw.as_str())))
      .unwrap_or(false);
    Self { strict }
  }
}

#[derive(Debug, Clone, Default)]
pub struct HarnessOptionsParseResult {
  pub options: HarnessOptions,
  pub notes: Vec<String>,
}

impl HarnessOptions {
  pub fn from_directives(directives: &[HarnessDirective]) -> HarnessOptions {
    Self::from_directives_with_options(directives, DirectiveParseOptions::default()).options
  }

  pub fn from_directives_with_options(
    directives: &[HarnessDirective],
    parse_options: DirectiveParseOptions,
  ) -> HarnessOptionsParseResult {
    let mut options = HarnessOptions::default();
    let mut notes = Vec::new();

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unknown_notes = Vec::new();

    let mut applied: BTreeSet<String> = BTreeSet::new();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    let mut unsupported: BTreeSet<String> = BTreeSet::new();

    for directive in directives {
      let value = directive.value.as_deref();
      let mut recognized = true;
      match directive.name.as_str() {
        "filename" => {
          applied.insert(directive.name.clone());
        }
        "target" => {
          options.target = value.map(|v| normalize_scalar(v).to_string());
          applied.insert(directive.name.clone());
        }
        "module" => {
          options.module = value.map(|v| normalize_scalar(v).to_string());
          applied.insert(directive.name.clone());
        }
        "jsx" => {
          options.jsx = value.map(|v| normalize_scalar(v).to_string());
          applied.insert(directive.name.clone());
        }
        "jsximportsource" => {
          options.jsx_import_source = value.map(|v| normalize_scalar(v).to_string());
          applied.insert(directive.name.clone());
        }
        "baseurl" => {
          options.base_url = value.map(|v| normalize_scalar(v).to_string());
          applied.insert(directive.name.clone());
        }
        "strict" => {
          options.strict = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "strictfunctiontypes" => {
          options.strict_function_types = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "noimplicitany" => {
          options.no_implicit_any = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "strictnullchecks" => {
          options.strict_null_checks = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "exactoptionalpropertytypes" => {
          options.exact_optional_property_types = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "nouncheckedindexedaccess" => {
          options.no_unchecked_indexed_access = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "nolib" => {
          options.no_lib = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "lib" => {
          options.lib = parse_list(value);
          applied.insert(directive.name.clone());
        }
        "skiplibcheck" => {
          options.skip_lib_check = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "noemit" => {
          options.no_emit = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "usedefineforclassfields" | "use_define_for_class_fields" => {
          options.use_define_for_class_fields = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "noemitonerror" => {
          options.no_emit_on_error = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "declaration" => {
          options.declaration = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "moduleresolution" => {
          options.module_resolution = value.map(|v| normalize_scalar(v).to_ascii_lowercase());
          applied.insert(directive.name.clone());
        }
        "typeroots" => {
          options.type_roots = parse_list(value);
          applied.insert(directive.name.clone());
        }
        "paths" => {
          options.paths = match value.and_then(|v| parse_jsonish_value(v, &mut notes)) {
            Some(parsed) => Some(parsed),
            None => None,
          };
          applied.insert(directive.name.clone());
        }
        "types" => {
          options.types = parse_list(value);
          applied.insert(directive.name.clone());
        }
        "esmoduleinterop" => {
          options.es_module_interop = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "allowsyntheticdefaultimports" => {
          options.allow_synthetic_default_imports = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "resolvejsonmodule" => {
          options.resolve_json_module = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "experimentaldecorators" => {
          options.experimental_decorators = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "emitdecoratormetadata" => {
          options.emit_decorator_metadata = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "allowjs" => {
          options.allow_js = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "checkjs" => {
          options.check_js = parse_bool(value);
          applied.insert(directive.name.clone());
        }
        "moduledetection" => {
          options.module_detection = value.map(|v| normalize_scalar(v).to_ascii_lowercase());
          applied.insert(directive.name.clone());
        }

        // Known TypeScript harness directives that we recognize but do not
        // currently model in `HarnessOptions` / compiler option mapping.
        //
        // When these appear, the harness will record them as "unsupported" so
        // reports can highlight potentially untrustworthy comparisons.
        "noimplicitthis"
        | "strictpropertyinitialization"
        | "isolatedmodules"
        | "preserveconstenums"
        | "importsnotusedasvalues"
        | "jsxfactory"
        | "jsxfragmentfactory"
        | "reactnamespace"
        | "out"
        | "outdir"
        | "outfile"
        | "sourcemap"
        | "inlinesourcemap"
        | "inlinesources" => {
          unsupported.insert(directive.name.clone());
        }

        _ => {
          recognized = false;
        }
      }

      if recognized {
        if directive.name != "filename" {
          *counts.entry(directive.name.as_str()).or_insert(0) += 1;
        }
      } else {
        unknown.insert(directive.name.clone());
        if parse_options.strict {
          let line = directive.line.unwrap_or(0);
          if let Some(value) = directive.value.as_deref() {
            unknown_notes.push(format!(
              "unrecognized directive @{} at line {} (value: {})",
              directive.name, line, value
            ));
          } else {
            unknown_notes.push(format!(
              "unrecognized directive @{} at line {}",
              directive.name, line
            ));
          }
        }
      }
    }

    for (name, count) in counts {
      if count > 1 {
        notes.push(format!(
          "duplicate @{name} directive ({count} occurrences); last one wins"
        ));
      }
    }

    notes.extend(tsc_only_option_notes(&options));
    if !unknown_notes.is_empty() {
      unknown_notes.sort();
      notes.extend(unknown_notes);
    }

    options.directives = HarnessDirectiveInfo {
      applied: applied.into_iter().collect(),
      unknown: unknown.into_iter().collect(),
      unsupported: unsupported.into_iter().collect(),
    };

    HarnessOptionsParseResult { options, notes }
  }

  /// Convert parsed harness options to `typecheck-ts` compiler options.
  pub fn to_compiler_options(&self) -> CompilerOptions {
    let mut opts = CompilerOptions::default();

    if let Some(target) = self
      .target
      .as_deref()
      .and_then(|raw| parse_script_target(raw))
    {
      opts.target = target;
    }

    if let Some(module) = self
      .module
      .as_deref()
      .and_then(|raw| parse_module_kind(raw))
    {
      opts.module = Some(module);
    }

    if let Some(strict) = self.strict {
      opts.strict_null_checks = strict;
      opts.no_implicit_any = strict;
      opts.strict_function_types = strict;
    }
    if let Some(value) = self.no_implicit_any {
      opts.no_implicit_any = value;
    }
    if let Some(value) = self.strict_null_checks {
      opts.strict_null_checks = value;
    }
    if let Some(value) = self.strict_function_types {
      opts.strict_function_types = value;
    }
    if let Some(value) = self.exact_optional_property_types {
      opts.exact_optional_property_types = value;
    }
    if let Some(value) = self.no_unchecked_indexed_access {
      opts.no_unchecked_indexed_access = value;
    }
    if let Some(value) = self.use_define_for_class_fields {
      opts.use_define_for_class_fields = value;
    }
    if let Some(value) = self.skip_lib_check {
      opts.skip_lib_check = value;
    }
    if let Some(value) = self.no_emit {
      opts.no_emit = value;
    }
    if let Some(value) = self.no_emit_on_error {
      opts.no_emit_on_error = value;
    }
    if let Some(value) = self.declaration {
      opts.declaration = value;
    }
    if let Some(value) = self.module_resolution.as_ref() {
      opts.module_resolution = Some(value.clone());
    }
    if !self.types.is_empty() {
      opts.types = self.types.clone();
    }
    if let Some(value) = self.allow_js {
      opts.allow_js = value;
    }
    if let Some(value) = self.check_js {
      opts.check_js = value;
    }
    if let Some(value) = self.module_detection.as_ref() {
      opts.module_detection = Some(value.clone());
    }
    if let Some(value) = self.jsx_import_source.as_ref() {
      opts.jsx_import_source = Some(value.clone());
    }

    if let Some(mode) = self.jsx.as_deref().and_then(parse_jsx_mode) {
      opts.jsx = Some(mode);
    }

    let mut libs = Vec::new();
    for lib in &self.lib {
      if let Some(parsed) = LibName::from_compiler_option_value(lib) {
        libs.push(parsed);
      }
    }
    opts.libs = libs;
    opts.no_default_lib = self.no_lib.unwrap_or(false);

    opts.normalize()
  }

  pub(crate) fn to_tsc_options_map(&self) -> Map<String, Value> {
    let mut map = Map::new();
    apply_default_tsc_options(&mut map);

    let compiler = self.to_compiler_options();
    map.insert(
      "target".to_string(),
      Value::String(script_target_str(compiler.target).to_string()),
    );
    map.insert(
      "noImplicitAny".to_string(),
      Value::Bool(compiler.no_implicit_any),
    );
    map.insert(
      "strictNullChecks".to_string(),
      Value::Bool(compiler.strict_null_checks),
    );
    map.insert(
      "strictFunctionTypes".to_string(),
      Value::Bool(compiler.strict_function_types),
    );
    map.insert(
      "exactOptionalPropertyTypes".to_string(),
      Value::Bool(compiler.exact_optional_property_types),
    );
    map.insert(
      "noUncheckedIndexedAccess".to_string(),
      Value::Bool(compiler.no_unchecked_indexed_access),
    );
    map.insert(
      "useDefineForClassFields".to_string(),
      Value::Bool(compiler.use_define_for_class_fields),
    );
    if let Some(value) = self.no_lib {
      map.insert("noLib".to_string(), Value::Bool(value));
    }

    if let Some(strict) = self.strict {
      map.insert("strict".to_string(), Value::Bool(strict));
    }

    if let Some(module) = compiler.module {
      map.insert(
        "module".to_string(),
        Value::String(module.option_name().to_string()),
      );
    } else if let Some(module) = &self.module {
      map.insert("module".to_string(), Value::String(module.clone()));
    }
    if let Some(mode) = compiler.jsx {
      map.insert(
        "jsx".to_string(),
        Value::String(jsx_mode_str(mode).to_string()),
      );
    } else if let Some(raw) = &self.jsx {
      map.insert("jsx".to_string(), Value::String(raw.clone()));
    }
    if let Some(source) = self.jsx_import_source.as_ref() {
      map.insert("jsxImportSource".to_string(), Value::String(source.clone()));
    }

    if !self.lib.is_empty() {
      let libs: Vec<String> = compiler
        .libs
        .iter()
        .map(|lib| lib.as_str().to_string())
        .collect();
      map.insert(
        "lib".to_string(),
        Value::Array(libs.into_iter().map(Value::String).collect()),
      );
    }
    if let Some(value) = self.skip_lib_check {
      map.insert("skipLibCheck".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.no_emit {
      map.insert("noEmit".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.no_emit_on_error {
      map.insert("noEmitOnError".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.declaration {
      map.insert("declaration".to_string(), Value::Bool(value));
    }
    if let Some(value) = compiler.module_resolution.as_ref() {
      map.insert("moduleResolution".to_string(), Value::String(value.clone()));
    }
    if !compiler.types.is_empty() {
      map.insert(
        "types".to_string(),
        Value::Array(compiler.types.iter().cloned().map(Value::String).collect()),
      );
    }
    if !self.type_roots.is_empty() {
      map.insert(
        "typeRoots".to_string(),
        Value::Array(self.type_roots.iter().cloned().map(Value::String).collect()),
      );
    }
    if let Some(base_url) = self.base_url.as_ref() {
      map.insert("baseUrl".to_string(), Value::String(base_url.clone()));
    }
    if let Some(paths) = self.paths.as_ref() {
      map.insert("paths".to_string(), paths.clone());
    }
    if let Some(value) = self.es_module_interop {
      map.insert("esModuleInterop".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.allow_synthetic_default_imports {
      map.insert(
        "allowSyntheticDefaultImports".to_string(),
        Value::Bool(value),
      );
    }
    if let Some(value) = self.resolve_json_module {
      map.insert("resolveJsonModule".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.experimental_decorators {
      map.insert("experimentalDecorators".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.emit_decorator_metadata {
      map.insert("emitDecoratorMetadata".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.allow_js {
      map.insert("allowJs".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.check_js {
      map.insert("checkJs".to_string(), Value::Bool(value));
    }
    if let Some(value) = self.module_detection.as_ref() {
      map.insert("moduleDetection".to_string(), Value::String(value.clone()));
    }

    map
  }
}

fn parse_bool(raw: Option<&str>) -> Option<bool> {
  match raw.map(|s| normalize_scalar(s).to_ascii_lowercase()) {
    None => Some(true),
    Some(value) if value.is_empty() => Some(true),
    Some(value) if matches!(value.as_str(), "true" | "1" | "yes" | "on") => Some(true),
    Some(value) if matches!(value.as_str(), "false" | "0" | "no" | "off") => Some(false),
    _ => None,
  }
}

fn normalize_scalar(raw: &str) -> &str {
  raw.split(',').next().unwrap_or(raw).trim()
}

fn parse_list(raw: Option<&str>) -> Vec<String> {
  let Some(raw) = raw else {
    return Vec::new();
  };

  raw
    .split(|c| c == ',' || c == ' ' || c == '\t')
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
    .collect()
}

fn parse_script_target(raw: &str) -> Option<ScriptTarget> {
  match normalize_scalar(raw).to_ascii_lowercase().as_str() {
    "es3" => Some(ScriptTarget::Es3),
    "es5" => Some(ScriptTarget::Es5),
    "es2015" => Some(ScriptTarget::Es2015),
    "es2016" => Some(ScriptTarget::Es2016),
    "es2017" => Some(ScriptTarget::Es2017),
    "es2018" => Some(ScriptTarget::Es2018),
    "es2019" => Some(ScriptTarget::Es2019),
    "es2020" => Some(ScriptTarget::Es2020),
    "es2021" => Some(ScriptTarget::Es2021),
    "es2022" => Some(ScriptTarget::Es2022),
    "esnext" => Some(ScriptTarget::EsNext),
    _ => None,
  }
}

fn script_target_str(target: ScriptTarget) -> &'static str {
  match target {
    ScriptTarget::Es3 => "ES3",
    ScriptTarget::Es5 => "ES5",
    ScriptTarget::Es2015 => "ES2015",
    ScriptTarget::Es2016 => "ES2016",
    ScriptTarget::Es2017 => "ES2017",
    ScriptTarget::Es2018 => "ES2018",
    ScriptTarget::Es2019 => "ES2019",
    ScriptTarget::Es2020 => "ES2020",
    ScriptTarget::Es2021 => "ES2021",
    ScriptTarget::Es2022 => "ES2022",
    ScriptTarget::EsNext => "ESNext",
  }
}

fn parse_jsx_mode(raw: &str) -> Option<JsxMode> {
  match normalize_scalar(raw).to_ascii_lowercase().as_str() {
    "preserve" => Some(JsxMode::Preserve),
    "react" => Some(JsxMode::React),
    "react-jsx" => Some(JsxMode::ReactJsx),
    "react-jsxdev" => Some(JsxMode::ReactJsxdev),
    _ => None,
  }
}

fn jsx_mode_str(mode: JsxMode) -> &'static str {
  match mode {
    JsxMode::Preserve => "preserve",
    JsxMode::React => "react",
    JsxMode::ReactJsx => "react-jsx",
    JsxMode::ReactJsxdev => "react-jsxdev",
  }
}

fn parse_module_kind(raw: &str) -> Option<ModuleKind> {
  match normalize_scalar(raw).to_ascii_lowercase().as_str() {
    "none" => Some(ModuleKind::None),
    "commonjs" => Some(ModuleKind::CommonJs),
    "amd" => Some(ModuleKind::Amd),
    "system" => Some(ModuleKind::System),
    "umd" => Some(ModuleKind::Umd),
    "es6" | "es2015" => Some(ModuleKind::Es2015),
    "es2020" => Some(ModuleKind::Es2020),
    "es2022" => Some(ModuleKind::Es2022),
    "esnext" => Some(ModuleKind::EsNext),
    "node16" => Some(ModuleKind::Node16),
    "nodenext" => Some(ModuleKind::NodeNext),
    _ => None,
  }
}

fn parse_jsonish_value(raw: &str, notes: &mut Vec<String>) -> Option<Value> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  match json5::from_str::<Value>(raw) {
    Ok(value) => Some(value),
    Err(err) => {
      notes.push(format!("failed to parse JSON-ish directive value: {err}"));
      Some(Value::String(raw.to_string()))
    }
  }
}

fn tsc_only_option_notes(options: &HarnessOptions) -> Vec<String> {
  let mut notes = Vec::new();
  if options.allow_js.is_some() {
    notes.push(
      "tsc option allowJs is set via directives but is ignored by the Rust checker".to_string(),
    );
  }
  if options.check_js.is_some() {
    notes.push(
      "tsc option checkJs is set via directives but is ignored by the Rust checker".to_string(),
    );
  }
  if options.module_detection.is_some() {
    notes.push(
      "tsc option moduleDetection is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if !options.type_roots.is_empty() {
    notes.push(
      "tsc option typeRoots is set via directives but is ignored by the Rust checker".to_string(),
    );
  }
  if options.base_url.is_some() {
    notes.push(
      "tsc option baseUrl is set via directives but is ignored by the Rust checker".to_string(),
    );
  }
  if options.paths.is_some() {
    notes.push(
      "tsc option paths is set via directives but is ignored by the Rust checker".to_string(),
    );
  }
  if options.jsx_import_source.is_some() {
    notes.push(
      "tsc option jsxImportSource is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if options.es_module_interop.is_some() {
    notes.push(
      "tsc option esModuleInterop is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if options.allow_synthetic_default_imports.is_some() {
    notes.push(
      "tsc option allowSyntheticDefaultImports is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if options.resolve_json_module.is_some() {
    notes.push(
      "tsc option resolveJsonModule is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if options.experimental_decorators.is_some() {
    notes.push(
      "tsc option experimentalDecorators is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  if options.emit_decorator_metadata.is_some() {
    notes.push(
      "tsc option emitDecoratorMetadata is set via directives but is ignored by the Rust checker"
        .to_string(),
    );
  }
  notes.sort();
  notes
}

#[cfg(test)]
mod tests {
  use super::*;

  fn dir(name: &str, value: Option<&str>) -> HarnessDirective {
    HarnessDirective {
      name: name.to_string(),
      value: value.map(|v| v.to_string()),
      source: Some(DirectiveSource::Line),
      line: None,
    }
  }

  #[test]
  fn parses_line_comment_directive() {
    let directive = parse_directive("  //   @target: ES2022   ", 1).expect("directive");
    assert_eq!(directive.name, "target");
    assert_eq!(directive.value.as_deref(), Some("ES2022"));
    assert_eq!(directive.source, Some(DirectiveSource::Line));
    assert_eq!(directive.line, Some(1));
  }

  #[test]
  fn parses_block_comment_directive() {
    let directive = parse_directive("/* @module: CommonJS */", 4).expect("directive");
    assert_eq!(directive.name, "module");
    assert_eq!(directive.value.as_deref(), Some("CommonJS"));
    assert_eq!(directive.source, Some(DirectiveSource::Block));
    assert_eq!(directive.line, Some(4));
  }

  #[test]
  fn ignores_non_directives() {
    assert!(parse_directive("const x = 1; // not a directive", 1).is_none());
    assert!(parse_directive("// @ts-ignore: next line", 2).is_none());
    assert!(parse_directive("/* missing colon @foo */", 3).is_none());
  }

  #[test]
  fn builds_options_from_directives() {
    let directives = vec![
      dir("target", Some("ES5")),
      dir("strict", Some("false")),
      dir("strict", Some("true")),
      dir("lib", Some("dom,es2015")),
      dir("skiplibcheck", None),
    ];

    let options = HarnessOptions::from_directives(&directives);
    assert_eq!(options.target.as_deref(), Some("ES5"));
    assert_eq!(options.strict, Some(true));
    assert_eq!(options.lib, vec!["dom", "es2015"]);
    assert_eq!(options.skip_lib_check, Some(true));
  }

  #[test]
  fn duplicate_directives_last_one_wins() {
    let directives = vec![dir("module", Some("commonjs")), dir("module", Some("amd"))];
    let parsed =
      HarnessOptions::from_directives_with_options(&directives, DirectiveParseOptions::default());
    assert_eq!(parsed.options.module.as_deref(), Some("amd"));
    assert!(
      parsed
        .notes
        .iter()
        .any(|note| note.contains("duplicate @module directive")),
      "expected duplicate directive note; notes={:?}",
      parsed.notes
    );
  }

  #[test]
  fn maps_directives_to_tsc_and_compiler_options() {
    let directives = vec![
      dir("target", Some("ES5")),
      dir("jsx", Some("react-jsx")),
      dir("strict", Some("true")),
      dir("noimplicitany", Some("false")),
      dir("module", Some("nodenext")),
      dir("lib", Some("dom es2015")),
    ];

    let options = HarnessOptions::from_directives(&directives);
    let tsc = options.to_tsc_options_map();

    assert_eq!(tsc.get("target"), Some(&Value::String("ES5".to_string())));
    assert_eq!(
      tsc.get("jsx"),
      Some(&Value::String("react-jsx".to_string()))
    );
    assert_eq!(tsc.get("strict"), Some(&Value::Bool(true)));
    assert_eq!(tsc.get("noImplicitAny"), Some(&Value::Bool(false)));
    assert_eq!(tsc.get("strictNullChecks"), Some(&Value::Bool(true)));
    assert_eq!(
      tsc.get("module"),
      Some(&Value::String("NodeNext".to_string()))
    );
    assert_eq!(
      tsc
        .get("lib")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
      Some(vec!["dom", "es2015"])
    );

    let compiler = options.to_compiler_options();
    assert_eq!(compiler.target, ScriptTarget::Es5);
    assert_eq!(compiler.jsx, Some(JsxMode::ReactJsx));
    assert_eq!(compiler.module, Some(ModuleKind::NodeNext));
    assert!(compiler.strict_function_types);
    assert!(!compiler.no_implicit_any);
    assert!(compiler.strict_null_checks);
    assert!(!compiler.no_default_lib);
    assert_eq!(
      compiler.libs,
      vec![
        LibName::parse("dom").expect("dom lib"),
        LibName::parse("es2015").expect("es2015 lib")
      ]
    );
  }

  #[test]
  fn normalizes_no_unchecked_indexed_access_name() {
    let directive =
      parse_directive("// @noUncheckedIndexedAccess: true", 1).expect("directive should parse");
    assert_eq!(directive.name, "nouncheckedindexedaccess");
  }

  #[test]
  fn maps_additional_directives_including_no_lib() {
    let directives = vec![
      dir("nolib", None),
      dir("lib", Some("es2020")),
      dir("types", Some("foo bar")),
      dir("declaration", None),
      dir("moduleresolution", Some("Node16")),
      dir("usedefineforclassfields", Some("false")),
    ];

    let options = HarnessOptions::from_directives(&directives);
    let tsc = options.to_tsc_options_map();
    let compiler = options.to_compiler_options();

    assert_eq!(
      compiler.libs,
      vec![LibName::parse("es2020").expect("es2020 lib")]
    );
    assert!(compiler.no_default_lib);
    assert_eq!(compiler.types, vec!["bar".to_string(), "foo".to_string()]);
    assert_eq!(
      compiler.module_resolution.as_deref(),
      Some("node16"),
      "moduleResolution should be normalized"
    );
    assert!(!compiler.use_define_for_class_fields);
    assert_eq!(tsc.get("noLib"), Some(&Value::Bool(true)));
    assert_eq!(
      tsc
        .get("types")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
      Some(vec!["bar", "foo"])
    );
  }

  #[test]
  fn builds_default_tsc_options() {
    let options = HarnessOptions::default();
    let tsc = options.to_tsc_options_map();
    assert_eq!(tsc.get("noEmit"), Some(&Value::Bool(true)));
    assert_eq!(tsc.get("skipLibCheck"), Some(&Value::Bool(true)));
    assert_eq!(tsc.get("pretty"), Some(&Value::Bool(false)));
    assert_eq!(
      tsc.get("target"),
      Some(&Value::String("ES2015".to_string()))
    );
    assert!(
      tsc.get("moduleResolution").is_none(),
      "expected moduleResolution to be omitted unless explicitly specified"
    );
  }

  #[test]
  fn compiler_options_apply_strict_overrides() {
    let mut opts = HarnessOptions::default();
    opts.strict = Some(true);
    opts.strict_null_checks = Some(false);
    let compiler = opts.to_compiler_options();
    assert!(compiler.strict_function_types);
    assert!(compiler.no_implicit_any);
    assert!(!compiler.strict_null_checks);
  }

  #[test]
  fn parses_type_roots_list() {
    let directives = vec![dir("typeroots", Some("/types,/node_modules/@types"))];
    let options = HarnessOptions::from_directives(&directives);
    assert_eq!(options.type_roots, vec!["/types", "/node_modules/@types"]);
    let tsc = options.to_tsc_options_map();
    assert_eq!(
      tsc
        .get("typeRoots")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
      Some(vec!["/types", "/node_modules/@types"])
    );
  }

  #[test]
  fn parses_paths_as_jsonish() {
    let directives = vec![dir("paths", Some(r#"{ "foo/*": ["bar/*"] }"#))];
    let options = HarnessOptions::from_directives(&directives);
    let tsc = options.to_tsc_options_map();
    let paths = tsc
      .get("paths")
      .and_then(|v| v.as_object())
      .expect("paths should parse as object");
    let foo = paths
      .get("foo/*")
      .and_then(|v| v.as_array())
      .expect("foo array");
    assert_eq!(
      foo.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
      vec!["bar/*"]
    );
  }

  #[test]
  fn parses_case_insensitive_directive_names() {
    let dir = parse_directive("// @EsModuleInterop: true,", 1).expect("directive");
    assert_eq!(dir.name, "esmoduleinterop");
    let parsed =
      HarnessOptions::from_directives_with_options(&[dir], DirectiveParseOptions::default());
    assert_eq!(parsed.options.es_module_interop, Some(true));
    let tsc = parsed.options.to_tsc_options_map();
    assert_eq!(tsc.get("esModuleInterop"), Some(&Value::Bool(true)));
  }

  #[test]
  fn strict_directives_surfaces_unknown_directives_as_notes() {
    let dir = parse_directive("// @unknownOption: true", 3).expect("directive");
    let parsed =
      HarnessOptions::from_directives_with_options(&[dir], DirectiveParseOptions { strict: true });
    assert!(
      parsed
        .notes
        .iter()
        .any(|note| note.contains("unrecognized directive @unknownoption")),
      "expected unknown directive note; notes={:?}",
      parsed.notes
    );
  }

  #[test]
  fn tracks_unknown_and_unsupported_directives() {
    let directives = vec![
      dir("target", Some("ES5")),
      dir("noimplicitthis", Some("true")),
      dir("madeup", Some("true")),
    ];
    let options = HarnessOptions::from_directives(&directives);
    assert!(options.directives.applied.iter().any(|d| d == "target"));
    assert_eq!(options.directives.unsupported, vec!["noimplicitthis"]);
    assert_eq!(options.directives.unknown, vec!["madeup"]);
  }

  #[test]
  fn maps_module_detection_and_js_directives() {
    let directives = vec![
      dir("allowjs", Some("true")),
      dir("checkjs", Some("false")),
      dir("moduledetection", Some("force")),
      dir("jsximportsource", Some("preact")),
    ];
    let options = HarnessOptions::from_directives(&directives);
    let tsc = options.to_tsc_options_map();
    let compiler = options.to_compiler_options();

    assert_eq!(tsc.get("allowJs"), Some(&Value::Bool(true)));
    assert_eq!(tsc.get("checkJs"), Some(&Value::Bool(false)));
    assert_eq!(
      tsc.get("moduleDetection"),
      Some(&Value::String("force".to_string()))
    );
    assert_eq!(
      tsc.get("jsxImportSource"),
      Some(&Value::String("preact".to_string()))
    );

    assert!(compiler.allow_js);
    assert!(!compiler.check_js);
    assert_eq!(
      compiler.module_detection.as_deref(),
      Some("force"),
      "module_detection should be normalized"
    );
    assert_eq!(compiler.jsx_import_source.as_deref(), Some("preact"));
  }

  #[test]
  fn accepts_use_define_for_class_fields_alias() {
    let directives = vec![dir("use_define_for_class_fields", Some("false"))];
    let options = HarnessOptions::from_directives(&directives);
    assert_eq!(options.use_define_for_class_fields, Some(false));

    let compiler = options.to_compiler_options();
    assert!(!compiler.use_define_for_class_fields);
  }
}
