#![cfg(feature = "typed")]

use effect_js::{analyze_body_tables_typed, resolve_member};
use hir_js::{ExprId, ExprKind, ObjectKey};
use std::sync::Arc;
use effect_js::typed::TypedProgram;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
export {};

interface URL {
  pathname: string;
  href: string;
}

const u: URL = { pathname: "", href: "" };
u.pathname;
u.href;

const xs: number[] = [1];
xs.length;
"#;

fn find_member_expr(
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
      let ExprKind::Member(member) = &expr.kind else {
        return None;
      };
      if member.optional {
        return None;
      }
      let ObjectKey::Ident(prop) = member.property else {
        return None;
      };
      let prop = lowered.names.resolve(prop)?;
      if prop != prop_name {
        return None;
      }

      let recv = body.exprs.get(member.object.0 as usize)?;
      let ExprKind::Ident(name) = recv.kind else {
        return None;
      };
      let recv = lowered.names.resolve(name)?;
      (recv == recv_name).then_some(ExprId(idx as u32))
    })
    .unwrap_or_else(|| panic!("expected to find `{recv_name}.{prop_name}` member expression"))
}

#[test]
fn resolves_known_member_reads_typed() {
  let index_key = FileKey::new("index.ts");

  let mut host = MemoryHost::new();
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

  let pathname = find_member_expr(&lowered, body, "u", "pathname");
  let href = find_member_expr(&lowered, body, "u", "href");
  let length = find_member_expr(&lowered, body, "xs", "length");

  let resolved_pathname = resolve_member(&lowered, root_body, pathname, &types).expect("resolve u.pathname");
  assert_eq!(resolved_pathname.api.as_str(), "URL.prototype.pathname");
  assert_eq!(resolved_pathname.member, pathname);
  let ExprKind::Member(member) = &body.exprs[pathname.0 as usize].kind else {
    panic!("expected member expression for u.pathname");
  };
  assert_eq!(resolved_pathname.receiver, member.object);

  let resolved_href = resolve_member(&lowered, root_body, href, &types).expect("resolve u.href");
  assert_eq!(resolved_href.api.as_str(), "URL.prototype.href");

  let resolved_length = resolve_member(&lowered, root_body, length, &types).expect("resolve xs.length");
  assert_eq!(resolved_length.api.as_str(), "Array.prototype.length");

  // Ensure side tables are wired up as well.
  let tables = analyze_body_tables_typed(&lowered, &types);
  let root_tables = tables.get(&root_body).expect("root body tables");
  assert_eq!(
    root_tables.resolved_member[pathname.0 as usize].map(|api| api.as_str()),
    Some("URL.prototype.pathname")
  );
  assert_eq!(
    root_tables.resolved_member[length.0 as usize].map(|api| api.as_str()),
    Some("Array.prototype.length")
  );
}
