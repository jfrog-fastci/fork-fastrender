use llvm_stackmaps::verify::{verify_stackmaps_bytes, VerifyOptions};

#[test]
fn verifier_accepts_all_checked_in_fixtures() {
    let fixtures: &[(&str, &[u8])] = &[
        (
            "deopt_bundle2",
            include_bytes!("fixtures/deopt_bundle2.stackmaps.bin"),
        ),
        (
            "deopt_transition",
            include_bytes!("fixtures/deopt_transition.stackmaps.bin"),
        ),
        ("deopt_var", include_bytes!("fixtures/deopt_var.stackmaps.bin")),
        (
            "transition_bundle",
            include_bytes!("fixtures/transition_bundle.stackmaps.bin"),
        ),
        (
            "llvm18_two_funcs",
            include_bytes!("fixtures/llvm18_stackmaps/two_funcs.stackmaps.bin"),
        ),
        (
            "llvm18_two_statepoints",
            include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin"),
        ),
    ];

    for (name, bytes) in fixtures {
        let report = verify_stackmaps_bytes(bytes, VerifyOptions::default());
        assert!(
            report.ok(),
            "verifier rejected fixture {name}: {:#?}",
            report.failures
        );
    }
}

