use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use rustc_hash::FxHasher;

/// Environment variable to override the browser download directory.
pub const ENV_BROWSER_DOWNLOAD_DIR: &str = "FASTR_BROWSER_DOWNLOAD_DIR";
/// Legacy alias for [`ENV_BROWSER_DOWNLOAD_DIR`] (kept for older test setups).
pub const ENV_LEGACY_DOWNLOAD_DIR: &str = "FASTR_DOWNLOAD_DIR";

fn download_dir_from_env_value(raw: &OsStr) -> Option<PathBuf> {
  if raw.is_empty() {
    return None;
  }

  // Treat whitespace-only values as unset.
  //
  // Only trim when the env value is valid UTF-8. For non-UTF-8 values (possible on Unix), treat the
  // raw bytes as a path and skip the whitespace-only check.
  match raw.to_str() {
    Some(raw) => {
      let trimmed = raw.trim();
      if trimmed.is_empty() {
        None
      } else {
        Some(PathBuf::from(trimmed))
      }
    }
    None => Some(PathBuf::from(raw)),
  }
}

/// Resolve the base download directory given a set of optional overrides.
///
/// Precedence (highest → lowest):
/// 1. CLI override (`browser --download-dir <path>`)
/// 2. `FASTR_BROWSER_DOWNLOAD_DIR`
/// 3. legacy alias `FASTR_DOWNLOAD_DIR`
/// 4. OS downloads directory (e.g. via `directories::UserDirs`)
/// 5. The current working directory (`std::env::current_dir()`, falling back to `.` on error)
///
/// Note: for testability, the core selection logic lives in
/// `resolve_download_directory_with_fallback`, which allows callers to provide an explicit cwd
/// fallback.
pub fn resolve_download_directory(
  cli_override: Option<&Path>,
  env_browser_download_dir: Option<&OsStr>,
  env_legacy_download_dir: Option<&OsStr>,
  os_downloads_dir: Option<&Path>,
) -> PathBuf {
  let cwd_fallback = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
  resolve_download_directory_with_fallback(
    cli_override,
    env_browser_download_dir,
    env_legacy_download_dir,
    os_downloads_dir,
    &cwd_fallback,
  )
}

fn resolve_download_directory_with_fallback(
  cli_override: Option<&Path>,
  env_browser_download_dir: Option<&OsStr>,
  env_legacy_download_dir: Option<&OsStr>,
  os_downloads_dir: Option<&Path>,
  cwd_fallback: &Path,
) -> PathBuf {
  if let Some(path) = cli_override.filter(|p| !p.as_os_str().is_empty()) {
    return path.to_path_buf();
  }

  if let Some(path) = env_browser_download_dir.and_then(download_dir_from_env_value) {
    return path;
  }

  if let Some(path) = env_legacy_download_dir.and_then(download_dir_from_env_value) {
    return path;
  }

  if let Some(path) = os_downloads_dir.filter(|p| !p.as_os_str().is_empty()) {
    return path.to_path_buf();
  }

  cwd_fallback.to_path_buf()
}

/// Return the `.part` path used while writing a download.
///
/// The worker writes to a sibling `*.part` file and renames to the final path on success to avoid
/// leaving partially-written final filenames behind.
pub fn part_path_for_final(final_path: &Path) -> PathBuf {
  let Some(file_name) = final_path.file_name() else {
    return final_path.with_extension("part");
  };
  let mut part_name = file_name.to_os_string();
  part_name.push(".part");
  final_path.with_file_name(part_name)
}

/// Best-effort cross-platform download filename sanitization.
///
/// Semantics (Chrome-like):
/// - Strip path separators (`/` and `\`) and control characters.
/// - Replace Windows-illegal characters (`<>:"|?*`) with `_`.
/// - Trim trailing dots/spaces (Windows compatibility).
/// - Avoid reserved Windows device names by prefixing `_`.
/// - If the result would be empty, fall back to `"download"`.
pub fn sanitize_download_filename(raw: &str) -> String {
  let mut out = String::with_capacity(raw.len());

  for c in raw.chars() {
    if c == '/' || c == '\\' || c.is_control() {
      continue;
    }

    // Keep filenames broadly compatible with Windows.
    if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
      out.push('_');
    } else {
      out.push(c);
    }
  }

  // Windows forbids filenames ending in a dot or space.
  while out.ends_with('.') || out.ends_with(' ') {
    out.pop();
  }

  if out.is_empty() {
    return "download".to_string();
  }

  // Windows device names are reserved even when followed by an extension (e.g. `CON.txt`).
  // Use the name before the *first* dot for the check, matching Windows' behavior.
  let base = out.split_once('.').map(|(b, _)| b).unwrap_or(&out);
  if is_windows_reserved_device_name(base) {
    out.insert(0, '_');
  }

  if out.is_empty() {
    "download".to_string()
  } else {
    out
  }
}

fn is_windows_reserved_device_name(name: &str) -> bool {
  let upper = name.to_ascii_uppercase();
  if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
    return true;
  }

  // COM1..COM9, LPT1..LPT9
  for prefix in ["COM", "LPT"] {
    if let Some(num) = upper.strip_prefix(prefix) {
      if let Ok(n) = num.parse::<u8>() {
        return (1..=9).contains(&n);
      }
    }
  }

  false
}

fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
  let Some(dot_idx) = name.rfind('.') else {
    return (name, None);
  };
  if dot_idx == 0 || dot_idx + 1 >= name.len() {
    return (name, None);
  }
  (&name[..dot_idx], Some(&name[dot_idx + 1..]))
}

const MAX_NUMERIC_SUFFIX: u32 = 10_000;
const MAX_HASH_SUFFIX_ATTEMPTS: u32 = 1_024;

fn candidate_available(download_dir: &Path, candidate: &str) -> Option<PathBuf> {
  let final_path = download_dir.join(candidate);
  let part_path = part_path_for_final(&final_path);

  if !final_path.exists() && !part_path.exists() {
    Some(final_path)
  } else {
    None
  }
}

fn hash_suffix_token(stem: &str, ext: Option<&str>, attempt: u32) -> String {
  let mut hasher = FxHasher::default();
  stem.hash(&mut hasher);
  ext.hash(&mut hasher);
  attempt.hash(&mut hasher);
  format!("{:016x}", hasher.finish())
}

/// Choose a deterministic non-colliding path in `download_dir` for `requested_name`.
///
/// If `foo.ext` exists, we try `foo (1).ext`, `foo (2).ext`, ... (Chrome-like). The suffix is added
/// before the last extension; for extensionless names it is appended at the end.
pub fn choose_unique_download_path(download_dir: &Path, requested_name: &str) -> PathBuf {
  choose_unique_download_path_with_max_suffix(download_dir, requested_name, MAX_NUMERIC_SUFFIX)
}

fn choose_unique_download_path_with_max_suffix(
  download_dir: &Path,
  requested_name: &str,
  max_numeric_suffix: u32,
) -> PathBuf {
  let sanitized = sanitize_download_filename(requested_name);
  let (stem, ext) = split_stem_ext(&sanitized);
  let stem = if stem.is_empty() { "download" } else { stem };

  let mut idx = 0u32;
  loop {
    let candidate = if idx == 0 {
      sanitized.clone()
    } else if let Some(ext) = ext {
      format!("{stem} ({idx}).{ext}")
    } else {
      format!("{stem} ({idx})")
    };

    if let Some(path) = candidate_available(download_dir, &candidate) {
      return path;
    }

    if idx >= max_numeric_suffix {
      break;
    }

    // `idx` is bounded by `max_numeric_suffix`, which is a `u32` passed in by the caller. Use a
    // checked add anyway to avoid debug-build overflow panics if the limit is ever changed.
    idx = match idx.checked_add(1) {
      Some(next) => next,
      None => break,
    };
  }

  // If all numeric suffixes were exhausted (or we hit the configured cap), fall back to a hashed
  // suffix. This avoids a panic from iterator overflow (`0u32..`) and guarantees termination.
  for attempt in 0..MAX_HASH_SUFFIX_ATTEMPTS {
    let token = hash_suffix_token(stem, ext, attempt);
    let candidate = if let Some(ext) = ext {
      format!("{stem} ({token}).{ext}")
    } else {
      format!("{stem} ({token})")
    };
    if let Some(path) = candidate_available(download_dir, &candidate) {
      return path;
    }
  }

  // As a last resort, return the final attempt's candidate even if it collides; the caller should
  // still use create-new semantics when opening the file.
  let token = hash_suffix_token(stem, ext, MAX_HASH_SUFFIX_ATTEMPTS);
  let candidate = if let Some(ext) = ext {
    format!("{stem} ({token}).{ext}")
  } else {
    format!("{stem} ({token})")
  };
  download_dir.join(candidate)
}

/// Derive a default filename for a download URL (used when no `<a download="...">` name is given).
pub fn filename_from_url(url: &str) -> String {
  let parsed = url::Url::parse(url).ok();

  let derived = parsed.as_ref().and_then(|url| {
    if url.scheme() == "file" {
      let path = url.to_file_path().ok()?;
      return path
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    }
    url
      .path_segments()
      .and_then(|segments| segments.last())
      .filter(|seg| !seg.is_empty())
      .map(|seg| seg.to_string())
  });

  derived.unwrap_or_else(|| "download".to_string())
}

/// Resolve the base download directory for the browser UI worker.
///
/// Front-ends can override this per-worker via [`crate::ui::messages::UiToWorker::SetDownloadDirectory`].
///
/// For convenience (e.g. local debugging), this default can also be overridden process-wide via the
/// `FASTR_BROWSER_DOWNLOAD_DIR` runtime toggle (or the legacy `FASTR_DOWNLOAD_DIR` alias).
pub fn default_download_dir() -> PathBuf {
  let toggles = crate::debug::runtime::runtime_toggles();
  let browser_env = toggles.get(ENV_BROWSER_DOWNLOAD_DIR).map(OsStr::new);
  let legacy_env = toggles.get(ENV_LEGACY_DOWNLOAD_DIR).map(OsStr::new);

  let user_downloads: Option<PathBuf> = {
    #[cfg(feature = "browser_ui")]
    {
      directories::UserDirs::new()
        .and_then(|user_dirs| user_dirs.download_dir().map(Path::to_path_buf))
    }
    #[cfg(not(feature = "browser_ui"))]
    {
      None
    }
  };

  resolve_download_directory(None, browser_env, legacy_env, user_downloads.as_deref())
}

/// Returns an error toast message when `path` does not exist.
///
/// The returned string is intended to be shown verbatim in the windowed browser UI toast overlay.
pub fn missing_path_toast_message(path: &Path) -> Option<String> {
  if path.exists() {
    return None;
  }

  let filename = path
    .file_name()
    .map(|name| name.to_string_lossy().to_string())
    .unwrap_or_else(|| path.display().to_string());

  Some(format!("File not found: {filename}"))
}

/// Update the current download directory based on a user folder selection, returning the message
/// that should be sent to the worker if the directory actually changed.
///
/// Frontends should treat an empty selection as "cancelled" and leave the current directory
/// unchanged.
pub fn apply_download_directory_selection(
  current: &mut PathBuf,
  selected: PathBuf,
) -> Option<crate::ui::messages::UiToWorker> {
  use crate::ui::messages::UiToWorker;

  if selected.as_os_str().is_empty() {
    return None;
  }
  if *current == selected {
    return None;
  }

  *current = selected.clone();
  Some(UiToWorker::SetDownloadDirectory { path: selected })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::messages::UiToWorker;

  #[test]
  fn sanitize_strips_separators_and_controls() {
    assert_eq!(
      sanitize_download_filename("a/b\\c\u{0000}d"),
      "abcd".to_string()
    );
  }

  #[test]
  fn sanitize_trims_trailing_dots_and_spaces() {
    assert_eq!(sanitize_download_filename("foo. "), "foo".to_string());
  }

  #[test]
  fn sanitize_avoids_windows_device_names() {
    assert_eq!(sanitize_download_filename("CON"), "_CON".to_string());
    assert_eq!(
      sanitize_download_filename("con.txt"),
      "_con.txt".to_string()
    );
    assert_eq!(sanitize_download_filename("LPT9"), "_LPT9".to_string());
  }

  #[test]
  fn sanitize_empty_falls_back_to_download() {
    assert_eq!(sanitize_download_filename("/"), "download".to_string());
    assert_eq!(sanitize_download_filename("   "), "download".to_string());
  }

  #[test]
  fn missing_path_toast_message_formats_filename_for_missing_path() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let missing = dir.path().join("missing.txt");
    assert!(
      !missing.exists(),
      "expected test fixture path to not exist: {}",
      missing.display()
    );
    assert_eq!(
      missing_path_toast_message(&missing),
      Some("File not found: missing.txt".to_string())
    );
  }

  #[test]
  fn apply_download_directory_selection_updates_state_and_returns_message() {
    let mut current = PathBuf::from("old-downloads");
    let selected = PathBuf::from("new-downloads");

    let msg = apply_download_directory_selection(&mut current, selected.clone())
      .expect("expected download dir selection to return a worker message");
    assert_eq!(current, selected);
    assert!(
      matches!(&msg, UiToWorker::SetDownloadDirectory { path } if path == &current),
      "unexpected message: {msg:?}"
    );
  }

  #[test]
  fn apply_download_directory_selection_rejects_empty_path() {
    let mut current = PathBuf::from("downloads");
    let msg = apply_download_directory_selection(&mut current, PathBuf::new());
    assert!(msg.is_none());
    assert_eq!(current, PathBuf::from("downloads"));
  }

  #[test]
  fn apply_download_directory_selection_is_noop_when_unchanged() {
    let mut current = PathBuf::from("downloads");
    let msg = apply_download_directory_selection(&mut current, PathBuf::from("downloads"));
    assert!(msg.is_none());
    assert_eq!(current, PathBuf::from("downloads"));
  }

  #[test]
  fn resolve_download_directory_cli_wins() {
    let cli = PathBuf::from("cli");
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(
        Some(&cli),
        Some(OsStr::new("env")),
        Some(OsStr::new("legacy")),
        Some(&os),
        &cwd,
      ),
      cli
    );
  }

  #[test]
  fn resolve_download_directory_env_wins_when_no_cli() {
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(
        None,
        Some(OsStr::new("env")),
        Some(OsStr::new("legacy")),
        Some(&os),
        &cwd,
      ),
      PathBuf::from("env")
    );
  }

  #[test]
  fn resolve_download_directory_legacy_env_wins_when_browser_env_missing() {
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(
        None,
        None,
        Some(OsStr::new("legacy")),
        Some(&os),
        &cwd
      ),
      PathBuf::from("legacy")
    );
  }

  #[test]
  fn resolve_download_directory_os_downloads_wins_when_no_overrides() {
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(None, None, None, Some(&os), &cwd),
      os
    );
  }

  #[test]
  fn resolve_download_directory_falls_back_to_current_dir() {
    let expected = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    assert_eq!(resolve_download_directory(None, None, None, None), expected);
  }

  #[test]
  fn resolve_download_directory_ignores_empty_overrides() {
    let empty_cli = PathBuf::new();
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(
        Some(&empty_cli),
        Some(OsStr::new("")),
        Some(OsStr::new("")),
        Some(&os),
        &cwd,
      ),
      os
    );
  }

  #[test]
  fn resolve_download_directory_ignores_whitespace_env_values() {
    let os = PathBuf::from("os");
    let cwd = PathBuf::from("cwd");
    assert_eq!(
      resolve_download_directory_with_fallback(
        None,
        Some(OsStr::new("   ")),
        Some(OsStr::new("\t\n")),
        Some(&os),
        &cwd,
      ),
      os
    );
  }

  #[test]
  fn choose_unique_download_path_falls_back_to_hashed_suffix_when_numeric_exhausted() {
    let dir = tempfile::tempdir().expect("tempdir should create");

    // Exhaust the numeric candidates for a low suffix cap:
    // - `foo.txt` exists
    // - `foo (1).txt.part` exists (in-progress download)
    std::fs::write(dir.path().join("foo.txt"), b"existing").expect("write base file");
    let colliding = dir.path().join("foo (1).txt");
    std::fs::write(part_path_for_final(&colliding), b"partial").expect("write part file");

    let chosen = choose_unique_download_path_with_max_suffix(dir.path(), "foo.txt", 1);
    let chosen_again = choose_unique_download_path_with_max_suffix(dir.path(), "foo.txt", 1);

    assert_eq!(chosen, chosen_again, "expected deterministic fallback path");
    assert!(!chosen.exists(), "expected chosen path to not exist yet");
    assert_ne!(chosen.file_name(), Some(OsStr::new("foo.txt")));
    assert_ne!(chosen.file_name(), Some(OsStr::new("foo (1).txt")));
    assert_eq!(chosen.extension(), Some(OsStr::new("txt")));
  }
}
