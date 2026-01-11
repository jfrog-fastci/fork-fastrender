#[cfg(target_os = "linux")]
use runtime_native::stackmaps::StackMaps;
#[cfg(target_os = "linux")]
use runtime_native::stackmaps_loader::try_load_stackmaps_from_self_linux_phdr;

// Define a real text symbol so the embedded stackmap blob can reference an executable address.
#[cfg(target_os = "linux")]
#[no_mangle]
pub extern "C" fn stackmap_target() {}

// Embed a minimal, valid StackMap v3 blob into a stackmap section without using
// the repo's linker-script fragments (`runtime-native/link/stackmaps*.ld`).
//
// The function address field is relocated to `stackmap_target`, so the runtime can validate the
// candidate blob by checking that function addresses point into executable PT_LOAD segments.
#[cfg(target_os = "linux")]
core::arch::global_asm!(
  r#"
    // Use `.data.rel.ro.*` so the relocation against `stackmap_target` is legal for PIE builds
    // (avoids text relocations).
    .section .data.rel.ro.llvm_stackmaps,"aw",@progbits
    .p2align 3
    .globl __test_llvm_stackmaps_blob
__test_llvm_stackmaps_blob:
    // StackMap v3 header.
    .byte 3
    .byte 0
    .short 0
    .long 1      // NumFunctions
    .long 0      // NumConstants
    .long 1      // NumRecords

    // StackSizeRecord[0]
    .quad stackmap_target  // function address (relocated)
    .quad 32               // stack size
    .quad 1                // record count

    // StackMapRecord[0] with 0 locations / 0 live-outs.
    .quad 1      // patchpoint id
    .long 0      // instruction offset
    .short 0     // reserved
    .short 0     // num locations
    .short 0     // live-out padding (ignored)
    .short 0     // num live-outs
    .long 0      // record-end padding to 8-byte alignment
"#
);

#[cfg(target_os = "linux")]
extern "C" {
  static __test_llvm_stackmaps_blob: u8;
}

#[cfg(target_os = "linux")]
fn main() {
  // Keep the `.llvm_stackmaps` section (and its symbol) from being GC'd by the linker.
  unsafe {
    core::ptr::read_volatile(&__test_llvm_stackmaps_blob);
  }

  let bytes =
    try_load_stackmaps_from_self_linux_phdr().expect("failed to locate .llvm_stackmaps in memory");
  assert!(!bytes.is_empty(), "stackmaps slice is unexpectedly empty");

  let maps = StackMaps::parse(bytes).expect("failed to parse discovered stackmaps");
  println!("LEN={} CALLSITES={}", bytes.len(), maps.callsites().len());
}

#[cfg(not(target_os = "linux"))]
fn main() {
  // This binary exists solely to support the Linux `dl_iterate_phdr` stackmap discovery test.
}
