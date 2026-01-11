#![cfg(feature = "typed")]

use effect_js::{analyze_body_tables_typed, resolve_member};
use effect_js::ApiId;
use hir_js::{ExprId, ExprKind, ObjectKey};
use std::sync::Arc;
use effect_js::typed::TypedProgram;
use typecheck_ts::{FileKey, MemoryHost, Program};

const INDEX_TS: &str = r#"
export {};

interface URL {
  pathname: string;
  href: string;
  origin: string;
  protocol: string;
  host: string;
  hostname: string;
  port: string;
  search: string;
  hash: string;
}

const u: URL = {
  pathname: "",
  href: "",
  origin: "",
  protocol: "",
  host: "",
  hostname: "",
  port: "",
  search: "",
  hash: "",
};
u.pathname;
u.href;
u.origin;
u.protocol;
u.host;
u.hostname;
u.port;
u.search;
u.hash;
u["pathname"];

const s: string = "hi";
s.length;
s["length"];

const m: Map<string, number> = new Map();
m.size;
m["size"];

const set: Set<string> = new Set();
set.size;
set["size"];

const xs: number[] = [1];
xs.length;
xs["length"];
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

fn find_computed_member_expr(
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
      let ObjectKey::Computed(prop_expr) = member.property else {
        return None;
      };
      let prop_expr = body.exprs.get(prop_expr.0 as usize)?;
      match &prop_expr.kind {
        ExprKind::Literal(hir_js::Literal::String(s)) if s.lossy.as_str() == prop_name => {}
        _ => return None,
      }

      let recv = body.exprs.get(member.object.0 as usize)?;
      let ExprKind::Ident(name) = recv.kind else {
        return None;
      };
      let recv = lowered.names.resolve(name)?;
      (recv == recv_name).then_some(ExprId(idx as u32))
    })
    .unwrap_or_else(|| panic!("expected to find `{recv_name}[\"{prop_name}\"]` member expression"))
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
  let origin = find_member_expr(&lowered, body, "u", "origin");
  let protocol = find_member_expr(&lowered, body, "u", "protocol");
  let host = find_member_expr(&lowered, body, "u", "host");
  let hostname = find_member_expr(&lowered, body, "u", "hostname");
  let port = find_member_expr(&lowered, body, "u", "port");
  let search = find_member_expr(&lowered, body, "u", "search");
  let hash = find_member_expr(&lowered, body, "u", "hash");
  let str_length = find_member_expr(&lowered, body, "s", "length");
  let map_size = find_member_expr(&lowered, body, "m", "size");
  let set_size = find_member_expr(&lowered, body, "set", "size");
  let length = find_member_expr(&lowered, body, "xs", "length");
  let computed_pathname = find_computed_member_expr(&lowered, body, "u", "pathname");
  let computed_str_length = find_computed_member_expr(&lowered, body, "s", "length");
  let computed_map_size = find_computed_member_expr(&lowered, body, "m", "size");
  let computed_set_size = find_computed_member_expr(&lowered, body, "set", "size");
  let computed_length = find_computed_member_expr(&lowered, body, "xs", "length");

  let resolved_pathname = resolve_member(&lowered, root_body, pathname, &types).expect("resolve u.pathname");
  assert_eq!(resolved_pathname.api, ApiId::from_name("URL.prototype.pathname"));
  assert_eq!(resolved_pathname.member, pathname);
  let ExprKind::Member(member) = &body.exprs[pathname.0 as usize].kind else {
    panic!("expected member expression for u.pathname");
  };
  assert_eq!(resolved_pathname.receiver, member.object);

  let resolved_href = resolve_member(&lowered, root_body, href, &types).expect("resolve u.href");
  assert_eq!(resolved_href.api, ApiId::from_name("URL.prototype.href"));

  let resolved_origin =
    resolve_member(&lowered, root_body, origin, &types).expect("resolve u.origin");
  assert_eq!(resolved_origin.api, ApiId::from_name("URL.prototype.origin"));

  let resolved_protocol =
    resolve_member(&lowered, root_body, protocol, &types).expect("resolve u.protocol");
  assert_eq!(resolved_protocol.api, ApiId::from_name("URL.prototype.protocol"));

  let resolved_host = resolve_member(&lowered, root_body, host, &types).expect("resolve u.host");
  assert_eq!(resolved_host.api, ApiId::from_name("URL.prototype.host"));

  let resolved_hostname =
    resolve_member(&lowered, root_body, hostname, &types).expect("resolve u.hostname");
  assert_eq!(resolved_hostname.api, ApiId::from_name("URL.prototype.hostname"));

  let resolved_port = resolve_member(&lowered, root_body, port, &types).expect("resolve u.port");
  assert_eq!(resolved_port.api, ApiId::from_name("URL.prototype.port"));

  let resolved_search =
    resolve_member(&lowered, root_body, search, &types).expect("resolve u.search");
  assert_eq!(resolved_search.api, ApiId::from_name("URL.prototype.search"));

  let resolved_hash = resolve_member(&lowered, root_body, hash, &types).expect("resolve u.hash");
  assert_eq!(resolved_hash.api, ApiId::from_name("URL.prototype.hash"));

  let resolved_str_length =
    resolve_member(&lowered, root_body, str_length, &types).expect("resolve s.length");
  assert_eq!(resolved_str_length.api, ApiId::from_name("String.prototype.length"));

  let resolved_computed_str_length =
    resolve_member(&lowered, root_body, computed_str_length, &types).expect("resolve s[\"length\"]");
  assert_eq!(resolved_computed_str_length.api, ApiId::from_name("String.prototype.length"));

  let resolved_map_size =
    resolve_member(&lowered, root_body, map_size, &types).expect("resolve m.size");
  assert_eq!(resolved_map_size.api, ApiId::from_name("Map.prototype.size"));

  let resolved_computed_map_size =
    resolve_member(&lowered, root_body, computed_map_size, &types).expect("resolve m[\"size\"]");
  assert_eq!(resolved_computed_map_size.api, ApiId::from_name("Map.prototype.size"));

  let resolved_set_size =
    resolve_member(&lowered, root_body, set_size, &types).expect("resolve set.size");
  assert_eq!(resolved_set_size.api, ApiId::from_name("Set.prototype.size"));

  let resolved_computed_set_size =
    resolve_member(&lowered, root_body, computed_set_size, &types).expect("resolve set[\"size\"]");
  assert_eq!(resolved_computed_set_size.api, ApiId::from_name("Set.prototype.size"));

  let resolved_length = resolve_member(&lowered, root_body, length, &types).expect("resolve xs.length");
  assert_eq!(resolved_length.api, ApiId::from_name("Array.prototype.length"));

  let resolved_computed_length =
    resolve_member(&lowered, root_body, computed_length, &types).expect("resolve xs[\"length\"]");
  assert_eq!(resolved_computed_length.api, ApiId::from_name("Array.prototype.length"));

  let resolved_computed_pathname =
    resolve_member(&lowered, root_body, computed_pathname, &types).expect("resolve u[\"pathname\"]");
  assert_eq!(
    resolved_computed_pathname.api,
    ApiId::from_name("URL.prototype.pathname")
  );

  // Ensure side tables are wired up as well.
  let tables = analyze_body_tables_typed(&lowered, &types);
  let root_tables = tables.get(&root_body).expect("root body tables");
  assert_eq!(
    root_tables.resolved_member[pathname.0 as usize],
    Some(ApiId::from_name("URL.prototype.pathname"))
  );
  assert_eq!(
    root_tables.resolved_member[origin.0 as usize],
    Some(ApiId::from_name("URL.prototype.origin"))
  );
  assert_eq!(
    root_tables.resolved_member[length.0 as usize],
    Some(ApiId::from_name("Array.prototype.length"))
  );
  assert_eq!(
    root_tables.resolved_member[str_length.0 as usize],
    Some(ApiId::from_name("String.prototype.length"))
  );
  assert_eq!(
    root_tables.resolved_member[computed_length.0 as usize],
    Some(ApiId::from_name("Array.prototype.length"))
  );
  assert_eq!(
    root_tables.resolved_member[computed_str_length.0 as usize],
    Some(ApiId::from_name("String.prototype.length"))
  );
  assert_eq!(
    root_tables.resolved_member[map_size.0 as usize],
    Some(ApiId::from_name("Map.prototype.size"))
  );
  assert_eq!(
    root_tables.resolved_member[computed_map_size.0 as usize],
    Some(ApiId::from_name("Map.prototype.size"))
  );
  assert_eq!(
    root_tables.resolved_member[set_size.0 as usize],
    Some(ApiId::from_name("Set.prototype.size"))
  );
  assert_eq!(
    root_tables.resolved_member[computed_set_size.0 as usize],
    Some(ApiId::from_name("Set.prototype.size"))
  );
  assert_eq!(
    root_tables.resolved_member[computed_pathname.0 as usize],
    Some(ApiId::from_name("URL.prototype.pathname"))
  );
}
