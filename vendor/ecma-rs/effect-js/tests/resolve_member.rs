#![cfg(feature = "typed")]

use effect_js::{analyze_body_tables_typed, resolve_member};
use effect_js::load_default_api_database;
use hir_js::{ExprId, ExprKind, ObjectKey};
use std::sync::Arc;
use effect_js::typed::TypedProgram;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
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
u["pathname"];
u["href"];
u["origin"];
u["protocol"];
u["host"];
u["hostname"];
u["port"];
u["search"];
u["hash"];

const s: string = "hi";
s["length"];

const m: Map<string, number> = new Map();
m["size"];

const set: Set<string> = new Set();
set["size"];

const xs: number[] = [1];
xs["length"];
"#;

fn es2015_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es2015").expect("LibName::parse(es2015)")],
    ..Default::default()
  })
}

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
      let prop = match &member.property {
        ObjectKey::Ident(prop) => lowered.names.resolve(*prop)?,
        ObjectKey::Computed(expr_id) => {
          let expr = body.exprs.get(expr_id.0 as usize)?;
          match &expr.kind {
            ExprKind::Literal(hir_js::Literal::String(s)) => s.lossy.as_str(),
            _ => return None,
          }
        }
        _ => return None,
      };
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
    .unwrap_or_else(|| panic!("expected to find `{recv_name}` member expression for `{prop_name}`"))
}

#[test]
fn resolves_known_member_reads_typed() {
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

  let pathname_id = kb.id_of("URL.prototype.pathname").unwrap();
  let href_id = kb.id_of("URL.prototype.href").unwrap();
  let origin_id = kb.id_of("URL.prototype.origin").unwrap();
  let protocol_id = kb.id_of("URL.prototype.protocol").unwrap();
  let host_id = kb.id_of("URL.prototype.host").unwrap();
  let hostname_id = kb.id_of("URL.prototype.hostname").unwrap();
  let port_id = kb.id_of("URL.prototype.port").unwrap();
  let search_id = kb.id_of("URL.prototype.search").unwrap();
  let hash_id = kb.id_of("URL.prototype.hash").unwrap();
  let str_length_id = kb.id_of("String.prototype.length").unwrap();
  let map_size_id = kb.id_of("Map.prototype.size").unwrap();
  let set_size_id = kb.id_of("Set.prototype.size").unwrap();
  let array_length_id = kb.id_of("Array.prototype.length").unwrap();

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
  let array_length = find_member_expr(&lowered, body, "xs", "length");

  let resolved_pathname =
    resolve_member(&kb, &lowered, root_body, pathname, &types).expect("resolve u.pathname");
  assert_eq!(resolved_pathname.api, "URL.prototype.pathname");
  assert_eq!(resolved_pathname.api_id, pathname_id);
  assert_eq!(resolved_pathname.member, pathname);
  let ExprKind::Member(member) = &body.exprs[pathname.0 as usize].kind else {
    panic!("expected member expression for u.pathname");
  };
  assert_eq!(resolved_pathname.receiver, member.object);

  let resolved_href = resolve_member(&kb, &lowered, root_body, href, &types).expect("resolve u.href");
  assert_eq!(resolved_href.api, "URL.prototype.href");
  assert_eq!(resolved_href.api_id, href_id);

  let resolved_origin =
    resolve_member(&kb, &lowered, root_body, origin, &types).expect("resolve u.origin");
  assert_eq!(resolved_origin.api, "URL.prototype.origin");
  assert_eq!(resolved_origin.api_id, origin_id);

  let resolved_protocol =
    resolve_member(&kb, &lowered, root_body, protocol, &types).expect("resolve u.protocol");
  assert_eq!(resolved_protocol.api, "URL.prototype.protocol");
  assert_eq!(resolved_protocol.api_id, protocol_id);

  let resolved_host = resolve_member(&kb, &lowered, root_body, host, &types).expect("resolve u.host");
  assert_eq!(resolved_host.api, "URL.prototype.host");
  assert_eq!(resolved_host.api_id, host_id);

  let resolved_hostname =
    resolve_member(&kb, &lowered, root_body, hostname, &types).expect("resolve u.hostname");
  assert_eq!(resolved_hostname.api, "URL.prototype.hostname");
  assert_eq!(resolved_hostname.api_id, hostname_id);

  let resolved_port = resolve_member(&kb, &lowered, root_body, port, &types).expect("resolve u.port");
  assert_eq!(resolved_port.api, "URL.prototype.port");
  assert_eq!(resolved_port.api_id, port_id);

  let resolved_search =
    resolve_member(&kb, &lowered, root_body, search, &types).expect("resolve u.search");
  assert_eq!(resolved_search.api, "URL.prototype.search");
  assert_eq!(resolved_search.api_id, search_id);

  let resolved_hash = resolve_member(&kb, &lowered, root_body, hash, &types).expect("resolve u.hash");
  assert_eq!(resolved_hash.api, "URL.prototype.hash");
  assert_eq!(resolved_hash.api_id, hash_id);

  let resolved_str_length =
    resolve_member(&kb, &lowered, root_body, str_length, &types).expect("resolve s.length");
  assert_eq!(resolved_str_length.api, "String.prototype.length");
  assert_eq!(resolved_str_length.api_id, str_length_id);

  let resolved_map_size =
    resolve_member(&kb, &lowered, root_body, map_size, &types).expect("resolve m.size");
  assert_eq!(resolved_map_size.api, "Map.prototype.size");
  assert_eq!(resolved_map_size.api_id, map_size_id);

  let resolved_set_size =
    resolve_member(&kb, &lowered, root_body, set_size, &types).expect("resolve set.size");
  assert_eq!(resolved_set_size.api, "Set.prototype.size");
  assert_eq!(resolved_set_size.api_id, set_size_id);

  let resolved_length =
    resolve_member(&kb, &lowered, root_body, array_length, &types).expect("resolve xs.length");
  assert_eq!(resolved_length.api, "Array.prototype.length");
  assert_eq!(resolved_length.api_id, array_length_id);

  // Ensure side tables are wired up as well.
  let tables = analyze_body_tables_typed(&kb, &lowered, &types);
  let root_tables = tables.get(&root_body).expect("root body tables");
  assert_eq!(root_tables.resolved_member[pathname.0 as usize], Some(pathname_id));
  assert_eq!(root_tables.resolved_member[href.0 as usize], Some(href_id));
  assert_eq!(root_tables.resolved_member[origin.0 as usize], Some(origin_id));
  assert_eq!(root_tables.resolved_member[protocol.0 as usize], Some(protocol_id));
  assert_eq!(root_tables.resolved_member[host.0 as usize], Some(host_id));
  assert_eq!(root_tables.resolved_member[hostname.0 as usize], Some(hostname_id));
  assert_eq!(root_tables.resolved_member[port.0 as usize], Some(port_id));
  assert_eq!(root_tables.resolved_member[search.0 as usize], Some(search_id));
  assert_eq!(root_tables.resolved_member[hash.0 as usize], Some(hash_id));
  assert_eq!(root_tables.resolved_member[str_length.0 as usize], Some(str_length_id));
  assert_eq!(root_tables.resolved_member[map_size.0 as usize], Some(map_size_id));
  assert_eq!(root_tables.resolved_member[set_size.0 as usize], Some(set_size_id));
  assert_eq!(root_tables.resolved_member[array_length.0 as usize], Some(array_length_id));
}
