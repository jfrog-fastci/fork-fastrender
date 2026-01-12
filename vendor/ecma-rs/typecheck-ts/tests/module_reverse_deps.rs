use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn module_reverse_deps_follow_import_graph() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  let mut host = MemoryHost::with_options(options);

  let entry = FileKey::new("entry.ts");
  let a = FileKey::new("a.ts");
  let b = FileKey::new("b.ts");
  let c = FileKey::new("c.ts");

  host.insert(a.clone(), "export const a = 1;");
  host.insert(b.clone(), "import { a } from \"./a\"; export const b = a;");
  host.insert(c.clone(), "import { a } from \"./a\"; export const c = a;");
  host.insert(
    entry.clone(),
    r#"
import "./b";
import "./c";
export const entry = 0;
"#,
  );

  host.link(b.clone(), "./a", a.clone());
  host.link(c.clone(), "./a", a.clone());
  host.link(entry.clone(), "./b", b.clone());
  host.link(entry.clone(), "./c", c.clone());

  let program = Program::new(host, vec![entry.clone()]);
  let entry_id = program.file_id(&entry).expect("entry file id");
  let a_id = program.file_id(&a).expect("a file id");
  let b_id = program.file_id(&b).expect("b file id");
  let c_id = program.file_id(&c).expect("c file id");

  let mut expected_direct = vec![b_id, c_id];
  expected_direct.sort_by_key(|id| id.0);
  assert_eq!(program.reverse_module_deps(a_id), expected_direct);

  let mut expected_transitive = vec![a_id, b_id, c_id, entry_id];
  expected_transitive.sort_by_key(|id| id.0);
  assert_eq!(
    program.transitive_reverse_module_deps(a_id),
    expected_transitive
  );
}

