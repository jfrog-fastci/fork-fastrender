#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use runtime_native::load_llvm_stackmaps;

// Ensure the section isn't GC'd by the linker and is discoverable by the stackmaps loader.
#[used]
#[link_section = ".data.rel.ro.llvm_stackmaps"]
static TEST_SECTION_BYTES: [u8; 8] = *b"STACKMAP";

#[test]
fn load_llvm_stackmaps_finds_section_in_running_binary() {
  let bytes = load_llvm_stackmaps().expect("load stackmaps section");
  assert!(
    bytes
      .windows(TEST_SECTION_BYTES.len())
      .any(|w| w == TEST_SECTION_BYTES),
    "loaded stackmaps did not contain expected marker bytes"
  );
}
