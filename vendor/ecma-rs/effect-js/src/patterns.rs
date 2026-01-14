use crate::{properties, Api, CallSiteInfo};
use hir_js::{Body, PatId, StmtId, StmtKind, VarDeclKind};

use crate::RecognizedPattern;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayOpKind {
  Map,
  Filter,
  Reduce,
  ForEach,
}

impl ArrayOpKind {
  pub fn from_api_name(name: &str) -> Option<Self> {
    match name {
      "Array.prototype.map" => Some(Self::Map),
      "Array.prototype.filter" => Some(Self::Filter),
      "Array.prototype.reduce" => Some(Self::Reduce),
      "Array.prototype.forEach" => Some(Self::ForEach),
      _ => None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{parse_api_semantics_yaml_str, ApiDatabase, CallSiteInfo, EffectSet, Purity};

  fn array_db() -> ApiDatabase {
    let yaml = include_str!("../../knowledge-base/core/array.yaml");
    ApiDatabase::from_entries(parse_api_semantics_yaml_str(yaml).unwrap())
  }

  fn pure_callsite() -> CallSiteInfo {
    CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      callback_is_associative: Some(false),
    }
  }

  #[test]
  fn recognizes_map_filter_reduce_pipeline() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap().clone();
    let filter = db.get("Array.prototype.filter").unwrap().clone();
    let reduce = db.get("Array.prototype.reduce").unwrap().clone();

    let callsite = pure_callsite();
    let pipeline = MapFilterReduce::recognize(vec![
      (map, callsite),
      (filter, callsite),
      (reduce, callsite),
    ])
    .expect("recognize pipeline");

    assert_eq!(pipeline.ops.len(), 3);

    assert_eq!(pipeline.ops[0].kind, ArrayOpKind::Map);
    assert!(pipeline.ops[0].meta.fusable);
    assert!(pipeline.ops[0].meta.parallelizable);
    assert_eq!(
      pipeline.ops[0].meta.output_length_relation,
      crate::properties::OutputLengthRelation::SameAsInput
    );

    assert_eq!(pipeline.ops[1].kind, ArrayOpKind::Filter);
    assert!(pipeline.ops[1].meta.fusable);
    assert!(pipeline.ops[1].meta.parallelizable);
    assert_eq!(
      pipeline.ops[1].meta.output_length_relation,
      crate::properties::OutputLengthRelation::LeInput
    );

    assert_eq!(pipeline.ops[2].kind, ArrayOpKind::Reduce);
    assert!(pipeline.ops[2].meta.fusable);
    assert!(!pipeline.ops[2].meta.parallelizable);
    assert_eq!(
      pipeline.ops[2].meta.output_length_relation,
      crate::properties::OutputLengthRelation::Unknown
    );
  }

  #[test]
  fn rejects_chain_without_reduce_tail() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap().clone();
    let filter = db.get("Array.prototype.filter").unwrap().clone();

    let callsite = pure_callsite();
    assert!(
      MapFilterReduce::recognize(vec![(map, callsite), (filter, callsite)]).is_none(),
      "expected non-reduce tail to be rejected"
    );
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayOpMetadata {
  pub fusable: bool,
  pub parallelizable: bool,
  pub output_length_relation: properties::OutputLengthRelation,
}

#[derive(Debug, Clone)]
pub struct ArrayOp {
  pub api: Api,
  pub callsite: CallSiteInfo,
  pub kind: ArrayOpKind,
  pub meta: ArrayOpMetadata,
}

#[derive(Debug, Clone)]
pub struct MapFilterReduce {
  pub ops: Vec<ArrayOp>,
}

impl MapFilterReduce {
  /// Recognize a (map|filter)+reduce pipeline based purely on a sequence of APIs.
  ///
  /// This is intentionally structural only; callers are expected to supply the
  /// `Api` + `CallSiteInfo` sequence extracted from AST/IR pattern matching.
  pub fn recognize(ops: Vec<(Api, CallSiteInfo)>) -> Option<Self> {
    if ops.len() < 2 {
      return None;
    }

    let apis: Vec<_> = ops.iter().map(|(api, _)| api.clone()).collect();
    let kinds: Vec<_> = ops
      .iter()
      .map(|(api, _)| ArrayOpKind::from_api_name(&api.name))
      .collect::<Option<Vec<_>>>()?;

    if !matches!(kinds.last(), Some(ArrayOpKind::Reduce)) {
      return None;
    }
    if kinds[..kinds.len() - 1]
      .iter()
      .any(|k| !matches!(k, ArrayOpKind::Map | ArrayOpKind::Filter))
    {
      return None;
    }

    let annotated_ops = ops
      .into_iter()
      .enumerate()
      .map(|(idx, (api, callsite))| {
        let kind = ArrayOpKind::from_api_name(&api.name)
          .expect("validated by initial kind extraction above");

        let fusable = match (idx.checked_sub(1).map(|p| &apis[p]), apis.get(idx + 1)) {
          (_, Some(next)) => {
            properties::fusable_with(&api, next) || properties::fusable_with(next, &api)
          }
          (Some(prev), None) => {
            properties::fusable_with(prev, &api) || properties::fusable_with(&api, prev)
          }
          (None, None) => false,
        };

        let meta = ArrayOpMetadata {
          fusable,
          parallelizable: properties::is_parallelizable(&api, &callsite),
          output_length_relation: properties::output_length_relation(&api),
        };

        ArrayOp {
          api,
          callsite,
          kind,
          meta,
        }
      })
      .collect();

    Some(Self { ops: annotated_ops })
  }
}

/// Stable identifier for a recognized pattern within a single [`RecognizePatternsResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecognizedPatternId(pub u32);

/// Side tables keyed by [`hir_js::ExprId`] (aligned to `body.exprs`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExprPatternTables {
  pub patterns_by_expr: Vec<Vec<RecognizedPatternId>>,
}

impl ExprPatternTables {
  fn new_aligned(body: &Body) -> Self {
    Self {
      patterns_by_expr: vec![Vec::new(); body.exprs.len()],
    }
  }
}

/// Side tables keyed by [`StmtId`] (aligned to `body.stmts`).
#[derive(Debug, Clone, PartialEq)]
pub struct StmtPatternTables {
  pub patterns_by_stmt: Vec<Vec<RecognizedPatternId>>,
}

impl StmtPatternTables {
  fn new_aligned(body: &Body) -> Self {
    Self {
      patterns_by_stmt: vec![Vec::new(); body.stmts.len()],
    }
  }
}

/// Output of [`recognize_patterns`] for a single [`hir_js::Body`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecognizePatternsResult {
  pub patterns: Vec<RecognizedPattern>,
  pub expr_patterns: ExprPatternTables,
  pub stmt_patterns: StmtPatternTables,
}

pub fn recognize_patterns(body: &Body) -> RecognizePatternsResult {
  let mut recognizer = PatternRecognizer {
    body,
    patterns: Vec::new(),
    expr_patterns: ExprPatternTables::new_aligned(body),
    stmt_patterns: StmtPatternTables::new_aligned(body),
  };

  for &stmt in body.root_stmts.iter() {
    recognizer.visit_stmt(stmt);
  }

  RecognizePatternsResult {
    patterns: recognizer.patterns,
    expr_patterns: recognizer.expr_patterns,
    stmt_patterns: recognizer.stmt_patterns,
  }
}

struct PatternRecognizer<'a> {
  body: &'a Body,
  patterns: Vec<RecognizedPattern>,
  expr_patterns: ExprPatternTables,
  stmt_patterns: StmtPatternTables,
}

impl PatternRecognizer<'_> {
  fn push_stmt_pattern(&mut self, stmt: StmtId, pattern: RecognizedPattern) -> RecognizedPatternId {
    let id = RecognizedPatternId(self.patterns.len() as u32);
    self.patterns.push(pattern);
    self.stmt_patterns.patterns_by_stmt[stmt.0 as usize].push(id);
    id
  }

  fn visit_stmt(&mut self, stmt_id: StmtId) {
    let stmt = &self.body.stmts[stmt_id.0 as usize];

    // Statement-level patterns.
    if let StmtKind::ForIn {
      left,
      right,
      body,
      is_for_of: true,
      await_: true,
    } = &stmt.kind
    {
      let binding_pat: Option<(PatId, Option<VarDeclKind>)> = match left {
        hir_js::ForHead::Pat(pat) => Some((*pat, None)),
        hir_js::ForHead::Var(var) => var
          .declarators
          .first()
          .map(|decl| (decl.pat, Some(var.kind))),
      };

      if let Some((binding_pat, binding_kind)) = binding_pat {
        self.push_stmt_pattern(
          stmt_id,
          RecognizedPattern::AsyncIterator {
            stmt: stmt_id,
            iterable: *right,
            binding_pat,
            binding_kind,
            body: *body,
          },
        );
      }
    }

    // Walk nested statements for additional patterns.
    match &stmt.kind {
      StmtKind::Block(stmts) => {
        for &child in stmts {
          self.visit_stmt(child);
        }
      }
      StmtKind::If {
        consequent,
        alternate,
        ..
      } => {
        self.visit_stmt(*consequent);
        if let Some(alt) = alternate {
          self.visit_stmt(*alt);
        }
      }
      StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
        self.visit_stmt(*body);
      }
      StmtKind::For { body, .. } => {
        self.visit_stmt(*body);
      }
      StmtKind::ForIn { body, .. } => {
        self.visit_stmt(*body);
      }
      StmtKind::Switch { cases, .. } => {
        for case in cases {
          for &child in case.consequent.iter() {
            self.visit_stmt(child);
          }
        }
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        self.visit_stmt(*block);
        if let Some(catch) = catch {
          self.visit_stmt(catch.body);
        }
        if let Some(finally) = finally_block {
          self.visit_stmt(*finally);
        }
      }
      StmtKind::Labeled { body, .. } | StmtKind::With { body, .. } => {
        self.visit_stmt(*body);
      }
      StmtKind::Expr(_)
      | StmtKind::ExportDefaultExpr(_)
      | StmtKind::Decl(_)
      | StmtKind::Return(_)
      | StmtKind::Throw(_)
      | StmtKind::Break(_)
      | StmtKind::Continue(_)
      | StmtKind::Var(_)
      | StmtKind::Debugger
      | StmtKind::Empty => {}
    }
  }
}
