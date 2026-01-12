#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(maps) = llvm_stackmaps::StackMaps::parse(data) else {
        return;
    };

    for rec in &maps.records {
        // Decoding is fallible; we only care that it never panics on hostile inputs.
        let _ = llvm_stackmaps::StatepointRecordView::decode(rec);
    }
});

