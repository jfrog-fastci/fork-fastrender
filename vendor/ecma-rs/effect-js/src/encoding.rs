use hir_js::{BinaryOp, ExprId, ExprKind, Literal, ObjectKey};
use knowledge_base::KnowledgeBase;
use std::collections::HashMap;

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
  let mut out = HashMap::new();
  for (body_id, idx) in result.body_index.iter() {
    let body = &result.bodies[*idx];
    let mut analyzer = BodyAnalyzer {
      body,
      names: &result.names,
      kb,
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

struct BodyAnalyzer<'a> {
  body: &'a hir_js::Body,
  names: &'a hir_js::NameInterner,
  kb: &'a KnowledgeBase,
  cache: Vec<Option<StringEncoding>>,
}

impl BodyAnalyzer<'_> {
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
      ExprKind::Literal(Literal::String(string_lit)) => {
        if is_ascii_str(&string_lit.lossy) {
          StringEncoding::Ascii
        } else {
          StringEncoding::Utf8
        }
      }
      ExprKind::Template(template) => self.encoding_of_template(template),

      // Pure wrappers around the underlying expression value.
      ExprKind::TypeAssertion { expr, .. } => self.encoding_of(*expr),
      ExprKind::NonNull { expr } => self.encoding_of(*expr),
      ExprKind::Satisfies { expr, .. } => self.encoding_of(*expr),

      ExprKind::Binary { op, left, right } => match op {
        BinaryOp::Add => self.encoding_of_add(*left, *right),
        _ => StringEncoding::Unknown,
      },

      ExprKind::Call(call) => self.encoding_of_call(call),

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

  fn encoding_of_add(&mut self, left: ExprId, right: ExprId) -> StringEncoding {
    #[cfg(not(feature = "typed"))]
    {
      let _ = (left, right);
      return StringEncoding::Unknown;
    }

    #[cfg(feature = "typed")]
    {
      let left_enc = self.encoding_of(left);
      let right_enc = self.encoding_of(right);

      // Without a full type system we treat "proven string" as "we inferred an
      // encoding for the operand". This is intentionally conservative.
      let is_string = left_enc != StringEncoding::Unknown || right_enc != StringEncoding::Unknown;
      if !is_string {
        return StringEncoding::Unknown;
      }

      if left_enc == StringEncoding::Ascii && right_enc == StringEncoding::Ascii {
        StringEncoding::Ascii
      } else {
        StringEncoding::Unknown
      }
    }
  }

  fn encoding_of_call(&mut self, call: &hir_js::CallExpr) -> StringEncoding {
    // We only model a subset of string-returning APIs. Everything else defaults
    // to Unknown.

    let callee = call.callee;
    let Some(callee_expr) = self.body.exprs.get(callee.0 as usize) else {
      return StringEncoding::Unknown;
    };
    let ExprKind::Member(member) = &callee_expr.kind else {
      return self.encoding_of_kb_free_call(call);
    };

    let Some(prop_name) = object_key_to_str(self.names, &member.property) else {
      return StringEncoding::Unknown;
    };

    #[cfg(not(feature = "typed"))]
    {
      let _ = prop_name;
      return StringEncoding::Unknown;
    }

    #[cfg(feature = "typed")]
    {
      let recv_enc = self.encoding_of(member.object);
      if recv_enc == StringEncoding::Unknown {
        // Conservatively refuse to treat this as a string method call unless we
        // can prove the receiver is a string.
        return StringEncoding::Unknown;
      }

      let api_key = format!("String.prototype.{prop_name}");
      if let Some(enc) = self.encoding_via_kb(&api_key, recv_enc) {
        return enc;
      }

      match prop_name {
        "slice" => recv_enc,
        "toLowerCase" | "toUpperCase" => {
          if recv_enc == StringEncoding::Ascii {
            StringEncoding::Ascii
          } else {
            StringEncoding::Unknown
          }
        }
        _ => StringEncoding::Unknown,
      }
    }
  }

  fn encoding_of_kb_free_call(&mut self, call: &hir_js::CallExpr) -> StringEncoding {
    let Some(callee_expr) = self.body.exprs.get(call.callee.0 as usize) else {
      return StringEncoding::Unknown;
    };

    let ExprKind::Ident(name) = &callee_expr.kind else {
      return StringEncoding::Unknown;
    };
    let Some(name_str) = self.names.resolve(*name) else {
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

  fn encoding_via_kb(&self, api: &str, input: StringEncoding) -> Option<StringEncoding> {
    let entry = self.kb.get(api)?;
    let output = entry.properties.get("encoding.output")?;

    if let Some(preserves) = entry.properties.get("encoding.preserves_input_if") {
      let required = parse_encoding(preserves)?;
      return (input == required).then_some(input);
    }

    match output.as_str() {
      "ascii" => Some(StringEncoding::Ascii),
      "latin1" => Some(StringEncoding::Latin1),
      "utf8" => Some(StringEncoding::Utf8),
      "unknown" => Some(StringEncoding::Unknown),
      "same_as_input" => Some(input),
      _ => None,
    }
  }
}

fn object_key_to_str<'a>(
  names: &'a hir_js::NameInterner,
  key: &'a ObjectKey,
) -> Option<&'a str> {
  match key {
    ObjectKey::Ident(name) => names.resolve(*name),
    ObjectKey::String(s) => Some(s.as_str()),
    _ => None,
  }
}

fn is_ascii_str(text: &str) -> bool {
  text.chars().all(|ch| (ch as u32) < 0x80)
}

fn encoding_of_literal_segment(text: &str) -> StringEncoding {
  if is_ascii_str(text) {
    StringEncoding::Ascii
  } else {
    StringEncoding::Utf8
  }
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
  use knowledge_base::KnowledgeBase;

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
  fn utf8_string_literal_is_utf8() {
    let lower = hir_js::lower_from_source("\"hé\";").unwrap();
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

  #[cfg(feature = "typed")]
  #[test]
  fn to_lowercase_preserves_ascii() {
    let lower = hir_js::lower_from_source("\"ABC\".toLowerCase();").unwrap();
    let root_body_id = lower.hir.root_body;
    let root_body = &lower.bodies[*lower.body_index.get(&root_body_id).unwrap()];

    let expr_id = find_first_expr(root_body, |kind| matches!(kind, hir_js::ExprKind::Call(_)));

    let kb = KnowledgeBase::default();
    let results = analyze_string_encodings(&lower, &kb);
    let root = results.get(&root_body_id).unwrap();

    assert_eq!(root.encodings[expr_id.0 as usize], StringEncoding::Ascii);
  }
}
