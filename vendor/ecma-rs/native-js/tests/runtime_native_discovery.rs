use native_js::link::{find_runtime_native_staticlib_in, RuntimeNativeStaticlibDiscoveryContext};
use std::fs;
use std::path::PathBuf;

fn touch(path: &std::path::Path) {
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(path, b"").unwrap();
}

#[test]
fn discovery_env_override_is_returned_verbatim() {
  let expected = PathBuf::from("/tmp/does-not-need-to-exist/libruntime_native.a");
  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: Some(expected.clone()),
    current_exe: None,
    cargo_target_dir: None,
    cargo_manifest_dir: PathBuf::from("/tmp/unused"),
  })
  .unwrap();
  assert_eq!(found, expected);
}

#[test]
fn discovery_current_exe_adjacent_wins() {
  let td = tempfile::tempdir().unwrap();
  let exe_dir = td.path().join("bin");
  let exe = exe_dir.join("myexe");
  let adjacent = exe_dir.join("libruntime_native.a");
  touch(&adjacent);

  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: None,
    current_exe: Some(exe),
    cargo_target_dir: Some(td.path().join("target")), // should lose to current_exe lookup
    cargo_manifest_dir: td.path().join("crate"),
  })
  .unwrap();

  assert_eq!(found, adjacent);
}

#[test]
fn discovery_current_exe_deps_is_used() {
  let td = tempfile::tempdir().unwrap();
  let exe_dir = td.path().join("bin");
  let exe = exe_dir.join("myexe");
  let deps = exe_dir.join("deps").join("libruntime_native.a");
  touch(&deps);

  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: None,
    current_exe: Some(exe),
    cargo_target_dir: Some(td.path().join("target")), // should lose to current_exe lookup
    cargo_manifest_dir: td.path().join("crate"),
  })
  .unwrap();

  assert_eq!(found, deps);
}

#[test]
fn discovery_prefers_cargo_target_dir_over_workspace_target() {
  let td = tempfile::tempdir().unwrap();
  let ws_root = td.path().join("ws");
  fs::create_dir_all(&ws_root).unwrap();
  fs::write(ws_root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

  let crate_dir = ws_root.join("native-js");
  fs::create_dir_all(&crate_dir).unwrap();

  let cargo_target_dir = td.path().join("custom-target");
  let expected = cargo_target_dir
    .join("debug")
    .join("deps")
    .join("libruntime_native.a");
  touch(&expected);

  let ws_target = ws_root
    .join("target")
    .join("debug")
    .join("deps")
    .join("libruntime_native.a");
  touch(&ws_target);

  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: None,
    current_exe: None,
    cargo_target_dir: Some(cargo_target_dir),
    cargo_manifest_dir: crate_dir,
  })
  .unwrap();

  assert_eq!(found, expected);
}

#[test]
fn discovery_workspace_root_target_is_used() {
  let td = tempfile::tempdir().unwrap();
  let ws_root = td.path().join("ws");
  fs::create_dir_all(&ws_root).unwrap();
  fs::write(ws_root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

  let crate_dir = ws_root.join("native-js");
  fs::create_dir_all(&crate_dir).unwrap();
  fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"native-js\"\n").unwrap();

  let expected = ws_root
    .join("target")
    .join("debug")
    .join("deps")
    .join("libruntime_native.a");
  touch(&expected);

  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: None,
    current_exe: None,
    cargo_target_dir: None,
    cargo_manifest_dir: crate_dir,
  })
  .unwrap();

  assert_eq!(found, expected);
}

#[test]
fn discovery_crate_local_target_fallback_is_used_for_non_workspace_builds() {
  let td = tempfile::tempdir().unwrap();
  let crate_dir = td.path().join("crate");
  fs::create_dir_all(&crate_dir).unwrap();
  fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"my-crate\"\n").unwrap();

  let expected = crate_dir
    .join("target")
    .join("release")
    .join("deps")
    .join("libruntime_native.a");
  touch(&expected);

  let found = find_runtime_native_staticlib_in(RuntimeNativeStaticlibDiscoveryContext {
    env_runtime_native_a: None,
    current_exe: None,
    cargo_target_dir: None,
    cargo_manifest_dir: crate_dir,
  })
  .unwrap();

  assert_eq!(found, expected);
}

