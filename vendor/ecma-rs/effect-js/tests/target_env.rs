use effect_js::{
  callsite_info_for_args, callsite_info_for_args_for_target, ApiDatabase, TargetEnv, TargetedKb,
  WebPlatform,
};
use hir_js::{BodyId, ExprId, StmtKind};

fn first_stmt_expr(lowered: &hir_js::LowerResult) -> (BodyId, ExprId) {
  let root = lowered.root_body();
  let root_body = lowered.body(root).expect("root body");
  let first_stmt = *root_body.root_stmts.first().expect("root stmt");
  let stmt = &root_body.stmts[first_stmt.0 as usize];
  match stmt.kind {
    StmtKind::Expr(expr) => (root, expr),
    _ => panic!("expected expression statement"),
  }
}

fn synthetic_db() -> ApiDatabase {
  let node_yaml = r#"
schema: 1
apis:
  - name: foo
    effects: Io
    purity: Impure
    properties:
      tag: node
"#;

  let web_yaml = r#"
schema: 1
apis:
  - name: foo
    effects: Pure
    purity: Pure
    properties:
      tag: web
"#;

  ApiDatabase::load_from_sources(&[("node/test.yaml", node_yaml), ("web/test.yaml", web_yaml)])
    .expect("load synthetic KB")
}

#[test]
fn targeted_kb_selects_env_specific_entries() {
  let db = synthetic_db();
  let id = db.id_of("foo").expect("foo in kb");

  let node_target = TargetEnv::Node {
    version: "20.0.0".parse().expect("valid semver"),
  };
  let web_target = TargetEnv::Web {
    platform: WebPlatform::Generic,
  };

  let node_kb = TargetedKb::new(&db, node_target);
  assert_eq!(
    node_kb
      .get_by_id(id)
      .and_then(|api| api.properties.get("tag"))
      .and_then(|v| v.as_str()),
    Some("node")
  );

  let web_kb = TargetedKb::new(&db, web_target);
  assert_eq!(
    web_kb
      .get("foo")
      .and_then(|api| api.properties.get("tag"))
      .and_then(|v| v.as_str()),
    Some("web")
  );

  // Legacy `ApiDatabase::get` uses `TargetEnv::Unknown`, which intentionally
  // biases toward Node when both variants exist.
  assert_eq!(
    db.get("foo")
      .and_then(|api| api.properties.get("tag"))
      .and_then(|v| v.as_str()),
    Some("node")
  );
}

#[test]
fn callback_analysis_uses_targeted_semantics() {
  let db = synthetic_db();

  let lowered = hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(() => foo());")
    .unwrap();
  let (body_id, call_expr) = first_stmt_expr(&lowered);

  let node_target = TargetEnv::Node {
    version: "20.0.0".parse().expect("valid semver"),
  };
  let web_target = TargetEnv::Web {
    platform: WebPlatform::Generic,
  };

  let node_info = callsite_info_for_args_for_target(&lowered, body_id, call_expr, &db, &node_target);
  assert_eq!(node_info.callback_is_pure, Some(false));

  let web_info = callsite_info_for_args_for_target(&lowered, body_id, call_expr, &db, &web_target);
  assert_eq!(web_info.callback_is_pure, Some(true));

  // Legacy callback analysis defaults to `TargetEnv::Unknown`, which biases
  // toward Node.
  let legacy = callsite_info_for_args(&lowered, body_id, call_expr, &db);
  assert_eq!(legacy.callback_is_pure, Some(false));
}

