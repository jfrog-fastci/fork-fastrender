use std::path::Path;

use xtask::webidl::{extract_webidl_blocks, parse_webidl, ParsedWebIdlWorld};
use xtask::webidl::resolve::resolve_webidl_world;

#[test]
fn combined_dom_html_url_fetch_parses_url_and_headers() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  let sources = [
    ("DOM", repo_root.join("specs/whatwg-dom/dom.bs")),
    ("HTML", repo_root.join("specs/whatwg-html/source")),
    ("URL", repo_root.join("specs/whatwg-url/url.bs")),
    ("Fetch", repo_root.join("specs/whatwg-fetch/fetch.bs")),
  ];

  for (label, path) in &sources {
    if !path.exists() {
      eprintln!(
        "skipping combined WebIDL source test: missing {label} source at {}",
        path.display()
      );
      return;
    }
  }

  let mut idl = String::new();
  let mut world = ParsedWebIdlWorld::default();
  for (_label, path) in sources {
    let text = std::fs::read_to_string(&path).expect("read spec source");
    for mut block in extract_webidl_blocks(&text) {
      idl.push_str(&block);
      idl.push_str("\n;\n\n");
      // Parse each extracted block in isolation so a malformed statement in one block does not
      // swallow subsequent spec sources.
      block.push_str("\n;\n");
      let parsed = parse_webidl(&block).unwrap();
      world.definitions.extend(parsed.definitions);
    }
  }

  assert!(
    idl.contains("interface URL {"),
    "expected combined IDL to contain interface URL"
  );
  assert!(
    idl.contains("interface Headers {"),
    "expected combined IDL to contain interface Headers"
  );

  let resolved = resolve_webidl_world(&world);
  assert!(resolved.interface("URL").is_some(), "expected interface URL");
  assert!(
    resolved.interface("URLSearchParams").is_some(),
    "expected interface URLSearchParams"
  );
  assert!(resolved.interface("Headers").is_some(), "expected interface Headers");
}
