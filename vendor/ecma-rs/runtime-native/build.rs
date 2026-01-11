use std::path::PathBuf;

fn main() {
  println!("cargo:rerun-if-changed=stackmaps.ld");

  // Linux/ELF: expose `.llvm_stackmaps` as a loaded in-memory byte slice via
  // linker-defined start/end symbols (see `stackmaps.ld`).
  //
  // Other platforms (Mach-O/PE) will need different mechanisms; keep the build
  // script gated so the crate remains portable.
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if target_os != "linux" {
    return;
  }

  let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
  let script = manifest_dir.join("stackmaps.ld");

  // Pass an *absolute* path so the linker can always find it, regardless of the
  // current working directory Cargo uses for the link step.
  println!("cargo:rustc-link-arg=-Wl,-T,{}", script.display());
}

