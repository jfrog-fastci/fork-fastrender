#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let opts = llvm_stackmaps::ParseOptions::FUZZING;
    let Ok(maps) = llvm_stackmaps::StackMaps::parse_with_options(data, &opts) else {
        return;
    };

    for rec in &maps.records {
        // Decoding is fallible; we only care that it never panics on hostile inputs.
        if let Some(view) = llvm_stackmaps::StatepointRecordView::decode(rec) {
            let _ = view.call_conv;
            let _ = view.flags;
            let _ = view.deopt_args.len();
            let _ = view.num_gc_roots();
            for pair in view.gc_root_pairs() {
                let _ = pair.base.kind();
                let _ = pair.derived.kind();
            }
        }
    }

    // Also exercise the callsite index + lookup APIs.
    for cs in maps.callsites() {
        let _ = maps.lookup_callsite(cs.pc);
        let _ = maps.lookup(cs.pc);
        let _ = maps.lookup_statepoint(cs.pc);
    }
});
