use std::path::{Path, PathBuf};

/// Compute a `file://` URL string for a local filesystem path.
///
/// This helper is used by CLI binaries to synthesize a base hint for cached HTML documents (when a
/// better URL hint is unavailable). It must never panic: missing files, missing CWD, and unusual
/// platform path formats should all result in a deterministic fallback URL string.
pub fn file_url_for_path(path: &Path) -> String {
  let abs: PathBuf = match std::fs::canonicalize(path) {
    Ok(abs) => abs,
    Err(_) => {
      if path.is_absolute() {
        path.to_path_buf()
      } else {
        match std::env::current_dir() {
          Ok(dir) => dir.join(path),
          Err(_) => PathBuf::from(".").join(path),
        }
      }
    }
  };

  match url::Url::from_file_path(&abs) {
    Ok(url) => url.to_string(),
    Err(()) => manual_file_url_from_path(&abs),
  }
}

fn manual_file_url_from_path(path: &Path) -> String {
  let path_str = path.to_string_lossy();

  // `file://` + (optional leading slash) + path.
  let mut out = String::with_capacity("file://".len() + path_str.len() + 1);
  out.push_str("file://");

  // Ensure the URL path begins with a `/`.
  let needs_leading_slash = match path_str.as_bytes().first() {
    Some(b'/') | Some(b'\\') => false,
    _ => true,
  };
  if needs_leading_slash {
    out.push('/');
  }

  // Normalise Windows separators (`\`) into URL path separators (`/`).
  for ch in path_str.chars() {
    out.push(if ch == '\\' { '/' } else { ch });
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn file_url_for_path_relative_existing() {
    let cwd = std::env::current_dir().expect("cwd");
    let dir = tempfile::tempdir_in(&cwd).expect("tempdir");
    let abs_path = dir.path().join("demo.html");
    std::fs::write(&abs_path, "ok").expect("write");

    let rel_path = abs_path.strip_prefix(&cwd).expect("strip prefix");
    let got = file_url_for_path(rel_path);

    let abs = std::fs::canonicalize(rel_path).expect("canonicalize");
    let expected = url::Url::from_file_path(&abs)
      .expect("Url::from_file_path")
      .to_string();
    assert_eq!(got, expected);
  }

  #[test]
  fn file_url_for_path_absolute_existing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let abs_path = dir.path().join("demo.html");
    std::fs::write(&abs_path, "ok").expect("write");

    let got = file_url_for_path(&abs_path);
    let abs = std::fs::canonicalize(&abs_path).expect("canonicalize");
    let expected = url::Url::from_file_path(&abs)
      .expect("Url::from_file_path")
      .to_string();
    assert_eq!(got, expected);
  }

  #[test]
  fn file_url_for_path_relative_missing() {
    let cwd = std::env::current_dir().expect("cwd");
    let dir = tempfile::tempdir_in(&cwd).expect("tempdir");
    let abs_path = dir.path().join("missing.html");
    let rel_path = abs_path.strip_prefix(&cwd).expect("strip prefix");

    let got = file_url_for_path(rel_path);
    let expected = url::Url::from_file_path(&cwd.join(rel_path))
      .expect("Url::from_file_path")
      .to_string();
    assert_eq!(got, expected);
  }

  #[test]
  fn manual_file_url_replaces_backslashes() {
    let url = manual_file_url_from_path(Path::new(r"C:\tmp\demo.html"));
    assert!(
      !url.contains('\\'),
      "expected no backslashes in file URL: {url}"
    );
    assert!(url.starts_with("file://"), "unexpected prefix: {url}");
  }
}
