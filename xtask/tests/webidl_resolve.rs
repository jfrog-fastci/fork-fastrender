use std::path::Path;

use xtask::webidl::{
  extract_webidl_blocks_from_bikeshed, extract_webidl_blocks_from_whatwg_html, parse_webidl,
};
use xtask::webidl::resolve::{resolve_webidl_world, ExposureTarget};

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

  let inputs = [
    ("specs/whatwg-dom/dom.bs", "DOM"),
    ("specs/whatwg-url/url.bs", "URL"),
    ("specs/whatwg-fetch/fetch.bs", "Fetch"),
  ];

  let mut combined_idl = String::new();

  for (rel, label) in inputs {
    let path = repo_root.join(rel);
    if !path.exists() {
      eprintln!("skipping WebIDL smoke test: missing {label} submodule at {}", path.display());
      return;
    }
    let src = std::fs::read_to_string(&path).expect("read spec source");
    for block in extract_webidl_blocks_from_bikeshed(&src) {
      combined_idl.push_str(&block);
      combined_idl.push('\n');
    }
  }

  let parsed = parse_webidl(&combined_idl).unwrap();
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

  let path = repo_root.join("specs/whatwg-html/source");
  if !path.exists() {
    eprintln!(
      "skipping WebIDL smoke test: missing WHATWG HTML source at {}",
      path.display()
    );
    return;
  }

  let src = std::fs::read_to_string(&path).expect("read spec source");
  let mut combined_idl = String::new();
  for block in extract_webidl_blocks_from_whatwg_html(&src) {
    combined_idl.push_str(&block);
    combined_idl.push('\n');
  }

  let parsed = parse_webidl(&combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    !resolved.interfaces.is_empty(),
    "expected non-empty interface set from WHATWG HTML"
  );
}
