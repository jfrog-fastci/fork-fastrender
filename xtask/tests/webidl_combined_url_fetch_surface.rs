use std::path::Path;

use xtask::webidl::{extract_webidl_blocks, parse_webidl, ParsedDefinition};
use xtask::webidl::resolve::resolve_webidl_world;

#[test]
fn combined_webidl_includes_url_and_fetch_surfaces() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");

  #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
  struct BalanceState {
    curly: u32,
    bracket: u32,
    paren: u32,
    in_string: Option<u8>,
    in_line_comment: bool,
    in_block_comment: bool,
    escape: bool,
  }

  impl BalanceState {
    fn is_neutral(&self) -> bool {
      self.curly == 0
        && self.bracket == 0
        && self.paren == 0
        && self.in_string.is_none()
        && !self.in_line_comment
        && !self.in_block_comment
        && !self.escape
    }
  }

  fn scan_balance(state: &mut BalanceState, input: &str) {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
      let b = bytes[i];

      if state.in_line_comment {
        if b == b'\n' {
          state.in_line_comment = false;
        }
        i += 1;
        continue;
      }
      if state.in_block_comment {
        if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
          state.in_block_comment = false;
          i += 2;
          continue;
        }
        i += 1;
        continue;
      }
      if let Some(q) = state.in_string {
        if state.escape {
          state.escape = false;
          i += 1;
          continue;
        }
        if b == b'\\' {
          state.escape = true;
          i += 1;
          continue;
        }
        if b == q {
          state.in_string = None;
        }
        i += 1;
        continue;
      }

      if b == b'/' && i + 1 < bytes.len() {
        if bytes[i + 1] == b'/' {
          state.in_line_comment = true;
          i += 2;
          continue;
        }
        if bytes[i + 1] == b'*' {
          state.in_block_comment = true;
          i += 2;
          continue;
        }
      }

      match b {
        b'"' | b'\'' => {
          state.in_string = Some(b);
          i += 1;
        }
        b'{' => {
          state.curly += 1;
          i += 1;
        }
        b'}' => {
          state.curly = state.curly.saturating_sub(1);
          i += 1;
        }
        b'[' => {
          state.bracket += 1;
          i += 1;
        }
        b']' => {
          state.bracket = state.bracket.saturating_sub(1);
          i += 1;
        }
        b'(' => {
          state.paren += 1;
          i += 1;
        }
        b')' => {
          state.paren = state.paren.saturating_sub(1);
          i += 1;
        }
        _ => i += 1,
      }
    }
  }

  fn parse_sources(repo_root: &Path, rel_paths: &[&str]) -> (String, xtask::webidl::ParsedWebIdlWorld) {
    let mut combined_idl = String::new();
    let mut state = BalanceState::default();
    for rel in rel_paths {
      let path = repo_root.join(rel);
      let src = std::fs::read_to_string(&path).expect("read spec source");
      for (idx, block) in extract_webidl_blocks(&src).into_iter().enumerate() {
        combined_idl.push_str(&block);
        // Some extracted blocks omit trailing `;`. Insert an explicit statement terminator so the
        // next block cannot be concatenated into the same top-level statement.
        combined_idl.push_str("\n;\n\n");

        scan_balance(&mut state, &block);
        scan_balance(&mut state, "\n;\n\n");
        assert!(
          state.is_neutral(),
          "extracted WebIDL block left unterminated delimiters/comments/strings (path={rel}, block_index={idx}): state={state:?}, block_len={}, contains_close_brace={}, block_prefix={:?}, block_suffix={:?}",
          block.len(),
          block.contains("};"),
          block.chars().take(160).collect::<String>(),
          block.chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect::<String>(),
        );
      }
    }
    let parsed = parse_webidl(&combined_idl).expect("parse combined WebIDL");
    (combined_idl, parsed)
  }

  let source_sets = [
    // Sanity check that DOM + URL + Fetch works even without HTML blocks.
    (
      "DOM+URL+Fetch",
      vec![
        "specs/whatwg-dom/dom.bs",
        "specs/whatwg-url/url.bs",
        "specs/whatwg-fetch/fetch.bs",
      ],
    ),
    // The codegen snapshot uses DOM + HTML + URL + Fetch.
    (
      "DOM+HTML+URL+Fetch",
      vec![
        "specs/whatwg-dom/dom.bs",
        "specs/whatwg-html/source",
        "specs/whatwg-url/url.bs",
        "specs/whatwg-fetch/fetch.bs",
      ],
    ),
  ];

  for (label, rel_paths) in source_sets {
    for rel in &rel_paths {
      let path = repo_root.join(rel);
      if !path.exists() {
        eprintln!(
          "skipping combined WebIDL URL+Fetch surface test ({label}): missing spec source at {}",
          path.display()
        );
        return;
      }
    }

    let (combined_idl, parsed) = parse_sources(repo_root, &rel_paths);
    assert!(
      combined_idl.contains("interface URL") || combined_idl.contains("interface\nURL"),
      "expected combined IDL to contain interface URL ({label})"
    );
    assert!(
      combined_idl.contains("interface Headers") || combined_idl.contains("interface\nHeaders"),
      "expected combined IDL to contain interface Headers ({label})"
    );

    let resolved = resolve_webidl_world(&parsed);

    for name in ["URL", "URLSearchParams", "Headers", "Request", "Response"] {
      if resolved.interface(name).is_none() {
        let other_with_name = parsed
          .definitions
          .iter()
          .filter_map(|def| match def {
            ParsedDefinition::Other { raw } => Some(raw.as_str()),
            _ => None,
          })
          .filter(|raw| raw.contains(name))
          .take(3)
          .collect::<Vec<_>>();
        panic!(
          "resolved world missing interface {name} ({label}); first `Other` statements containing {name}: {other_with_name:#?}",
        );
      }
    }
  }
}
