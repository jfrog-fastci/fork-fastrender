use std::path::Path;

use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::resolve::{resolve_webidl_world, ExposureTarget};
use xtask::webidl::{parse_webidl, BuiltinType, IdlType};

#[test]
fn merges_partials_and_includes_with_deterministic_ordering() {
  let idl = r#"
    [Exposed=Window]
    interface Foo {
      attribute long a;
    };

    partial interface Foo {
      attribute long b;
    };

    interface mixin Mixin {
      [LegacyUnforgeable] attribute long c;
    };

    Foo includes Mixin;
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let foo = resolved.interface("Foo").expect("Foo resolved");

  let names: Vec<_> = foo
    .members
    .iter()
    .map(|m| m.name.as_deref().unwrap_or(&m.raw))
    .collect();
  assert_eq!(names, vec!["a", "b", "c"]);

  let c = foo
    .members
    .iter()
    .find(|m| m.name.as_deref() == Some("c"))
    .expect("c present");
  assert!(c.ext_attrs.iter().any(|a| a.name == "LegacyUnforgeable"));

  // Exercise exposure filtering surface.
  let _filtered = resolved.filter_by_exposure(ExposureTarget::Window);
}

#[test]
fn smoke_resolve_dom_url_fetch() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [
    WebIdlSource {
      rel_path: "specs/whatwg-dom/dom.bs",
      label: "DOM",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-url/url.bs",
      label: "URL",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-fetch/fetch.bs",
      label: "Fetch",
    },
  ];

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  if !loaded.missing_sources.is_empty() {
    for (label, path) in &loaded.missing_sources {
      eprintln!("skipping WebIDL smoke test: missing {label} submodule at {}", path.display());
    }
    return;
  }

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    !resolved.interfaces.is_empty(),
    "expected non-empty interface set from DOM+URL+Fetch"
  );
}

#[test]
fn smoke_resolve_whatwg_html() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [WebIdlSource {
    rel_path: "specs/whatwg-html/source",
    label: "HTML",
  }];

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  if !loaded.missing_sources.is_empty() {
    for (label, path) in &loaded.missing_sources {
      eprintln!("skipping WebIDL smoke test: missing {label} source at {}", path.display());
    }
    return;
  }

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    !resolved.interfaces.is_empty(),
    "expected non-empty interface set from WHATWG HTML"
  );
}

#[test]
fn prelude_provides_domhighrestimestamp_typedef() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  // Loading any spec sources is optional; the prelude should load unconditionally.
  let sources = [WebIdlSource {
    rel_path: "specs/whatwg-dom/dom.bs",
    label: "DOM",
  }];

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);

  assert!(
    resolved.typedefs.contains_key("DOMHighResTimeStamp"),
    "expected prelude to provide DOMHighResTimeStamp typedef"
  );

  // Exercise typedef resolution surface too (canonicalize through to `double`).
  let ty = resolved.resolve_typedef("DOMHighResTimeStamp").unwrap();
  assert_eq!(ty, IdlType::Builtin(BuiltinType::Double));
}

#[test]
fn typedef_resolution_expands_chains_and_detects_cycles() {
  let idl = r#"
    typedef long A;
    typedef A B;
    typedef sequence<B> SeqB;

    typedef CycleB CycleA;
    typedef CycleA CycleB;
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);

  assert_eq!(resolved.resolve_typedef("A").unwrap(), IdlType::Builtin(BuiltinType::Long));
  assert_eq!(resolved.resolve_typedef("B").unwrap(), IdlType::Builtin(BuiltinType::Long));

  assert_eq!(
    resolved.resolve_typedef("SeqB").unwrap(),
    IdlType::Sequence(Box::new(IdlType::Builtin(BuiltinType::Long)))
  );

  assert!(resolved.resolve_typedef("CycleA").is_err());
}

