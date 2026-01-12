use hir_js::{BinaryOp, BodyId, ExprId, ExprKind, Literal, ObjectKey};
use knowledge_base::{Api, ApiId, KnowledgeBase};
use std::collections::HashMap;

use crate::{resolve_api_use, ApiUseKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringEncoding {
  Ascii,
  Latin1,
  Utf8,
  Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodingResult {
  /// Per-expression encoding inference, indexed by `ExprId`.
  pub encodings: Vec<StringEncoding>,
}

pub fn analyze_string_encodings(
  result: &hir_js::LowerResult,
  kb: &KnowledgeBase,
) -> HashMap<hir_js::BodyId, EncodingResult> {
  analyze_string_encodings_impl(result, kb, UntypedOracle)
}

#[cfg(feature = "typed")]
pub fn analyze_string_encodings_typed(
  result: &hir_js::LowerResult,
  kb: &KnowledgeBase,
  types: &impl crate::types::TypeProvider,
) -> HashMap<hir_js::BodyId, EncodingResult> {
  analyze_string_encodings_impl(result, kb, TypedOracle { types })
}

fn analyze_string_encodings_impl<O: TypeOracle + Copy>(
  result: &hir_js::LowerResult,
  kb: &KnowledgeBase,
  oracle: O,
) -> HashMap<hir_js::BodyId, EncodingResult> {
  let mut out = HashMap::new();
  for (body_id, idx) in result.body_index.iter() {
    let body = &result.bodies[*idx];
    let mut analyzer = BodyAnalyzer {
      lowered: result,
      body_id: *body_id,
      body,
      kb,
      oracle,
      cache: vec![None; body.exprs.len()],
    };

    // Force evaluation for all expressions to populate `cache`.
    for expr_idx in 0..body.exprs.len() {
      analyzer.encoding_of(ExprId(expr_idx as u32));
    }

    let encodings = analyzer
      .cache
      .into_iter()
      .map(|enc| enc.unwrap_or(StringEncoding::Unknown))
      .collect();

    out.insert(*body_id, EncodingResult { encodings });
  }
  out
}

#[derive(Clone, Copy)]
struct UntypedOracle;

trait TypeOracle {
  fn receiver_is_string(&self, body: BodyId, expr: ExprId) -> bool;
  fn receiver_is_number(&self, body: BodyId, expr: ExprId) -> bool;
  fn expr_is_string(&self, body: BodyId, expr: ExprId) -> bool;

  #[cfg(feature = "typed")]
  fn type_provider(&self) -> Option<&dyn crate::types::TypeProvider> {
    None
  }
}

impl TypeOracle for UntypedOracle {
  fn receiver_is_string(&self, _body: BodyId, _expr: ExprId) -> bool {
    false
  }

  fn receiver_is_number(&self, _body: BodyId, _expr: ExprId) -> bool {
    false
  }

  fn expr_is_string(&self, _body: BodyId, _expr: ExprId) -> bool {
    false
  }
}

#[cfg(feature = "typed")]
#[derive(Clone, Copy)]
struct TypedOracle<'a> {
  types: &'a dyn crate::types::TypeProvider,
}

#[cfg(feature = "typed")]
impl TypeOracle for TypedOracle<'_> {
  fn receiver_is_string(&self, body: BodyId, expr: ExprId) -> bool {
    type_is_string(self.types, body, expr)
  }

  fn receiver_is_number(&self, body: BodyId, expr: ExprId) -> bool {
    type_is_number(self.types, body, expr)
  }

  fn expr_is_string(&self, body: BodyId, expr: ExprId) -> bool {
    type_is_string(self.types, body, expr)
  }

  fn type_provider(&self) -> Option<&dyn crate::types::TypeProvider> {
    Some(self.types)
  }
}

#[cfg(feature = "typed")]
fn type_is_string(types: &dyn crate::types::TypeProvider, body: BodyId, expr: ExprId) -> bool {
  types.expr_is_string(body, expr)
}

#[cfg(feature = "typed")]
fn type_is_number(types: &dyn crate::types::TypeProvider, body: BodyId, expr: ExprId) -> bool {
  use crate::types::TypeKindSummary;

  let Some(ty) = types.expr_type(body, expr) else {
    return false;
  };

  match types.type_kind(ty) {
    Some(TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_)) => true,
    Some(TypeKindSummary::Ref { def, .. }) => types.def_name(def).as_deref() == Some("Number"),
    _ => false,
  }
}

struct BodyAnalyzer<'a, O> {
  lowered: &'a hir_js::LowerResult,
  body_id: BodyId,
  body: &'a hir_js::Body,
  kb: &'a KnowledgeBase,
  oracle: O,
  cache: Vec<Option<StringEncoding>>,
}

impl<O: TypeOracle> BodyAnalyzer<'_, O> {
  fn encoding_of(&mut self, expr_id: ExprId) -> StringEncoding {
    let idx = expr_id.0 as usize;
    if let Some(Some(encoding)) = self.cache.get(idx).copied() {
      return encoding;
    }
    let encoding = self.encoding_of_uncached(expr_id);
    if let Some(slot) = self.cache.get_mut(idx) {
      *slot = Some(encoding);
    }
    encoding
  }

  fn encoding_of_uncached(&mut self, expr_id: ExprId) -> StringEncoding {
    let Some(expr) = self.body.exprs.get(expr_id.0 as usize) else {
      return StringEncoding::Unknown;
    };
    match &expr.kind {
      ExprKind::Literal(Literal::String(string_lit)) => encoding_of_string_literal(string_lit),
      ExprKind::Template(template) => self.encoding_of_template(template),

      // Pure wrappers around the underlying expression value.
      ExprKind::TypeAssertion { expr, .. } => self.encoding_of(*expr),
      ExprKind::Instantiation { expr, .. } => self.encoding_of(*expr),
      ExprKind::NonNull { expr } => self.encoding_of(*expr),
      ExprKind::Satisfies { expr, .. } => self.encoding_of(*expr),

      ExprKind::Binary { op, left, right } => match op {
        BinaryOp::Add => self.encoding_of_add(expr_id, *left, *right),
        _ => StringEncoding::Unknown,
      },

      ExprKind::Call(call) => self.encoding_of_call(expr_id, call),
      ExprKind::Member(_) => self.encoding_of_member(expr_id),

      // Everything else is either not a string or requires more context to
      // reason about.
      _ => StringEncoding::Unknown,
    }
  }

  fn encoding_of_template(&mut self, template: &hir_js::TemplateLiteral) -> StringEncoding {
    let mut encoding = encoding_of_literal_segment(&template.head);
    for span in template.spans.iter() {
      encoding = join_encodings(encoding, self.encoding_of(span.expr));
      encoding = join_encodings(encoding, encoding_of_literal_segment(&span.literal));
    }
    encoding
  }

  fn encoding_of_add(&mut self, expr: ExprId, left: ExprId, right: ExprId) -> StringEncoding {
    if !self.oracle.expr_is_string(self.body_id, expr) {
      return StringEncoding::Unknown;
    }

    let left_enc = self.encoding_of(left);
    let right_enc = self.encoding_of(right);
    join_encodings(left_enc, right_enc)
  }

  fn encoding_of_call(&mut self, expr_id: ExprId, call: &hir_js::CallExpr) -> StringEncoding {
    // We only model a subset of string-returning APIs. Everything else defaults
    // to Unknown.

    if let Some(resolved) =
      resolve_api_use(&self.lowered.hir, self.body, expr_id, &self.lowered.names, self.kb)
    {
      if matches!(resolved.kind, ApiUseKind::Call | ApiUseKind::Construct) {
        let input = call
          .args
          .first()
          .map(|arg| self.encoding_of(arg.expr))
          .unwrap_or(StringEncoding::Unknown);
        if let Some(enc) = self.encoding_via_kb_id(resolved.api, input) {
          return enc;
        }
      }
    }

    let callee = call.callee;
    let Some(callee_expr) = self.body.exprs.get(callee.0 as usize) else {
      return StringEncoding::Unknown;
    };
    let ExprKind::Member(member) = &callee_expr.kind else {
      return self.encoding_of_kb_free_call(call);
    };

    let Some(prop_name) = object_key_to_str(self.body, &self.lowered.names, &member.property) else {
      return StringEncoding::Unknown;
    };

    if self.oracle.receiver_is_string(self.body_id, member.object) {
      let recv_enc = self.encoding_of(member.object);

      let api_key = format!("String.prototype.{prop_name}");
      if let Some(enc) = self.encoding_via_kb(&api_key, recv_enc) {
        return enc;
      }

      match prop_name {
        "concat" => {
          let mut encoding = recv_enc;
          for arg in &call.args {
            if arg.spread {
              return StringEncoding::Unknown;
            }
            encoding = join_encodings(encoding, self.encoding_of(arg.expr));
          }
          encoding
        }
        _ => StringEncoding::Unknown,
      }
    } else if self.oracle.receiver_is_number(self.body_id, member.object) {
      let api_key = format!("Number.prototype.{prop_name}");
      self
        .encoding_via_kb(&api_key, StringEncoding::Unknown)
        .unwrap_or(StringEncoding::Unknown)
    } else {
      StringEncoding::Unknown
    }
  }

  fn encoding_of_kb_free_call(&mut self, call: &hir_js::CallExpr) -> StringEncoding {
    let Some(callee_expr) = self.body.exprs.get(call.callee.0 as usize) else {
      return StringEncoding::Unknown;
    };

    let ExprKind::Ident(name) = &callee_expr.kind else {
      return StringEncoding::Unknown;
    };
    let Some(name_str) = self.lowered.names.resolve(*name) else {
      return StringEncoding::Unknown;
    };

    let input = call
      .args
      .first()
      .map(|arg| self.encoding_of(arg.expr))
      .unwrap_or(StringEncoding::Unknown);

    self
      .encoding_via_kb(name_str, input)
      .unwrap_or(StringEncoding::Unknown)
  }

  fn encoding_of_member(&mut self, expr_id: ExprId) -> StringEncoding {
    if let Some(resolved) =
      resolve_api_use(&self.lowered.hir, self.body, expr_id, &self.lowered.names, self.kb)
    {
      if resolved.kind != ApiUseKind::Get {
        return StringEncoding::Unknown;
      }

      return self
        .encoding_via_kb_id(resolved.api, StringEncoding::Unknown)
        .unwrap_or(StringEncoding::Unknown);
    }

    // Typed-only fallback: resolve well-known member reads like `url.pathname`
    // using type information (e.g. proving the receiver is a `URL`).
    #[cfg(feature = "typed")]
    {
      if let Some(types) = self.oracle.type_provider() {
        if let Some(resolved) =
          crate::resolve::resolve_member(self.kb, self.lowered, self.body_id, expr_id, types)
        {
          return self
            .encoding_via_kb_id(resolved.api_id, StringEncoding::Unknown)
            .unwrap_or(StringEncoding::Unknown);
        }
      }
    }

    StringEncoding::Unknown
  }

  fn encoding_via_entry(&self, entry: &Api, input: StringEncoding) -> Option<StringEncoding> {
    let output = entry.properties.get("encoding.output")?.as_str()?;
    if let Some(preserves) = entry
      .properties
      .get("encoding.preserves_input_if")
      .and_then(|value| value.as_str())
    {
      let required = parse_encoding(preserves)?;
      return (input == required).then_some(input);
    }

    match output {
      "ascii" => Some(StringEncoding::Ascii),
      "latin1" => Some(StringEncoding::Latin1),
      "utf8" => Some(StringEncoding::Utf8),
      // `"unknown"` means the knowledge base has no useful encoding information.
      // Returning `None` allows caller-specific fallbacks (e.g. refining
      // `String.prototype.concat` when all operands are ASCII).
      "unknown" => None,
      "same_as_input" => Some(input),
      _ => None,
    }
  }

  fn encoding_via_kb_id(&self, api: ApiId, input: StringEncoding) -> Option<StringEncoding> {
    let entry = self.kb.get_by_id(api)?;
    self.encoding_via_entry(entry, input)
  }

  fn encoding_via_kb(&self, api: &str, input: StringEncoding) -> Option<StringEncoding> {
    let entry = self.kb.get(api)?;
    self.encoding_via_entry(entry, input)
  }
}

fn strip_transparent_wrappers(body: &hir_js::Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

fn object_key_to_str<'a>(
  body: &'a hir_js::Body,
  names: &'a hir_js::NameInterner,
  key: &'a ObjectKey,
) -> Option<&'a str> {
  match key {
    ObjectKey::Ident(name) => names.resolve(*name),
    ObjectKey::String(s) => Some(s.as_str()),
    ObjectKey::Number(n) => Some(n.as_str()),
    ObjectKey::Computed(expr) => {
      let expr = strip_transparent_wrappers(body, *expr);
      let expr = body.exprs.get(expr.0 as usize)?;
      match &expr.kind {
        ExprKind::Literal(hir_js::Literal::String(s)) => Some(s.lossy.as_str()),
        ExprKind::Literal(hir_js::Literal::Number(n)) => Some(n.as_str()),
        ExprKind::Literal(hir_js::Literal::BigInt(n)) => Some(n.as_str()),
        _ => None,
      }
    }
  }
}

fn encoding_of_string_literal(lit: &hir_js::StringLiteral) -> StringEncoding {
  if let Some(code_units) = &lit.code_units {
    return encoding_of_code_units(code_units);
  }
  encoding_of_text(&lit.lossy)
}

fn encoding_of_code_units(code_units: &[u16]) -> StringEncoding {
  if code_units.iter().all(|&unit| unit < 0x80) {
    return StringEncoding::Ascii;
  }
  if code_units.iter().all(|&unit| unit < 0x100) {
    return StringEncoding::Latin1;
  }
  StringEncoding::Utf8
}

fn encoding_of_text(text: &str) -> StringEncoding {
  if text.chars().all(|ch| (ch as u32) < 0x80) {
    return StringEncoding::Ascii;
  }
  if text.chars().all(|ch| (ch as u32) < 0x100) {
    return StringEncoding::Latin1;
  }
  StringEncoding::Utf8
}

fn encoding_of_literal_segment(text: &str) -> StringEncoding {
  encoding_of_text(text)
}

fn join_encodings(a: StringEncoding, b: StringEncoding) -> StringEncoding {
  use StringEncoding::*;
  match (a, b) {
    (Unknown, _) | (_, Unknown) => Unknown,
    (Utf8, _) | (_, Utf8) => Utf8,
    (Latin1, _) | (_, Latin1) => Latin1,
    (Ascii, Ascii) => Ascii,
  }
}

fn parse_encoding(value: &str) -> Option<StringEncoding> {
  match value {
    "ascii" => Some(StringEncoding::Ascii),
    "latin1" => Some(StringEncoding::Latin1),
    "utf8" => Some(StringEncoding::Utf8),
    "unknown" => Some(StringEncoding::Unknown),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::{analyze_string_encodings, StringEncoding};
  use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase, KnowledgeBase};

  #[cfg(feature = "typed")]
  use super::analyze_string_encodings_typed;

  fn find_first_expr(
    body: &hir_js::Body,
    pred: impl Fn(&hir_js::ExprKind) -> bool,
  ) -> hir_js::ExprId {
    body
      .exprs
      .iter()
      .enumerate()
      .find_map(|(idx, expr)| pred(&expr.kind).then_some(hir_js::ExprId(idx as u32)))
      .expect("expected to find matching expression in test body")
  }

  #[test]
  fn ascii_string_literal_is_ascii() {
    let lower = hir_js::lower_from_source("\"hello\";").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| {
      matches!(kind, hir_js::ExprKind::Literal(hir_js::Literal::String(_)))
    });

    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn latin1_string_literal_is_latin1() {
    let lower = hir_js::lower_from_source("\"hé\";").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| {
      matches!(kind, hir_js::ExprKind::Literal(hir_js::Literal::String(_)))
    });

    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Latin1);
  }

  #[test]
  fn utf8_string_literal_is_utf8() {
    let lower = hir_js::lower_from_source("\"💩\";").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| {
      matches!(kind, hir_js::ExprKind::Literal(hir_js::Literal::String(_)))
    });

    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Utf8);
  }

  #[test]
  fn template_literal_ascii_segments_and_ascii_expr_is_ascii() {
    let lower = hir_js::lower_from_source("`x${\"a\"}y`;").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Template(_)));

    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn date_to_iso_string_is_ascii_via_kb() {
    let lower = hir_js::lower_from_source("new Date().toISOString();").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| {
      matches!(kind, hir_js::ExprKind::Call(call) if !call.is_new)
    });

    let entries = parse_api_semantics_yaml_str(
      r#"
- name: Date.prototype.toISOString
  properties:
    encoding.output: ascii
"#,
    )
    .unwrap();
    let kb = ApiDatabase::from_entries(entries);
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[test]
  fn url_pathname_is_ascii_via_kb_getter() {
    let lower = hir_js::lower_from_source("new URL(\"https://example.com\").pathname;").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Member(_)));

    let entries = parse_api_semantics_yaml_str(
      r#"
- name: URL.prototype.pathname
  kind: getter
  properties:
    encoding.output: ascii
"#,
    )
    .unwrap();
    let kb = ApiDatabase::from_entries(entries);
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[cfg(feature = "typed")]
  #[test]
  fn to_lowercase_preserves_ascii() {
    use crate::typed::TypedProgram;
    use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
    use typecheck_ts::{FileKey, MemoryHost, Program};
    use std::sync::Arc;

    let key = FileKey::new("index.ts");
    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    });
    host.insert(key.clone(), "\"ABC\".toLowerCase();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));

    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = crate::load_default_api_database();
    let results = analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }

  #[cfg(feature = "typed")]
  #[test]
  fn to_lowercase_on_any_is_unknown() {
    use crate::typed::TypedProgram;
    use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
    use typecheck_ts::{FileKey, MemoryHost, Program};
    use std::sync::Arc;

    let key = FileKey::new("index.ts");
    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    });
    host.insert(key.clone(), "(\"ABC\" as any).toLowerCase();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));

    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Unknown);
  }

  #[cfg(feature = "typed")]
  #[test]
  fn trim_preserves_ascii_via_kb() {
    use crate::typed::TypedProgram;
    use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase};
    use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
    use std::sync::Arc;
    use typecheck_ts::{FileKey, MemoryHost, Program};

    let key = FileKey::new("index.ts");
    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    });
    host.insert(key.clone(), "\"ABC\".trim();");

    let program = Arc::new(Program::new(host, vec![key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&key).expect("index.ts loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body_id = lowered.root_body();
    let root_body = lowered.body(root_body_id).expect("root body exists");

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));

    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let entries = parse_api_semantics_yaml_str(
      r#"
- name: String.prototype.trim
  properties:
    encoding.output: same_as_input
"#,
    )
    .unwrap();
    let kb = ApiDatabase::from_entries(entries);
    let results = analyze_string_encodings_typed(lowered.as_ref(), &kb, &types);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }
}
