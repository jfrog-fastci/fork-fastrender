use std::path::Path;

use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::parse_webidl;
use xtask::webidl::resolve::resolve_webidl_world;

#[test]
fn webidl_codegen_sources_include_url_and_fetch() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [
    WebIdlSource {
      rel_path: "specs/whatwg-dom/dom.bs",
      label: "DOM",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-html/source",
      label: "HTML",
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

  let loaded = load_combined_webidl(repo_root, &sources).expect("load combined WebIDL sources");
  if !loaded.missing_sources.is_empty() {
    for (label, path) in loaded.missing_sources {
      eprintln!(
        "skipping WebIDL codegen sources test: missing {label} source at {}",
        path.display()
      );
    }
    return;
  }
  let parsed = parse_webidl(&loaded.combined_idl).expect("parse extracted WebIDL");
  let resolved = resolve_webidl_world(&parsed);

  for iface in ["URL", "URLSearchParams", "Headers", "Request", "Response"] {
    assert!(
      resolved.interface(iface).is_some(),
      "expected combined WebIDL to contain interface {iface}"
    );
  }

  let has_fetch = resolved
    .interfaces
    .values()
    .flat_map(|i| i.members.iter())
    .chain(resolved.interface_mixins.values().flat_map(|m| m.members.iter()))
    .any(|m| m.name.as_deref() == Some("fetch") || m.raw.contains(" fetch(") || m.raw.starts_with("fetch("));
  assert!(has_fetch, "expected a fetch operation in the resolved world");
}
