use std::sync::Arc;

use typecheck_ts::db::TypecheckDb;
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::{FileId, FileKey, FileOrigin};

#[test]
fn reverse_module_dependencies() {
  let mut db = TypecheckDb::default();
  let file_a = FileId(0);
  let file_b = FileId(1);
  let file_c = FileId(2);
  let file_d = FileId(3);
  let file_e = FileId(4);
  let key_a = FileKey::new("a.ts");
  let key_b = FileKey::new("b.ts");
  let key_c = FileKey::new("c.ts");
  let key_d = FileKey::new("d.ts");
  let key_e = FileKey::new("e.ts");

  db.set_roots(Arc::from([key_a.clone(), key_c.clone()]));

  db.set_file(
    file_a,
    key_a,
    FileKind::Ts,
    Arc::from(r#"import "./b";"#),
    FileOrigin::Source,
  );
  db.set_file(
    file_b,
    key_b,
    FileKind::Ts,
    Arc::from(r#"import "./d";"#),
    FileOrigin::Source,
  );
  db.set_file(
    file_c,
    key_c,
    FileKind::Ts,
    Arc::from(r#"import "./b";"#),
    FileOrigin::Source,
  );
  db.set_file(
    file_d,
    key_d,
    FileKind::Ts,
    Arc::from("export const d = 1;"),
    FileOrigin::Source,
  );
  db.set_file(
    file_e,
    key_e,
    FileKind::Ts,
    Arc::from(r#"import "./b";"#),
    FileOrigin::Source,
  );

  db.set_module_resolution(file_a, Arc::<str>::from("./b"), Some(file_b));
  db.set_module_resolution(file_c, Arc::<str>::from("./b"), Some(file_b));
  db.set_module_resolution(file_b, Arc::<str>::from("./d"), Some(file_d));
  db.set_module_resolution(file_e, Arc::<str>::from("./b"), Some(file_b));

  assert_eq!(db.module_reverse_deps(file_b).as_ref(), &[file_a, file_c]);
  assert_eq!(db.module_reverse_deps(file_d).as_ref(), &[file_b]);
  assert_eq!(
    db.module_reverse_deps_transitive(file_d).as_ref(),
    &[file_a, file_b, file_c],
  );
}

