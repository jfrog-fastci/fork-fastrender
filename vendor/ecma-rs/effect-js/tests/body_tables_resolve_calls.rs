use effect_js::{analyze_body_tables_untyped, load_default_api_database};
use hir_js::{BodyId, ExprId, FileKind, StmtKind};

fn first_root_expr(lowered: &hir_js::LowerResult) -> (BodyId, ExprId) {
  let root = lowered.root_body();
  let root_body = lowered.body(root).expect("root body");
  let first_stmt = *root_body.root_stmts.first().expect("root stmt");
  let stmt = &root_body.stmts[first_stmt.0 as usize];
  match stmt.kind {
    StmtKind::Expr(expr) => (root, expr),
    _ => panic!("expected expression statement"),
  }
}

#[test]
fn resolves_json_parse_in_body_tables_untyped() {
  let kb = load_default_api_database();
  let json_parse_id = kb.id_of("JSON.parse").expect("JSON.parse in KB");

  let lowered =
    hir_js::lower_from_source_with_kind(FileKind::Js, r#"JSON.parse("x");"#).unwrap();
  let (body_id, call_expr) = first_root_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = analyze_body_tables_untyped(&kb, &lowered);
  let root_tables = tables.get(&body_id).expect("root body tables");

  assert_eq!(root_tables.resolved_call[call_expr.0 as usize], Some(json_parse_id));

  let hir_js::ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
    panic!("expected Call expr");
  };
  let hir_js::ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
    panic!("expected Member callee for JSON.parse");
  };
  assert_eq!(
    root_tables.resolved_call_receiver[call_expr.0 as usize],
    Some(member.object)
  );
  assert_eq!(
    root_tables.resolved_call_args[call_expr.0 as usize].as_slice(),
    [call.args[0].expr]
  );
}

#[test]
fn resolves_promise_all_in_body_tables_untyped() {
  let kb = load_default_api_database();
  let promise_all_id = kb.id_of("Promise.all").expect("Promise.all in KB");

  let lowered =
    hir_js::lower_from_source_with_kind(FileKind::Js, r#"Promise.all([x]);"#).unwrap();
  let (body_id, call_expr) = first_root_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = analyze_body_tables_untyped(&kb, &lowered);
  let root_tables = tables.get(&body_id).expect("root body tables");

  assert_eq!(root_tables.resolved_call[call_expr.0 as usize], Some(promise_all_id));

  let args = &root_tables.resolved_call_args[call_expr.0 as usize];
  assert_eq!(args.len(), 1, "Promise.all should normalize to one arg");
  let arg0 = args[0];
  let hir_js::ExprKind::Array(arr) = &body.exprs[arg0.0 as usize].kind else {
    panic!("expected Promise.all arg0 to be an array literal expr");
  };
  assert_eq!(arr.elements.len(), 1);
}

#[test]
fn resolves_global_this_fetch_in_body_tables_untyped() {
  let kb = load_default_api_database();
  let fetch_id = kb.id_of("fetch").expect("fetch in KB");

  let lowered =
    hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.fetch("x");"#).unwrap();
  let (body_id, call_expr) = first_root_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = analyze_body_tables_untyped(&kb, &lowered);
  let root_tables = tables.get(&body_id).expect("root body tables");

  assert_eq!(root_tables.resolved_call[call_expr.0 as usize], Some(fetch_id));

  let hir_js::ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
    panic!("expected Call expr");
  };
  let hir_js::ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
    panic!("expected Member callee for globalThis.fetch");
  };
  assert_eq!(
    root_tables.resolved_call_receiver[call_expr.0 as usize],
    Some(member.object)
  );
  assert_eq!(
    root_tables.resolved_call_args[call_expr.0 as usize].as_slice(),
    [call.args[0].expr]
  );
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn resolves_array_semantic_ops_in_body_tables_untyped() {
  let kb = load_default_api_database();
  let array_map_id = kb
    .id_of("Array.prototype.map")
    .expect("Array.prototype.map in KB");

  let lowered =
    hir_js::lower_from_source_with_kind(FileKind::Js, r#"[1, 2, 3].map(x => x);"#).unwrap();
  let (body_id, call_expr) = first_root_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = analyze_body_tables_untyped(&kb, &lowered);
  let root_tables = tables.get(&body_id).expect("root body tables");

  let hir_js::ExprKind::ArrayMap { array, callback } = &body.exprs[call_expr.0 as usize].kind else {
    panic!("expected ArrayMap semantic-op node");
  };
  assert_eq!(root_tables.resolved_call[call_expr.0 as usize], Some(array_map_id));
  assert_eq!(
    root_tables.resolved_call_receiver[call_expr.0 as usize],
    Some(*array)
  );
  assert_eq!(
    root_tables.resolved_call_args[call_expr.0 as usize].as_slice(),
    [*callback]
  );
}

#[cfg(feature = "hir-semantic-ops")]
#[test]
fn resolves_array_chain_semantic_ops_in_body_tables_untyped() {
  let kb = load_default_api_database();
  let array_filter_id = kb
    .id_of("Array.prototype.filter")
    .expect("Array.prototype.filter in KB");
  let array_reduce_id = kb
    .id_of("Array.prototype.reduce")
    .expect("Array.prototype.reduce in KB");

  let lowered = hir_js::lower_from_source_with_kind(
    FileKind::Js,
    r#"[1, 2, 3].map(x => x).filter(x => x).reduce((a, b) => a + b, 0);"#,
  )
  .unwrap();
  let (body_id, root_expr) = first_root_expr(&lowered);
  let body = lowered.body(body_id).expect("body");

  let tables = analyze_body_tables_untyped(&kb, &lowered);
  let root_tables = tables.get(&body_id).expect("root body tables");

  let mut saw_filter_chain = false;
  let mut saw_reduce_chain = false;

  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    let hir_js::ExprKind::ArrayChain { array, ops } = &expr.kind else {
      continue;
    };
    let Some(terminal) = ops.last() else {
      continue;
    };

    match terminal {
      hir_js::ArrayChainOp::Filter(callback) => {
        saw_filter_chain = true;
        assert_eq!(root_tables.resolved_call[expr_id.0 as usize], Some(array_filter_id));
        assert_eq!(
          root_tables.resolved_call_receiver[expr_id.0 as usize],
          Some(*array)
        );
        assert_eq!(
          root_tables.resolved_call_args[expr_id.0 as usize].as_slice(),
          [*callback]
        );
      }
      hir_js::ArrayChainOp::Reduce(callback, init) => {
        saw_reduce_chain |= expr_id == root_expr;
        assert_eq!(root_tables.resolved_call[expr_id.0 as usize], Some(array_reduce_id));
        assert_eq!(
          root_tables.resolved_call_receiver[expr_id.0 as usize],
          Some(*array)
        );
        let mut expected = vec![*callback];
        if let Some(init) = init {
          expected.push(*init);
        }
        assert_eq!(
          root_tables.resolved_call_args[expr_id.0 as usize].as_slice(),
          expected.as_slice()
        );
      }
      _ => {}
    }
  }

  assert!(saw_filter_chain, "expected to see a filter ArrayChain node");
  assert!(saw_reduce_chain, "expected reduce root expr to be an ArrayChain node");
}

#[cfg(feature = "typed")]
mod typed {
  use super::*;

  use effect_js::analyze_body_tables_typed;
  use effect_js::typed::TypedProgram;
  use std::sync::Arc;
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  use typecheck_ts::{FileKey, MemoryHost, Program};

  const INDEX_TS: &str = r#"
export {};

const s: string = "x";
s.toLowerCase();
"#;

  fn es2015_host() -> MemoryHost {
    MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
      ..Default::default()
    })
  }

  fn find_call_expr(
    lowered: &hir_js::LowerResult,
    body: &hir_js::Body,
    recv_name: &str,
    prop_name: &str,
  ) -> ExprId {
    body
      .exprs
      .iter()
      .enumerate()
      .find_map(|(idx, expr)| {
        let hir_js::ExprKind::Call(call) = &expr.kind else {
          return None;
        };
        if call.optional || call.is_new {
          return None;
        }
        let hir_js::ExprKind::Member(member) = &body.exprs.get(call.callee.0 as usize)?.kind else {
          return None;
        };
        if member.optional {
          return None;
        }
        let hir_js::ObjectKey::Ident(prop) = &member.property else {
          return None;
        };
        if lowered.names.resolve(*prop)? != prop_name {
          return None;
        }
        let hir_js::ExprKind::Ident(recv) = &body.exprs.get(member.object.0 as usize)?.kind else {
          return None;
        };
        (lowered.names.resolve(*recv)? == recv_name).then_some(ExprId(idx as u32))
      })
      .unwrap_or_else(|| panic!("expected to find `{recv_name}.{prop_name}(...)` call"))
  }

  #[test]
  fn resolves_string_to_lower_case_in_body_tables_typed() {
    let index_key = FileKey::new("index.ts");

    let mut host = es2015_host();
    host.insert(index_key.clone(), INDEX_TS);

    let program = Arc::new(Program::new(host, vec![index_key.clone()]));
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "typecheck diagnostics: {diagnostics:#?}"
    );

    let file = program.file_id(&index_key).expect("index.ts is loaded");
    let lowered = program.hir_lowered(file).expect("HIR lowered");
    let root_body = lowered.root_body();
    let body = lowered.body(root_body).expect("root body exists");

    let types = TypedProgram::from_program(Arc::clone(&program), file);
    let kb = load_default_api_database();
    let to_lower_id = kb
      .id_of("String.prototype.toLowerCase")
      .expect("String.prototype.toLowerCase in KB");

    let call_expr = find_call_expr(&lowered, body, "s", "toLowerCase");

    let tables = analyze_body_tables_typed(&kb, &lowered, &types);
    let root_tables = tables.get(&root_body).expect("root body tables");

    assert_eq!(root_tables.resolved_call[call_expr.0 as usize], Some(to_lower_id));
    assert!(root_tables.resolved_call_args[call_expr.0 as usize].is_empty());

    let hir_js::ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
      panic!("expected Call expr for s.toLowerCase()");
    };
    let hir_js::ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
      panic!("expected Member callee for s.toLowerCase()");
    };
    assert_eq!(
      root_tables.resolved_call_receiver[call_expr.0 as usize],
      Some(member.object)
    );
  }
}
