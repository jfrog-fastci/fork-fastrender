use std::path::{Path, PathBuf};

use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::resolve::{ExposureTarget, ResolvedWebIdlWorld};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap()
}

fn load_world_or_skip() -> Option<ResolvedWebIdlWorld> {
  let repo_root = repo_root();
  let mut sources = vec![
    WebIdlSource {
      rel_path: "specs/whatwg-dom/dom.bs",
      label: "WHATWG DOM",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-html/source",
      label: "WHATWG HTML",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-url/url.bs",
      label: "WHATWG URL",
    },
  ];

  // Fetch is optional in this repository checkout.
  if repo_root.join("specs/whatwg-fetch/fetch.bs").exists() {
    sources.push(WebIdlSource {
      rel_path: "specs/whatwg-fetch/fetch.bs",
      label: "WHATWG Fetch",
    });
  }

  let combined = load_combined_webidl(&repo_root, &sources).ok()?;
  if !combined.missing_sources.is_empty() {
    return None;
  }

  // Ensure we didn't accidentally stop extracting blocks early (a regression we previously hit when
  // parsing WHATWG HTML sources).
  assert!(
    combined.combined_idl.contains("interface URL"),
    "combined WebIDL should contain the WHATWG URL interface"
  );

  let parsed = xtask::webidl::parse_webidl(&combined.combined_idl).ok()?;
  let resolved = xtask::webidl::resolve::resolve_webidl_world(&parsed);
  Some(resolved)
}

#[test]
fn url_spec_alone_parses_url_interface() {
  let repo_root = repo_root();
  let sources = [WebIdlSource {
    rel_path: "specs/whatwg-url/url.bs",
    label: "WHATWG URL",
  }];

  let combined = load_combined_webidl(&repo_root, &sources).expect("load combined webidl");
  if !combined.missing_sources.is_empty() {
    eprintln!("skipping: spec sources missing (did you init submodules?)");
    return;
  }

  assert!(
    combined.combined_idl.contains("interface URL"),
    "combined WebIDL should contain the WHATWG URL interface"
  );

  let parsed = xtask::webidl::parse_webidl(&combined.combined_idl).expect("parse webidl");
  let resolved = xtask::webidl::resolve::resolve_webidl_world(&parsed);
  assert!(
    resolved.interfaces.contains_key("URL"),
    "URL spec alone should yield interface URL"
  );
}

#[test]
fn dom_and_url_specs_parse_url_interface() {
  let repo_root = repo_root();
  let sources = [
    WebIdlSource {
      rel_path: "specs/whatwg-dom/dom.bs",
      label: "WHATWG DOM",
    },
    WebIdlSource {
      rel_path: "specs/whatwg-url/url.bs",
      label: "WHATWG URL",
    },
  ];

  let combined = load_combined_webidl(&repo_root, &sources).expect("load combined webidl");
  if !combined.missing_sources.is_empty() {
    eprintln!("skipping: spec sources missing (did you init submodules?)");
    return;
  }
  assert!(combined.combined_idl.contains("interface URL"));

  let parsed = xtask::webidl::parse_webidl(&combined.combined_idl).expect("parse webidl");
  let resolved = xtask::webidl::resolve::resolve_webidl_world(&parsed);
  assert!(
    resolved.interfaces.contains_key("URL"),
    "DOM+URL specs should include interface URL"
  );
}

#[test]
fn resolved_world_includes_url_interface() {
  let Some(world) = load_world_or_skip() else {
    eprintln!("skipping: spec sources missing (did you init submodules?)");
    return;
  };

  let Some(url) = world.interfaces.get("URL") else {
    panic!("resolved WebIDL world should include interface URL");
  };

  assert!(
    url.exposure.matches(ExposureTarget::Window),
    "URL should be exposed to Window (got {:?})",
    url.exposure
  );
}

#[test]
fn resolved_window_includes_timer_ops_from_window_or_worker_global_scope() {
  let Some(world) = load_world_or_skip() else {
    eprintln!("skipping: spec sources missing (did you init submodules?)");
    return;
  };

  let world = world.filter_by_exposure(ExposureTarget::Window);

  let window = world
    .interfaces
    .get("Window")
    .expect("resolved world should include Window interface");

  let mut names: Vec<&str> = window
    .members
    .iter()
    .filter_map(|m| m.name.as_deref())
    .collect();
  names.sort_unstable();
  names.dedup();

  for expected in [
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "queueMicrotask",
  ] {
    assert!(
      names.contains(&expected),
      "Window members should include `{expected}` (from WindowOrWorkerGlobalScope includes)"
    );
  }
}
