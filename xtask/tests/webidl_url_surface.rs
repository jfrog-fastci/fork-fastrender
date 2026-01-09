use std::{collections::BTreeSet, path::Path};

use xtask::webidl::{extract_webidl_blocks_from_bikeshed, parse_webidl, ExtendedAttribute};
use xtask::webidl::resolve::resolve_webidl_world;

fn has_ext_attr_value(attrs: &[ExtendedAttribute], name: &str, value: &str) -> bool {
  attrs
    .iter()
    .any(|a| a.name == name && a.value.as_deref() == Some(value))
}

fn has_ext_attr_name(attrs: &[ExtendedAttribute], name: &str) -> bool {
  attrs.iter().any(|a| a.name == name)
}

#[test]
fn webidl_url_surface() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let path = repo_root.join("specs/whatwg-url/url.bs");
  if !path.exists() {
    eprintln!(
      "skipping WebIDL URL surface test: missing whatwg-url submodule at {}",
      path.display()
    );
    return;
  }

  let src = std::fs::read_to_string(&path).expect("read URL Bikeshed source");
  let mut combined_idl = String::new();
  for block in extract_webidl_blocks_from_bikeshed(&src) {
    combined_idl.push_str(&block);
    combined_idl.push('\n');
  }
  assert!(
    !combined_idl.trim().is_empty(),
    "expected at least one <pre class=idl> block in {}",
    path.display()
  );

  let parsed = parse_webidl(&combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);

  let url = resolved.interface("URL").expect("interface URL exists");
  let url_search_params = resolved
    .interface("URLSearchParams")
    .expect("interface URLSearchParams exists");

  assert!(
    has_ext_attr_value(&url.ext_attrs, "Exposed", "*"),
    "expected [Exposed=*] on interface URL; got {:#?}",
    url.ext_attrs
  );
  assert!(
    has_ext_attr_value(&url.ext_attrs, "LegacyWindowAlias", "webkitURL"),
    "expected [LegacyWindowAlias=webkitURL] on interface URL; got {:#?}",
    url.ext_attrs
  );

  let url_member_names: BTreeSet<String> = url.members.iter().filter_map(|m| m.name.clone()).collect();
  for expected in [
    "constructor",
    "parse",
    "canParse",
    "href",
    "origin",
    "protocol",
    "username",
    "password",
    "host",
    "hostname",
    "port",
    "pathname",
    "search",
    "searchParams",
    "hash",
    "toJSON",
  ] {
    assert!(
      url_member_names.contains(expected),
      "interface URL missing member {expected}; got {:#?}",
      url_member_names
    );
  }

  let search_params = url
    .members
    .iter()
    .find(|m| m.name.as_deref() == Some("searchParams"))
    .expect("URL.searchParams present");
  assert!(
    has_ext_attr_name(&search_params.ext_attrs, "SameObject"),
    "expected [SameObject] on URL.searchParams; got {:#?}",
    search_params.ext_attrs
  );

  let href = url
    .members
    .iter()
    .find(|m| m.name.as_deref() == Some("href"))
    .expect("URL.href present");
  assert!(
    href.raw.contains("stringifier attribute"),
    "expected URL.href to be detected from `stringifier attribute ...`; raw = {}",
    href.raw
  );

  let usp_member_names: BTreeSet<String> = url_search_params
    .members
    .iter()
    .filter_map(|m| m.name.clone())
    .collect();
  for expected in [
    "constructor",
    "size",
    "append",
    "delete",
    "get",
    "getAll",
    "has",
    "set",
    "sort",
  ] {
    assert!(
      usp_member_names.contains(expected),
      "interface URLSearchParams missing member {expected}; got {:#?}",
      usp_member_names
    );
  }

  assert!(
    url_search_params
      .members
      .iter()
      .any(|m| m.raw.contains("iterable<USVString, USVString>")),
    "expected URLSearchParams to contain an iterable<USVString, USVString> member"
  );
  assert!(
    url_search_params
      .members
      .iter()
      .any(|m| m.raw.trim() == "stringifier"),
    "expected URLSearchParams to contain a `stringifier;` member"
  );
}

