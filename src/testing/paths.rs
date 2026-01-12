use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Path to the crate root (`CARGO_MANIFEST_DIR`).
pub(crate) fn manifest_dir() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Path to the `tests/` directory in the repository.
pub(crate) fn tests_dir() -> &'static Path {
  static TESTS_DIR: LazyLock<PathBuf> = LazyLock::new(|| manifest_dir().join("tests"));
  TESTS_DIR.as_path()
}

/// Path to the shared test fixtures under `tests/fixtures/`.
pub(crate) fn fixtures_dir() -> &'static Path {
  static FIXTURES_DIR: LazyLock<PathBuf> = LazyLock::new(|| tests_dir().join("fixtures"));
  FIXTURES_DIR.as_path()
}

/// Path to the reference-test fixtures under `tests/ref/fixtures/`.
pub(crate) fn ref_fixtures_dir() -> &'static Path {
  static REF_FIXTURES_DIR: LazyLock<PathBuf> = LazyLock::new(|| tests_dir().join("ref/fixtures"));
  REF_FIXTURES_DIR.as_path()
}

