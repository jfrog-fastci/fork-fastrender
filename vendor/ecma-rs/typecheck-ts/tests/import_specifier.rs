use typecheck_ts::{DefKind, FileKey, ImportTarget, MemoryHost, Program};

#[test]
fn import_specifier_is_preserved_for_resolved_imports() {
  let mut host = MemoryHost::new();
  let entry = FileKey::new("index.ts");
  let node_fs = FileKey::new("node_fs.ts");

  host.insert(
    entry.clone(),
    r#"import { readFile } from "node:fs"; readFile("ok");"#,
  );
  host.insert(
    node_fs.clone(),
    r#"export function readFile(path: string): void { void path; }"#,
  );
  host.link(entry.clone(), "node:fs", node_fs.clone());

  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let entry_id = program.file_id(&entry).expect("entry file id");
  let node_fs_id = program.file_id(&node_fs).expect("node:fs file id");
  let import_def = program
    .definitions_in_file(entry_id)
    .into_iter()
    .find(|def| matches!(program.def_kind(*def), Some(DefKind::Import(_))))
    .expect("import def");

  match program.def_kind(import_def) {
    Some(DefKind::Import(import)) => match import.target {
      ImportTarget::File(target) => assert_eq!(target, node_fs_id),
      ImportTarget::Unresolved { specifier } => {
        panic!("expected resolved import target; got unresolved {specifier:?}");
      }
    },
    other => panic!("expected import def kind, got {other:?}"),
  }

  assert_eq!(
    program.import_specifier(import_def),
    Some("node:fs".to_string())
  );
}

