use std::fs;
use std::path::PathBuf;

use walkdir::WalkDir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn is_marked_wrong(line: &str) -> bool {
  // Keep this intentionally strict: we only exempt raw `cargo ...` examples when the surrounding
  // documentation explicitly labels them as WRONG/FORBIDDEN (to avoid masking accidental regressions).
  line.contains("WRONG") || line.contains("FORBIDDEN")
}

#[derive(Debug)]
struct FenceState {
  ch: char,
  len: usize,
  marked: bool,
  cargo_lines: Vec<(usize, String)>,
}

fn fence_delimiter(line: &str) -> Option<(char, usize)> {
  let trimmed = line.trim_start();
  let mut chars = trimmed.chars();
  let ch = chars.next()?;
  if ch != '`' && ch != '~' {
    return None;
  }
  let mut len = 1;
  for c in chars {
    if c == ch {
      len += 1;
    } else {
      break;
    }
  }
  if len >= 3 {
    Some((ch, len))
  } else {
    None
  }
}

fn is_fence_end(line: &str, ch: char, len: usize) -> bool {
  let trimmed = line.trim_start();
  let mut count = 0usize;
  for c in trimmed.chars() {
    if c == ch {
      count += 1;
    } else {
      break;
    }
  }
  count >= len
}

#[test]
fn docs_do_not_use_raw_cargo_in_code_fences() {
  let repo_root = repo_root();
  let scan_roots = [repo_root.join("docs"), repo_root.join("instructions")];

  let mut violations = Vec::<String>::new();

  for scan_root in scan_roots {
    if !scan_root.exists() {
      continue;
    }

    for entry in WalkDir::new(&scan_root) {
      let entry = entry.expect("walkdir entry");
      if !entry.file_type().is_file() {
        continue;
      }
      if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
        continue;
      }

      let contents = fs::read_to_string(entry.path())
        .unwrap_or_else(|e| panic!("read {}: {e}", entry.path().display()));

      let mut fence: Option<FenceState> = None;
      let mut prev_nonempty_outside = String::new();

      for (idx, line) in contents.lines().enumerate() {
        let line_no = idx + 1;

        match fence.as_mut() {
          None => {
            if let Some((ch, len)) = fence_delimiter(line) {
              let marked = is_marked_wrong(&prev_nonempty_outside);
              fence = Some(FenceState {
                ch,
                len,
                marked,
                cargo_lines: Vec::new(),
              });
              continue;
            }
            if !line.trim().is_empty() {
              prev_nonempty_outside = line.to_string();
            }
          }
          Some(state) => {
            if is_fence_end(line, state.ch, state.len) {
              if !state.marked && !state.cargo_lines.is_empty() {
                let file = entry
                  .path()
                  .strip_prefix(&repo_root)
                  .unwrap_or(entry.path())
                  .display()
                  .to_string();
                for (line_no, cargo_line) in state.cargo_lines.drain(..) {
                  violations.push(format!("{file}:{line_no}: {cargo_line}"));
                }
              }
              fence = None;
              continue;
            }

            if is_marked_wrong(line) {
              state.marked = true;
            }

            let trimmed = line.trim_start();
            if trimmed.starts_with("cargo ")
              || trimmed.starts_with("cargo\t")
              || (trimmed == "cargo" || trimmed.starts_with("cargo\r"))
            {
              state
                .cargo_lines
                .push((line_no, line.trim_end().to_string()));
            }
          }
        }
      }

      // Handle unterminated fences (treat EOF as fence close).
      if let Some(mut state) = fence {
        if !state.marked && !state.cargo_lines.is_empty() {
          let file = entry
            .path()
            .strip_prefix(&repo_root)
            .unwrap_or(entry.path())
            .display()
            .to_string();
          for (line_no, cargo_line) in state.cargo_lines.drain(..) {
            violations.push(format!("{file}:{line_no}: {cargo_line}"));
          }
        }
      }
    }
  }

  if !violations.is_empty() {
    panic!(
      "found raw `cargo ...` invocations in markdown code fences under docs/ or instructions/.\n\
Use wrapper-safe commands:\n\
  - cargo: `bash scripts/cargo_agent.sh ...`\n\
  - renderer runs: `bash scripts/run_limited.sh --as 64G -- ...`\n\
\n\
If you need to show an intentionally-wrong example, mark the code fence with a line containing `WRONG` or `FORBIDDEN`.\n\
\n\
Violations:\n{}",
      violations.join("\n")
    );
  }
}
