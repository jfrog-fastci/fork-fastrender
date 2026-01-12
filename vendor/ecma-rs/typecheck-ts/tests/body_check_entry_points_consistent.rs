use std::collections::BTreeMap;
use std::sync::Arc;

use typecheck_ts::{codes, BodyId, FileKey, MemoryHost, Program};

#[test]
fn body_results_match_between_check_and_check_body() {
  let mut host = MemoryHost::default();
  let entry = FileKey::new("entry.ts");
  let dep = FileKey::new("dep.ts");
  let entry_source = r#"
import { add } from "./dep";

export const total = add(1, 2);

function local(x: number) {
  return x + total;
}

local(3);
"#;
  let dep_source = r#"export function add(a: number, b: number) { return a + b; }"#;

  host.insert(entry.clone(), Arc::from(entry_source.to_string()));
  host.insert(dep.clone(), Arc::from(dep_source.to_string()));
  host.link(entry.clone(), "./dep", dep.clone());

  let program_checked = Program::new(host.clone(), vec![entry.clone()]);
  let _ = program_checked.check();

  let mut bodies: Vec<BodyId> = Vec::new();
  for file in program_checked.reachable_files() {
    bodies.extend(program_checked.bodies_in_file(file));
  }
  bodies.sort_by_key(|id| id.0);
  bodies.dedup();

  let mut from_check: BTreeMap<BodyId, (Vec<typecheck_ts::Diagnostic>, usize)> = BTreeMap::new();
  for body in bodies.iter().copied() {
    let res = program_checked.check_body(body);
    let mut diagnostics = res.diagnostics().to_vec();
    codes::normalize_diagnostics(&mut diagnostics);
    from_check.insert(body, (diagnostics, res.expr_types().len()));
  }

  let program_bodies = Program::new(host, vec![entry]);
  let mut from_check_body: BTreeMap<BodyId, (Vec<typecheck_ts::Diagnostic>, usize)> =
    BTreeMap::new();
  for body in bodies.iter().copied() {
    let res = program_bodies.check_body(body);
    let mut diagnostics = res.diagnostics().to_vec();
    codes::normalize_diagnostics(&mut diagnostics);
    from_check_body.insert(body, (diagnostics, res.expr_types().len()));
  }

  assert_eq!(from_check_body, from_check);
}

