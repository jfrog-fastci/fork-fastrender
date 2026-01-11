use super::*;

impl ProgramState {
  pub(super) fn strict_native_dynamic_diagnostics(&self) -> Vec<Diagnostic> {
    if !self.compiler_options.strict_native {
      return Vec::new();
    }

    fn body_in_non_arrow_function_scope(
      state: &ProgramState,
      body: BodyId,
      cache: &mut HashMap<BodyId, bool>,
    ) -> bool {
      if let Some(cached) = cache.get(&body) {
        return *cached;
      }

      let mut in_scope = false;
      if let Some(lowered) = state.hir_lowered.get(&body.file()).map(Arc::as_ref) {
        if let Some(hir_body) = lowered.body(body) {
          if matches!(hir_body.kind, HirBodyKind::Function) {
            if hir_body.function.as_ref().is_some_and(|func| !func.is_arrow) {
              in_scope = true;
            }
          }
        }
      }

      if !in_scope {
        if let Some(parent) = state.body_parents.get(&body).copied() {
          in_scope = body_in_non_arrow_function_scope(state, parent, cache);
        }
      }

      cache.insert(body, in_scope);
      in_scope
    }

    fn expr_by_id<'a>(body: &'a hir_js::Body, id: ExprId) -> Option<&'a hir_js::Expr> {
      body.exprs.get(id.0 as usize)
    }

    fn pat_by_id<'a>(body: &'a hir_js::Body, id: PatId) -> Option<&'a hir_js::Pat> {
      body.pats.get(id.0 as usize)
    }

    fn ident_name<'a>(
      lowered: &'a LowerResult,
      body: &hir_js::Body,
      id: ExprId,
    ) -> Option<&'a str> {
      let expr = expr_by_id(body, id)?;
      match &expr.kind {
        HirExprKind::Ident(name_id) => lowered.names.resolve(*name_id),
        _ => None,
      }
    }

    fn member_property_name<'a>(
      lowered: &'a LowerResult,
      body: &'a hir_js::Body,
      key: &hir_js::ObjectKey,
    ) -> Option<&'a str> {
      match key {
        hir_js::ObjectKey::Ident(name_id) => lowered.names.resolve(*name_id),
        hir_js::ObjectKey::Computed(expr_id) => {
          let expr = expr_by_id(body, *expr_id)?;
          match &expr.kind {
            HirExprKind::Literal(hir_js::Literal::String(str)) => Some(str.lossy.as_str()),
            _ => None,
          }
        }
        _ => None,
      }
    }

    fn expr_is_literal_key(body: &hir_js::Body, id: ExprId) -> bool {
      let Some(expr) = expr_by_id(body, id) else {
        return false;
      };
      matches!(
        &expr.kind,
        HirExprKind::Literal(hir_js::Literal::String(_))
          | HirExprKind::Literal(hir_js::Literal::Number(_))
          | HirExprKind::Literal(hir_js::Literal::BigInt(_))
      )
    }

    fn type_is_literal_key(store: &tti::TypeStore, ty: TypeId) -> bool {
      match store.type_kind(store.canon(ty)) {
        tti::TypeKind::StringLiteral(_)
        | tti::TypeKind::NumberLiteral(_)
        | tti::TypeKind::BigIntLiteral(_) => true,
        tti::TypeKind::Union(items) => items.iter().copied().all(|ty| type_is_literal_key(store, ty)),
        tti::TypeKind::Never => true,
        _ => false,
      }
    }

    fn expr_chain_contains_proto_mutation(
      lowered: &LowerResult,
      body: &hir_js::Body,
      mut id: ExprId,
    ) -> bool {
      loop {
        let Some(expr) = expr_by_id(body, id) else {
          return false;
        };
        match &expr.kind {
          HirExprKind::Member(member) => {
            if member_property_name(lowered, body, &member.property)
              .is_some_and(|name| name == "prototype" || name == "__proto__")
            {
              return true;
            }
            id = member.object;
          }
          _ => return false,
        }
      }
    }

    let store = self.store.as_ref();
    let mut diagnostics = Vec::new();
    let mut files: Vec<FileId> = self.hir_lowered.keys().copied().collect();
    files.sort_by_key(|id| id.0);

    let mut in_non_arrow_cache: HashMap<BodyId, bool> = HashMap::new();

    for file in files {
      if self.lib_file_ids.contains(&file) {
        continue;
      }
      if self.file_kinds.get(&file) == Some(&FileKind::Dts) {
        continue;
      }
      let Some(lowered) = self.hir_lowered.get(&file).map(Arc::as_ref) else {
        continue;
      };

      let mut bodies: Vec<BodyId> = lowered.hir.bodies.iter().copied().collect();
      bodies.sort_by_key(|id| id.0);

      for body_id in bodies {
        let Some(body) = lowered.body(body_id) else {
          continue;
        };
        let body_result = self.body_results.get(&body_id);
        let in_non_arrow_scope = body_in_non_arrow_function_scope(self, body_id, &mut in_non_arrow_cache);

        for stmt in body.stmts.iter() {
          if matches!(&stmt.kind, hir_js::StmtKind::With { .. }) {
            diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_WITH.error(
              "`with` statements are forbidden in `strict_native` mode",
              Span::new(file, stmt.span),
            ));
          }
        }

        for expr in body.exprs.iter() {
          match &expr.kind {
            HirExprKind::Ident(name_id) => {
              if in_non_arrow_scope
                && lowered
                  .names
                  .resolve(*name_id)
                  .is_some_and(|name| name == "arguments")
              {
                diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_ARGUMENTS.error(
                  "`arguments` object is forbidden in `strict_native` mode",
                  Span::new(file, expr.span),
                ));
              }
            }
            HirExprKind::Call(call) => {
              if let Some(name) = ident_name(lowered, body, call.callee) {
                if name == "eval" {
                  diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_EVAL.error(
                    "`eval` is forbidden in `strict_native` mode",
                    Span::new(file, expr.span),
                  ));
                }
                if name == "Function" {
                  diagnostics.push(
                    codes::STRICT_NATIVE_FORBIDDEN_FUNCTION_CONSTRUCTOR.error(
                      "`Function` constructor is forbidden in `strict_native` mode",
                      Span::new(file, expr.span),
                    ),
                  );
                }
                if name == "Proxy" && call.is_new {
                  diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_PROXY.error(
                    "`Proxy` is forbidden in `strict_native` mode",
                    Span::new(file, expr.span),
                  ));
                }
              }

              let Some(callee_expr) = expr_by_id(body, call.callee) else {
                continue;
              };
              if let HirExprKind::Member(member) = &callee_expr.kind {
                let obj = ident_name(lowered, body, member.object);
                let prop = member_property_name(lowered, body, &member.property);

                if matches!((obj, prop), (Some("globalThis"), Some("eval"))) {
                  diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_EVAL.error(
                    "`eval` is forbidden in `strict_native` mode",
                    Span::new(file, expr.span),
                  ));
                }

                if matches!((obj, prop), (Some("Proxy"), Some("revocable"))) {
                  diagnostics.push(codes::STRICT_NATIVE_FORBIDDEN_PROXY.error(
                    "`Proxy` is forbidden in `strict_native` mode",
                    Span::new(file, expr.span),
                  ));
                }

                if matches!(
                  (obj, prop),
                  (Some("Object"), Some("setPrototypeOf")) | (Some("Reflect"), Some("setPrototypeOf"))
                ) {
                  diagnostics.push(
                    codes::STRICT_NATIVE_FORBIDDEN_PROTOTYPE_MUTATION.error(
                      "prototype mutation is forbidden in `strict_native` mode",
                      Span::new(file, expr.span),
                    ),
                  );
                }
              }
            }
            HirExprKind::Assignment { target, .. } => {
              let Some(pat) = pat_by_id(body, *target) else {
                continue;
              };
              let HirPatKind::AssignTarget(target_expr) = &pat.kind else {
                continue;
              };
              if expr_chain_contains_proto_mutation(lowered, body, *target_expr) {
                diagnostics.push(
                  codes::STRICT_NATIVE_FORBIDDEN_PROTOTYPE_MUTATION.error(
                    "prototype mutation is forbidden in `strict_native` mode",
                    Span::new(file, expr.span),
                  ),
                );
              }
            }
            HirExprKind::Member(member) => {
              let hir_js::ObjectKey::Computed(key_expr) = &member.property else {
                continue;
              };
              let key_expr = *key_expr;

              if expr_is_literal_key(body, key_expr) {
                continue;
              }

              let key_span = expr_by_id(body, key_expr)
                .map(|expr| expr.span)
                .unwrap_or(expr.span);

              let key_type = body_result.and_then(|res| res.expr_type(key_expr));
              if let Some(key_type) = key_type {
                if type_is_literal_key(store, key_type) {
                  continue;
                }
              }

              diagnostics.push(
                codes::STRICT_NATIVE_COMPUTED_KEY_NOT_CONSTANT.error(
                  "computed property access requires a constant key in `strict_native` mode",
                  Span::new(file, key_span),
                ),
              );
            }
            _ => {}
          }
        }
      }
    }

    diagnostics
  }
}
