use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

#[cfg(unix)]
fn make_executable(path: &Path) {
  use std::os::unix::fs::PermissionsExt;
  let mut perms = fs::metadata(path)
    .expect("stat stub executable")
    .permissions();
  perms.set_mode(0o755);
  fs::set_permissions(path, perms).expect("chmod stub executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn entry_anchor_id(name: &str) -> String {
  let mut hash: u64 = 14695981039346656037;
  for byte in name.as_bytes() {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(1099511628211);
  }
  format!("entry-{hash:016x}")
}

fn prepend_path(bin_dir: &Path) -> std::ffi::OsString {
  let path_var = std::env::var_os("PATH").unwrap_or_default();
  let mut paths = vec![bin_dir.to_path_buf()];
  paths.extend(std::env::split_paths(&path_var));
  std::env::join_paths(paths).expect("join PATH")
}

fn write_stub_cargo(bin_dir: &Path) -> PathBuf {
  let stub_cargo = bin_dir.join("cargo");
  fs::write(
    &stub_cargo,
    "#!/usr/bin/env sh\necho 'cargo should not be called' >&2\nexit 2\n",
  )
  .expect("write stub cargo");
  make_executable(&stub_cargo);
  stub_cargo
}

fn write_stub_cargo_asserts_avif(bin_dir: &Path) -> PathBuf {
  let stub_cargo = bin_dir.join("cargo");
  fs::write(
    &stub_cargo,
    r#"#!/usr/bin/env sh
set -eu

subcommand="${1:-}"
if [ "$subcommand" != "build" ]; then
  echo "stub cargo: unexpected subcommand '$subcommand'" >&2
  exit 2
fi

features=""
prev=""
for arg in "$@"; do
  case "$arg" in
    --features=*) features="${arg#--features=}" ;;
  esac
  if [ "$prev" = "--features" ]; then
    features="$arg"
  fi
  prev="$arg"
done

case ",${features}," in
  *,avif,*) ;;
  *)
    echo "stub cargo: expected build to include --features avif, got '$features'" >&2
    exit 2
    ;;
esac

exit 0
"#,
  )
  .expect("write stub cargo");
  make_executable(&stub_cargo);
  stub_cargo
}

const STUB_RENDER_FIXTURES: &str = r#"#!/usr/bin/env sh
set -eu

out=""
fixtures="hello"
write_snapshot=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out-dir) out="$2"; shift 2;;
    --fixtures) fixtures="$2"; shift 2;;
    --write-snapshot) write_snapshot=1; shift 1;;
    *) shift;;
  esac
done

mkdir -p "$out"
stem="$(printf "%s" "$fixtures" | cut -d',' -f1)"

# 1x1 transparent PNG (base64).
png_b64='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/w8AAgMBgVnZSwAAAABJRU5ErkJggg=='
if printf '' | base64 -d >/dev/null 2>&1; then
  printf "%s" "$png_b64" | base64 -d > "$out/$stem.png"
else
  # macOS/BSD base64 uses -D
  printf "%s" "$png_b64" | base64 -D > "$out/$stem.png"
fi

if [ "$write_snapshot" -eq 1 ]; then
  mkdir -p "$out/$stem"
  echo "{}" > "$out/$stem/snapshot.json"
  echo "{}" > "$out/$stem/diagnostics.json"
fi
exit 0
"#;

const STUB_DIFF_RENDERS_DIFF: &str = r#"#!/usr/bin/env sh
set -eu

html=""
json=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --html) html="$2"; shift 2;;
    --json) json="$2"; shift 2;;
    *) shift;;
  esac
done

mkdir -p "$(dirname "$html")"
mkdir -p "$(dirname "$json")"
echo "<!doctype html><title>stub diff</title>" > "$html"
cat > "$json" <<'JSON'
{
  "totals": {
    "discovered": 1,
    "processed": 1,
    "matches": 0,
    "within_threshold": 0,
    "differences": 1,
    "missing": 0,
    "errors": 0,
    "shard_skipped": 0
  },
  "results": [
    {
      "name": "hello",
      "status": "diff",
      "before": "../run1/hello.png",
      "after": "../run2/hello.png",
      "diff": "diff.png",
      "metrics": {
        "pixel_diff": 1,
        "total_pixels": 1,
        "diff_percentage": 100.0,
        "perceptual_distance": 1.0
      },
      "error": null
    }
  ]
}
JSON
echo "PNG" > "$(dirname "$html")/diff.png"
exit 1
"#;

const STUB_DIFF_RENDERS_MATCH: &str = r#"#!/usr/bin/env sh
set -eu

html=""
json=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --html) html="$2"; shift 2;;
    --json) json="$2"; shift 2;;
    *) shift;;
  esac
done

mkdir -p "$(dirname "$html")"
mkdir -p "$(dirname "$json")"
echo "<!doctype html><title>stub diff</title>" > "$html"
cat > "$json" <<'JSON'
{
  "totals": {
    "discovered": 1,
    "processed": 1,
    "matches": 1,
    "within_threshold": 0,
    "differences": 0,
    "missing": 0,
    "errors": 0,
    "shard_skipped": 0
  },
  "results": [
    {
      "name": "hello",
      "status": "match",
      "before": null,
      "after": null,
      "diff": null,
      "metrics": {
        "pixel_diff": 0,
        "total_pixels": 1,
        "diff_percentage": 0.0,
        "perceptual_distance": 0.0
      },
      "error": null
    }
  ]
}
JSON
exit 0
"#;

const STUB_DIFF_SNAPSHOTS: &str = r#"#!/usr/bin/env sh
set -eu

html=""
json=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --html) html="$2"; shift 2;;
    --json) json="$2"; shift 2;;
    *) shift;;
  esac
done

mkdir -p "$(dirname "$html")"
mkdir -p "$(dirname "$json")"
echo "<!doctype html><title>stub diff_snapshots</title>" > "$html"
echo "{}" > "$json"
exit 0
"#;

fn write_stub_executable(target_dir: &Path, profile: &str, name: &str, script: &str) -> PathBuf {
  let bin = target_dir.join(profile).join(name);
  fs::create_dir_all(bin.parent().expect("stub executable parent"))
    .expect("create stub target dir");
  fs::write(&bin, script).expect("write stub executable");
  make_executable(&bin);
  bin
}

fn write_stub_render_fixtures(target_dir: &Path) -> PathBuf {
  write_stub_executable(target_dir, "release", "render_fixtures", STUB_RENDER_FIXTURES)
}

fn write_stub_render_fixtures_debug(target_dir: &Path) -> PathBuf {
  write_stub_executable(target_dir, "debug", "render_fixtures", STUB_RENDER_FIXTURES)
}

fn write_stub_diff_renders(target_dir: &Path, differences: bool) -> PathBuf {
  let script = if differences {
    STUB_DIFF_RENDERS_DIFF
  } else {
    STUB_DIFF_RENDERS_MATCH
  };
  write_stub_executable(target_dir, "release", "diff_renders", script)
}

fn write_stub_diff_renders_debug(target_dir: &Path, differences: bool) -> PathBuf {
  let script = if differences {
    STUB_DIFF_RENDERS_DIFF
  } else {
    STUB_DIFF_RENDERS_MATCH
  };
  write_stub_executable(target_dir, "debug", "diff_renders", script)
}

fn write_stub_diff_snapshots(target_dir: &Path) -> PathBuf {
  write_stub_executable(target_dir, "release", "diff_snapshots", STUB_DIFF_SNAPSHOTS)
}

fn write_stub_diff_snapshots_debug(target_dir: &Path) -> PathBuf {
  write_stub_executable(target_dir, "debug", "diff_snapshots", STUB_DIFF_SNAPSHOTS)
}

fn run_fixture_determinism(
  bin_dir: &Path,
  target_dir: &Path,
  fixtures_dir: &Path,
  out_dir: &Path,
  allow_differences: bool,
  debug: bool,
  no_build: bool,
) -> std::process::Output {
  let mut cmd = Command::new(env!("CARGO_BIN_EXE_xtask"));
  cmd.current_dir(repo_root());
  cmd.env("PATH", prepend_path(bin_dir));
  cmd.env("CARGO_TARGET_DIR", target_dir);
  cmd.arg("fixture-determinism");
  if no_build {
    cmd.arg("--no-build");
  }
  if debug {
    cmd.arg("--debug");
  }
  if allow_differences {
    cmd.arg("--allow-differences");
  }
  cmd.arg("--fixtures-dir").arg(fixtures_dir);
  cmd.arg("--out-dir").arg(out_dir);
  cmd.args(["--fixtures", "hello"]);
  cmd.args(["--repeat", "2"]);
  cmd.output().expect("run xtask fixture-determinism")
}

#[test]
#[cfg(unix)]
fn fixture_determinism_no_build_writes_report() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  let target_dir = temp.path().join("target");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");
  fs::create_dir_all(target_dir.join("release")).expect("create stub target dir");

  // Place a stub `cargo` in PATH that fails if invoked. When `--no-build` is working correctly,
  // xtask should never spawn `cargo build`.
  write_stub_cargo(&bin_dir);
  write_stub_render_fixtures(&target_dir);
  write_stub_diff_renders(&target_dir, false);
  write_stub_diff_snapshots(&target_dir);

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  let out_dir = temp.path().join("out");

  let output = run_fixture_determinism(
    &bin_dir,
    &target_dir,
    &fixtures_dir,
    &out_dir,
    false,
    false,
    true,
  );
  assert!(
    output.status.success(),
    "expected fixture-determinism to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    out_dir.join("report.html").is_file(),
    "missing report.html in out dir"
  );
  assert!(
    out_dir.join("report.json").is_file(),
    "missing report.json in out dir"
  );

  let html = fs::read_to_string(out_dir.join("report.html")).expect("read report.html");
  assert!(
    html.contains("id=\"nondet-controls\""),
    "expected filter controls in report HTML:\n{html}"
  );
  assert!(
    html.contains("id=\"show-diff\""),
    "expected status toggles in report HTML:\n{html}"
  );
}

#[test]
#[cfg(unix)]
fn fixture_determinism_fails_when_differences_found() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  let target_dir = temp.path().join("target");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");
  fs::create_dir_all(target_dir.join("release")).expect("create stub target dir");

  write_stub_cargo(&bin_dir);
  write_stub_render_fixtures(&target_dir);
  write_stub_diff_renders(&target_dir, true);
  write_stub_diff_snapshots(&target_dir);

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  let out_dir = temp.path().join("out");

  let output = run_fixture_determinism(
    &bin_dir,
    &target_dir,
    &fixtures_dir,
    &out_dir,
    false,
    false,
    true,
  );
  assert!(
    !output.status.success(),
    "expected fixture-determinism to fail when differences are found.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    out_dir.join("report.html").is_file(),
    "missing report.html in out dir"
  );
  assert!(
    out_dir.join("report.json").is_file(),
    "missing report.json in out dir"
  );

  let html = fs::read_to_string(out_dir.join("report.html")).expect("read report.html");
  assert!(
    html.contains("id=\"nondet-controls\""),
    "expected filter controls in report HTML:\n{html}"
  );
  assert!(
    html.contains("Diff (1)"),
    "expected diff count in report HTML:\n{html}"
  );
  let anchor = entry_anchor_id("hello");
  assert!(
    html.contains(&format!("id=\"{anchor}\" class=\"diff\"")),
    "expected per-entry anchor id in report HTML:\n{html}"
  );
  assert!(
    html.contains(&format!("href=\"#{anchor}\">hello</a>")),
    "expected fixture name self-link in report HTML:\n{html}"
  );
  assert!(
    html.contains(r#"<a href="run1/hello.png"><img src="run1/hello.png""#),
    "expected clickable thumbnail image in report HTML:\n{html}"
  );
  assert!(
    html.contains(&format!(
      "href=\"diff_run1_run2/report.html#{anchor}\">pair report</a>"
    )),
    "expected pair report link to deep-link to entry anchor:\n{html}"
  );
}

#[test]
#[cfg(unix)]
fn fixture_determinism_allow_differences_exits_zero() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  let target_dir = temp.path().join("target");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");
  fs::create_dir_all(target_dir.join("release")).expect("create stub target dir");

  write_stub_cargo(&bin_dir);
  write_stub_render_fixtures(&target_dir);
  write_stub_diff_renders(&target_dir, true);
  write_stub_diff_snapshots(&target_dir);

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  let out_dir = temp.path().join("out");

  let output = run_fixture_determinism(
    &bin_dir,
    &target_dir,
    &fixtures_dir,
    &out_dir,
    true,
    false,
    true,
  );
  assert!(
    output.status.success(),
    "expected fixture-determinism to exit 0 when --allow-differences is set.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    out_dir.join("report.html").is_file(),
    "missing report.html in out dir"
  );
  assert!(
    out_dir.join("report.json").is_file(),
    "missing report.json in out dir"
  );

  let html = fs::read_to_string(out_dir.join("report.html")).expect("read report.html");
  assert!(
    html.contains("Diff (1)"),
    "expected diff count in report HTML:\n{html}"
  );
}

#[test]
#[cfg(unix)]
fn fixture_determinism_no_build_debug_profile_writes_report() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  let target_dir = temp.path().join("target");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");

  // Place a stub `cargo` in PATH that fails if invoked. When `--no-build` is working correctly,
  // xtask should never spawn `cargo build`.
  write_stub_cargo(&bin_dir);
  write_stub_render_fixtures_debug(&target_dir);
  write_stub_diff_renders_debug(&target_dir, false);
  write_stub_diff_snapshots_debug(&target_dir);

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  let out_dir = temp.path().join("out");

  let output = run_fixture_determinism(
    &bin_dir,
    &target_dir,
    &fixtures_dir,
    &out_dir,
    false,
    true,
    true,
  );
  assert!(
    output.status.success(),
    "expected fixture-determinism --debug to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    out_dir.join("report.html").is_file(),
    "missing report.html in out dir"
  );
  assert!(
    out_dir.join("report.json").is_file(),
    "missing report.json in out dir"
  );
}

#[test]
#[cfg(unix)]
fn fixture_determinism_build_includes_avif_feature() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  let target_dir = temp.path().join("target");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");
  fs::create_dir_all(target_dir.join("release")).expect("create stub target dir");

  write_stub_cargo_asserts_avif(&bin_dir);
  write_stub_render_fixtures(&target_dir);
  write_stub_diff_renders(&target_dir, false);
  write_stub_diff_snapshots(&target_dir);

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  let out_dir = temp.path().join("out");

  let output = run_fixture_determinism(
    &bin_dir,
    &target_dir,
    &fixtures_dir,
    &out_dir,
    false,
    false,
    false,
  );
  assert!(
    output.status.success(),
    "expected fixture-determinism to succeed when building with AVIF support.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    out_dir.join("report.html").is_file(),
    "missing report.html in out dir"
  );
}
