use effect_js::{recognize_patterns, RecognizedPattern, RecognizedPatternId};
use hir_js::{lower_from_source_with_kind, DefKind, FileKind, PatId, StmtId, StmtKind, VarDeclKind};

#[test]
fn recognizes_for_await_of_loops() {
  let source = r#"
    // Minimal stubs for the test harness (no lib.d.ts).
    interface AsyncIterable<T> {}
    declare const console: { log(x: unknown): void };

    async function f(it: AsyncIterable<number>) {
      for await (const x of it) {
        console.log(x);
      }
    }
  "#;

  let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower");
  let func = lowered
    .defs
    .iter()
    .find(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("f"))
    .expect("function f");
  let body_id = func.body.expect("function body id");
  let body = lowered.body(body_id).expect("function body");

  let result = recognize_patterns(body);
  assert_eq!(result.patterns.len(), 1, "expected exactly one recognized pattern");

  let async_for = body
    .stmts
    .iter()
    .enumerate()
    .find_map(|(idx, stmt)| match stmt.kind {
      StmtKind::ForIn {
        is_for_of: true,
        await_: true,
        ..
      } => Some(StmtId(idx as u32)),
      _ => None,
    })
    .expect("for-await-of stmt");

  let (expected_iterable, expected_binding_pat, expected_binding_kind, expected_body) =
    match &body.stmts[async_for.0 as usize].kind {
      StmtKind::ForIn {
        left,
        right,
        body,
        is_for_of: true,
        await_: true,
      } => {
        let (binding_pat, binding_kind): (PatId, Option<VarDeclKind>) = match left {
          hir_js::ForHead::Pat(pat) => (*pat, None),
          hir_js::ForHead::Var(var) => {
            let decl = var.declarators.first().expect("for-await-of var decl");
            (decl.pat, Some(var.kind))
          }
        };
        (*right, binding_pat, binding_kind, *body)
      }
      other => panic!("expected for-await-of stmt, got {other:?}"),
    };

  match &result.patterns[0] {
    RecognizedPattern::AsyncIterator {
      stmt,
      iterable,
      binding_pat,
      binding_kind,
      body,
    } => {
      assert_eq!(*stmt, async_for);
      assert_eq!(*iterable, expected_iterable);
      assert_eq!(*binding_pat, expected_binding_pat);
      assert_eq!(*binding_kind, expected_binding_kind);
      assert_eq!(*body, expected_body);
    }
    other => panic!("expected AsyncIterator pattern, got {other:?}"),
  }

  assert_eq!(
    result.stmt_patterns.patterns_by_stmt[async_for.0 as usize],
    vec![RecognizedPatternId(0)]
  );
}

