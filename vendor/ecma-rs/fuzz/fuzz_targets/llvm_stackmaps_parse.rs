#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = llvm_stackmaps::StackMaps::parse(data);
});

