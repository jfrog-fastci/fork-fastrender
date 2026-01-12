#![no_main]

use libfuzzer_sys::fuzz_target;

use std::sync::Once;

// Seed corpus from real `.llvm_stackmaps` fixtures (checked in under `llvm-stackmaps/tests/fixtures`).
const SEED_DEOPT_BUNDLE2: &[u8] = include_bytes!("../../llvm-stackmaps/tests/fixtures/deopt_bundle2.stackmaps.bin");
const SEED_DEOPT_TRANSITION: &[u8] =
    include_bytes!("../../llvm-stackmaps/tests/fixtures/deopt_transition.stackmaps.bin");
const SEED_DEOPT_VAR: &[u8] = include_bytes!("../../llvm-stackmaps/tests/fixtures/deopt_var.stackmaps.bin");
const SEED_TRANSITION_BUNDLE: &[u8] =
    include_bytes!("../../llvm-stackmaps/tests/fixtures/transition_bundle.stackmaps.bin");
const SEED_LLVM18_TWO_FUNCS: &[u8] =
    include_bytes!("../../llvm-stackmaps/tests/fixtures/llvm18_stackmaps/two_funcs.stackmaps.bin");
const SEED_LLVM18_TWO_STATEPOINTS: &[u8] = include_bytes!(
    "../../llvm-stackmaps/tests/fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin"
);

fuzz_target!(|data: &[u8]| {
    static INIT: Once = Once::new();
    let opts = llvm_stackmaps::ParseOptions::FUZZING;

    INIT.call_once(|| {
        for seed in [
            SEED_DEOPT_BUNDLE2,
            SEED_DEOPT_TRANSITION,
            SEED_DEOPT_VAR,
            SEED_TRANSITION_BUNDLE,
            SEED_LLVM18_TWO_FUNCS,
            SEED_LLVM18_TWO_STATEPOINTS,
        ] {
            let _ = llvm_stackmaps::StackMaps::parse_with_options(seed, &opts);
        }
    });

    let _ = llvm_stackmaps::StackMaps::parse_with_options(data, &opts);
});
