use typecheck_ts::FileKey;
use typecheck_ts::lib_support::{CompilerOptions, ModuleKind};
use typecheck_ts_harness::runner::HarnessFileSet;
use typecheck_ts_harness::VirtualFile;

#[test]
fn resolves_package_json_types_entrypoints() {
  let files = vec![
    VirtualFile {
      name: "/src/app.ts".to_string(),
      content: "import \"pkg\";\n".into(),
    },
    VirtualFile {
      name: "/node_modules/pkg/package.json".to_string(),
      content: r#"{ "types": "./dist/index.d.ts" }"#.into(),
    },
    VirtualFile {
      name: "/node_modules/pkg/dist/index.d.ts".to_string(),
      content: "export {};\n".into(),
    },
  ];

  let file_set = HarnessFileSet::new(&files);
  let mut opts = CompilerOptions::default();
  opts.module = Some(ModuleKind::Node16);
  opts.module_resolution = Some("node16".to_string());
  let resolved = file_set.resolve_import(&FileKey::new("/src/app.ts"), "pkg", &opts);
  assert_eq!(
    resolved,
    Some(FileKey::new("/node_modules/pkg/dist/index.d.ts"))
  );
}

#[test]
fn resolves_package_json_exports_types_entrypoints() {
  let files = vec![
    VirtualFile {
      name: "/src/app.ts".to_string(),
      content: "import \"pkg\";\n".into(),
    },
    VirtualFile {
      name: "/node_modules/pkg/package.json".to_string(),
      content: r#"{ "exports": { ".": { "types": "./dist/index.d.ts" } } }"#.into(),
    },
    VirtualFile {
      name: "/node_modules/pkg/dist/index.d.ts".to_string(),
      content: "export {};\n".into(),
    },
  ];

  let file_set = HarnessFileSet::new(&files);
  let mut opts = CompilerOptions::default();
  opts.module = Some(ModuleKind::Node16);
  opts.module_resolution = Some("node16".to_string());
  let resolved = file_set.resolve_import(&FileKey::new("/src/app.ts"), "pkg", &opts);
  assert_eq!(
    resolved,
    Some(FileKey::new("/node_modules/pkg/dist/index.d.ts"))
  );
}
