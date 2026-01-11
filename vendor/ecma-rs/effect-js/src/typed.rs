use crate::types::{TypeId, TypeKindSummary, TypeProvider};
use hir_js::{BodyId, ExprId, PatId};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Cached `TypeProvider` backed by a `typecheck-ts` [`Program`].
///
/// `typecheck-ts` stores expression/pattern types in per-body side tables.
/// `TypedProgram` snapshots those tables into per-body vectors aligned to HIR
/// `ExprId`/`PatId` indices so downstream passes can query types cheaply without
/// repeatedly calling into the checker.
///
/// In addition to type queries, this wrapper exposes a small set of semantic
/// helpers (`symbol_at_expr`, `def_kind`, etc.) used by typed-only API
/// resolution.
pub struct TypedProgram {
  program: Arc<typecheck_ts::Program>,
  expr_types: HashMap<BodyId, Vec<Option<TypeId>>>,
  pat_types: HashMap<BodyId, Vec<Option<TypeId>>>,
  body_files: HashMap<BodyId, typecheck_ts::FileId>,
}

impl fmt::Debug for TypedProgram {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Avoid requiring `typecheck_ts::Program: Debug`.
    f.debug_struct("TypedProgram")
      .field("expr_types", &self.expr_types)
      .field("pat_types", &self.pat_types)
      .finish_non_exhaustive()
  }
}

impl TypedProgram {
  pub fn from_program(program: Arc<typecheck_ts::Program>, file: typecheck_ts::FileId) -> Self {
    let mut expr_types: HashMap<BodyId, Vec<Option<TypeId>>> = HashMap::new();
    let mut pat_types: HashMap<BodyId, Vec<Option<TypeId>>> = HashMap::new();
    let mut body_files: HashMap<BodyId, typecheck_ts::FileId> = HashMap::new();

    if let Some(lowered) = program.hir_lowered(file) {
      for (body_id, idx) in lowered.body_index.iter() {
        let body = &lowered.bodies[*idx];
        let res = program.check_body(*body_id);

        let mut expr_vec = Vec::with_capacity(body.exprs.len());
        for expr_idx in 0..body.exprs.len() {
          expr_vec.push(res.expr_type(ExprId(expr_idx as u32)));
        }
        expr_types.insert(*body_id, expr_vec);

        let mut pat_vec = Vec::with_capacity(body.pats.len());
        for pat_idx in 0..body.pats.len() {
          pat_vec.push(res.pat_type(PatId(pat_idx as u32)));
        }
        pat_types.insert(*body_id, pat_vec);

        body_files.insert(*body_id, file);
      }
    }

    Self {
      program,
      expr_types,
      pat_types,
      body_files,
    }
  }

  pub fn program(&self) -> &Arc<typecheck_ts::Program> {
    &self.program
  }

  pub fn file_for_body(&self, body: BodyId) -> Option<typecheck_ts::FileId> {
    self.body_files.get(&body).copied()
  }

  pub fn symbol_at_expr(
    &self,
    body: BodyId,
    expr: ExprId,
  ) -> Option<typecheck_ts::semantic_js::SymbolId> {
    let span = self.program.expr_span(body, expr)?;
    self.program.symbol_at(span.file, span.range.start)
  }

  pub fn def_kind(&self, def: typecheck_ts::DefId) -> Option<typecheck_ts::DefKind> {
    self.program.def_kind(def)
  }

  pub fn file_key(&self, file: typecheck_ts::FileId) -> Option<typecheck_ts::FileKey> {
    self.program.file_key(file)
  }
}

impl TypeProvider for TypedProgram {
  fn as_typed_program(&self) -> Option<&crate::typed::TypedProgram> {
    Some(self)
  }

  fn expr_type(&self, body: BodyId, expr: ExprId) -> Option<TypeId> {
    self
      .expr_types
      .get(&body)
      .and_then(|types| types.get(expr.0 as usize).copied())
      .flatten()
  }

  fn pat_type(&self, body: BodyId, pat: PatId) -> Option<TypeId> {
    self
      .pat_types
      .get(&body)
      .and_then(|types| types.get(pat.0 as usize).copied())
      .flatten()
  }

  fn type_kind(&self, ty: TypeId) -> Option<TypeKindSummary> {
    Some(self.program.type_kind(ty))
  }
}
