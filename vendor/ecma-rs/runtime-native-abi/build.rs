use std::env;
use std::path::PathBuf;

fn main() {
  println!("cargo:rerun-if-changed=src/lib.rs");
  println!("cargo:rerun-if-changed=cbindgen.toml");

  let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));

  let header_path = out_dir.join("runtime_native_abi.h");

  let config = cbindgen::Config::from_root_or_default(&crate_dir);
  cbindgen::Builder::new()
    .with_crate(crate_dir.clone())
    .with_config(config)
    .generate()
    .expect("failed to generate runtime_native_abi.h")
    .write_to_file(&header_path);

  // cbindgen represents empty/opaque structs as a 0-length byte array. For the
  // runtime ABI we want a true forward declaration so external tooling never
  // relies on a (fake) layout.
  //
  // Keep this as a small post-process step so `runtime-native-abi/src/lib.rs`
  // remains the single source of truth.
  let mut header = std::fs::read_to_string(&header_path)
    .unwrap_or_else(|err| panic!("failed to read generated header {}: {err}", header_path.display()));
  let mut modified = false;
  if let Some(start) = header.find("typedef struct Coroutine {") {
    if let Some(end_rel) = header[start..].find("} Coroutine;") {
      let mut end = start + end_rel + "} Coroutine;".len();
      if header[end..].starts_with("\r\n") {
        end += 2;
      } else if header[end..].starts_with('\n') {
        end += 1;
      }
      header.replace_range(start..end, "typedef struct Coroutine Coroutine;\n");
      modified = true;
    }
  }

  // cbindgen does not currently emit foreign `extern` statics. The runtime exports
  // `RT_GC_EPOCH` as a link-visible symbol used by codegen for fast safepoint
  // polling; inject an extern declaration so the generated header is complete.
  if !header.contains("RT_GC_EPOCH") {
    if let Some(insert_at) = header.find("extern uint8_t *rt_alloc") {
      header.insert_str(
        insert_at,
        concat!(
          "// Global GC/safepoint epoch (monotonically increasing).\n",
          "//\n",
          "// Semantics:\n",
          "//   - even: no stop-the-world requested\n",
          "//   - odd:  stop-the-world requested\n",
          "//\n",
          "// Generated code should treat this as an atomic.\n",
          "#if defined(__cplusplus)\n",
          "extern uint64_t RT_GC_EPOCH;\n",
          "#elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L) && !defined(__STDC_NO_ATOMICS__)\n",
          "extern _Atomic uint64_t RT_GC_EPOCH;\n",
          "#else\n",
          "extern uint64_t RT_GC_EPOCH;\n",
          "#endif\n\n",
        ),
      );
      modified = true;
    }
  }

  if modified {
    std::fs::write(&header_path, header).unwrap_or_else(|err| {
      panic!(
        "failed to write post-processed header {}: {err}",
        header_path.display()
      )
    });
  }

  // Convenience copy: make the generated header available at a stable location
  // under the runtime crate.
  //
  // This file is generated, not source-of-truth. The ABI definitions live in
  // `runtime-native-abi/src/lib.rs`.
  let include_dir = crate_dir.join("..").join("runtime-native").join("include");
  if let Err(err) = std::fs::create_dir_all(&include_dir) {
    panic!(
      "failed to create runtime-native include dir at {}: {err}",
      include_dir.display()
    );
  }
  let include_path = include_dir.join("runtime_native_abi.h");
  if let Err(err) = std::fs::copy(&header_path, &include_path) {
    panic!(
      "failed to copy generated header from {} to {}: {err}",
      header_path.display(),
      include_path.display()
    );
  }
}
