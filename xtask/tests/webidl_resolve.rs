use std::path::Path;

use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::resolve::{resolve_webidl_world, ExposureTarget};
use xtask::webidl::{parse_webidl, BuiltinType, IdlType};

fn delimiter_depths(input: &str) -> (u32, u32, u32) {
  let bytes = input.as_bytes();
  let mut curly = 0u32;
  let mut bracket = 0u32;
  let mut paren = 0u32;
  let mut in_string: Option<u8> = None;
  let mut in_line_comment = false;
  let mut in_block_comment = false;
  let mut escape = false;

  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];

    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      i += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_block_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
    if let Some(q) = in_string {
      if escape {
        escape = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        escape = true;
        i += 1;
        continue;
      }
      if b == q {
        in_string = None;
      }
      i += 1;
      continue;
    }

    // Start of a comment.
    if b == b'/' && i + 1 < bytes.len() {
      if bytes[i + 1] == b'/' {
        in_line_comment = true;
        i += 2;
        continue;
      }
      if bytes[i + 1] == b'*' {
        in_block_comment = true;
        i += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'{' => {
        curly += 1;
        i += 1;
      }
      b'}' => {
        curly = curly.saturating_sub(1);
        i += 1;
      }
      b'[' => {
        bracket += 1;
        i += 1;
      }
      b']' => {
        bracket = bracket.saturating_sub(1);
        i += 1;
      }
      b'(' => {
        paren += 1;
        i += 1;
      }
      b')' => {
        paren = paren.saturating_sub(1);
        i += 1;
      }
      _ => i += 1,
    }
  }

  (curly, bracket, paren)
}

fn unmatched_open_curly_positions(input: &str) -> Vec<usize> {
  let bytes = input.as_bytes();
  let mut stack = Vec::<usize>::new();
  let mut in_string: Option<u8> = None;
  let mut in_line_comment = false;
  let mut in_block_comment = false;
  let mut escape = false;

  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];

    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      i += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_block_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
    if let Some(q) = in_string {
      if escape {
        escape = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        escape = true;
        i += 1;
        continue;
      }
      if b == q {
        in_string = None;
      }
      i += 1;
      continue;
    }

    if b == b'/' && i + 1 < bytes.len() {
      if bytes[i + 1] == b'/' {
        in_line_comment = true;
        i += 2;
        continue;
      }
      if bytes[i + 1] == b'*' {
        in_block_comment = true;
        i += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'{' => {
        stack.push(i);
        i += 1;
      }
      b'}' => {
        stack.pop();
        i += 1;
      }
      _ => i += 1,
    }
  }

  stack
}

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

  // Sanity check: the combined IDL should include key interfaces from URL/Fetch.
  assert!(
    loaded.combined_idl.contains("interface URL {"),
    "expected combined WebIDL input to include `interface URL`"
  );
  assert!(
    loaded.combined_idl.contains("interface Response {"),
    "expected combined WebIDL input to include `interface Response`"
  );

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    resolved.interfaces.contains_key("URL"),
    "expected `URL` interface to resolve"
  );
  assert!(
    resolved.interfaces.contains_key("Response"),
    "expected `Response` interface to resolve"
  );
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

  assert!(
    loaded
      .combined_idl
      .contains("interface mixin WindowOrWorkerGlobalScope"),
    "expected extracted IDL text to mention WindowOrWorkerGlobalScope"
  );
  assert!(
    loaded
      .combined_idl
      .contains("typedef (DOMString or Function or TrustedScript) TimerHandler"),
    "expected extracted IDL text to mention TimerHandler"
  );
  let depths = delimiter_depths(&loaded.combined_idl);
  if depths != (0, 0, 0) {
    let stack = unmatched_open_curly_positions(&loaded.combined_idl);
    if let Some(&idx) = stack.last() {
      let start = idx.saturating_sub(2000);
      let end = (idx + 2000).min(loaded.combined_idl.len());
      let snippet = loaded
        .combined_idl
        .get(start..end)
        .unwrap_or("<snippet not at UTF-8 boundaries>");
      eprintln!(
        "unbalanced extracted IDL: depths={depths:?}, last unmatched `{{` at byte {idx}\n{snippet}"
      );
    } else {
      eprintln!("unbalanced extracted IDL: depths={depths:?} (no unmatched `{{` found)");
    }
  }
  assert_eq!(
    depths,
    (0, 0, 0),
    "expected extracted IDL to have balanced delimiters (curly/bracket/paren)"
  );

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    !resolved.interfaces.is_empty(),
    "expected non-empty interface set from WHATWG HTML"
  );

  // Regression coverage: ensure later-file globals are present. These used to be missing when the
  // HTML extractor stopped scanning early (e.g. due to malformed tags / nested `<code>` handling).
  assert!(
    resolved.interface_mixins.contains_key("WindowOrWorkerGlobalScope"),
    "expected WindowOrWorkerGlobalScope interface mixin to be extracted+parsed"
  );
  assert!(
    resolved.typedefs.contains_key("TimerHandler"),
    "expected TimerHandler typedef to be extracted+parsed"
  );
  let window = resolved
    .interfaces
    .get("Window")
    .expect("expected Window interface to be extracted+parsed");
  assert_eq!(
    window.inherits.as_deref(),
    Some("EventTarget"),
    "expected Window to inherit EventTarget"
  );
}

#[test]
fn smoke_resolve_dom_html_url_fetch() {
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

  let loaded = load_combined_webidl(repo_root, &sources).unwrap();
  if !loaded.missing_sources.is_empty() {
    for (label, path) in &loaded.missing_sources {
      eprintln!("skipping WebIDL smoke test: missing {label} source at {}", path.display());
    }
    return;
  }

  assert!(
    loaded.combined_idl.contains("interface URL {"),
    "expected combined WebIDL input to include `interface URL`"
  );
  assert!(
    loaded.combined_idl.contains("interface Response {"),
    "expected combined WebIDL input to include `interface Response`"
  );

  let parsed = parse_webidl(&loaded.combined_idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  assert!(
    resolved.interfaces.contains_key("URL"),
    "expected `URL` interface to resolve"
  );
  assert!(
    resolved.interfaces.contains_key("Response"),
    "expected `Response` interface to resolve"
  );
  assert!(
    !resolved.interfaces.is_empty(),
    "expected non-empty interface set from DOM+HTML+URL+Fetch"
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

#[test]
fn preserves_callback_interface_flag() {
  let idl = r#"
    callback interface EventListener {
      undefined handleEvent(Event event);
    };

    interface Foo {
      undefined bar();
    };
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);

  assert!(
    resolved
      .interfaces
      .get("EventListener")
      .expect("EventListener resolved")
      .callback,
    "expected callback interface flag to be preserved"
  );
  assert!(
    !resolved.interfaces.get("Foo").expect("Foo resolved").callback,
    "expected non-callback interface to default to false"
  );
}
