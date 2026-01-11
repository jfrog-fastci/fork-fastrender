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

  // Prefer `size_t` for Rust `usize` so this header composes cleanly with the
  // existing handwritten `runtime_native.h` (which uses `size_t` for all
  // lengths/sizes). cbindgen defaults to `uintptr_t`.
  if header.contains("uintptr_t") {
    header = header.replace("uintptr_t", "size_t");
    modified = true;
  }

  // cbindgen emits some `usize`-typed bit flags as untyped integer expressions
  // (e.g. `#define RT_ARRAY_ELEM_PTR_FLAG (1 << 63)`), which is UB in C (shifting
  // a 32-bit `int` by 63). Rewrite those to explicitly use `size_t`.
  if header.contains("#define RT_ARRAY_ELEM_PTR_FLAG (1 << 63)") {
    header = header.replace(
      "#define RT_ARRAY_ELEM_PTR_FLAG (1 << 63)",
      "#define RT_ARRAY_ELEM_PTR_FLAG ((size_t)1u << 63)",
    );
    modified = true;
  }

  // Convenience macros for array allocation callers (mirrors `runtime_native.h`).
  if !header.contains("RT_ARRAY_ENCODE_PTR_ELEM_SIZE") {
    if let Some(pos) = header.find("#define RT_ARRAY_DATA_OFFSET") {
      if let Some(line_end) = header[pos..].find('\n') {
        let insert_at = pos + line_end + 1;
        header.insert_str(
          insert_at,
          concat!(
            "\n// Encode an `elem_size` value that requests a pointer-element array.\n",
            "#define RT_ARRAY_ENCODE_PTR_ELEM_SIZE() (sizeof(void*) | RT_ARRAY_ELEM_PTR_FLAG)\n",
            "\n// Compute the pointer to the element payload for an array base pointer.\n",
            "#define RT_ARRAY_DATA_PTR(base) ((uint8_t*)(base) + RT_ARRAY_DATA_OFFSET)\n",
          ),
        );
        modified = true;
      }
    }
  }

  // Make the header usable from C++ callers too (C linkage).
  if !header.contains("extern \"C\"") {
    if let Some(insert_at) = header.find("/**") {
      header.insert_str(
        insert_at,
        concat!(
          "#ifdef __cplusplus\n",
          "extern \"C\" {\n",
          "#endif\n\n",
        ),
      );
      modified = true;
    }
    if let Some(end_at) = header.rfind("#endif /* RUNTIME_NATIVE_ABI_H */") {
      header.insert_str(
        end_at,
        concat!(
          "\n#ifdef __cplusplus\n",
          "} // extern \"C\"\n",
          "#endif\n\n",
        ),
      );
      modified = true;
    }
  }
  // cbindgen emits opaque structs as a 0-length byte array field, which is a non-standard C
  // extension. Replace those with true forward declarations for maximum compatibility.
  for name in ["PromiseHeader", "Runtime", "Thread", "RtPromise"] {
    let start_pat = format!("typedef struct {name} {{");
    if let Some(start) = header.find(&start_pat) {
      let end_pat = format!("}} {name};");
      if let Some(end_rel) = header[start..].find(&end_pat) {
        let mut end = start + end_rel + end_pat.len();
        if header[end..].starts_with("\r\n") {
          end += 2;
        } else if header[end..].starts_with('\n') {
          end += 1;
        }
        header.replace_range(start..end, &format!("typedef struct {name} {name};\n"));
        modified = true;
      }
    }
  }

  // Flexible array members (`[u8; 0]` in Rust) are emitted by cbindgen as
  // `uint8_t data[0]`, which is rejected by strict C++ compilers. Model the same
  // C/C++ split used in `runtime_native.h`:
  // - C:   `uint8_t data[];`
  // - C++: `uint8_t data[1];` (still yields the correct `offsetof` for the payload)
  if header.contains("uint8_t data[0];") {
    header = header.replace(
      "uint8_t data[0];",
      concat!(
        "#if defined(__cplusplus)\n",
        "  // Flexible array members are not standard C++; use a 1-byte trailing field to\n",
        "  // keep the header usable from C++ while still computing the correct payload\n",
        "  // offset via `offsetof(RtArrayHeader, data)`.\n",
        "  uint8_t data[1];\n",
        "#else\n",
        "  uint8_t data[];\n",
        "#endif\n",
      ),
    );
    modified = true;
  }

  // cbindgen emits `rt_queue_microtask(struct Microtask task)` even though it also emits a typedef
  // for `Microtask`. Prefer using the typedef name so the generated header matches
  // `runtime_native.h` and is friendlier to bindings generators that key off exact substrings.
  if header.contains("rt_queue_microtask(struct Microtask") {
    header = header.replace(
      "rt_queue_microtask(struct Microtask",
      "rt_queue_microtask(Microtask",
    );
    modified = true;
  }

  // cbindgen does not currently emit foreign `extern` statics. The runtime exports
  // `RT_GC_EPOCH` as a link-visible symbol used by codegen for fast safepoint
  // polling; inject an extern declaration so the generated header is complete.
  if !header.contains("RT_GC_EPOCH") {
    // cbindgen's emitted prototype for `rt_alloc` depends on the Rust signature; prefer matching
    // against that, but be robust to future typedefs.
    let insert_at = header
      .find("extern uint8_t *rt_alloc")
      .or_else(|| header.find("extern GcPtr rt_alloc"))
      .or_else(|| header.find("extern "));
    if let Some(insert_at) = insert_at {
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
