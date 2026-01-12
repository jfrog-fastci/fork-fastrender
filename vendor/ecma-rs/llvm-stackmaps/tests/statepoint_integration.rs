use std::{fs, process::Command};

use llvm_stackmaps::{elf, Location, StackMaps};

const INPUT_IR: &str = r#"
; ModuleID = 'stackmaps_statepoint_integration'

declare ptr addrspace(1) @allocate(i64)

declare void @use(ptr addrspace(1)) #0
attributes #0 = { "gc-leaf-function" }

define ptr addrspace(1) @test(ptr addrspace(1) %p) gc "coreclr" {
entry:
  %obj = call ptr addrspace(1) @allocate(i64 16)
  call void @use(ptr addrspace(1) %p)
  ret ptr addrspace(1) %obj
}
"#;

fn have_tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_tool<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|c| have_tool(c))
}

#[test]
fn integration_statepoint_stackmap_lookup() {
    let Some(opt) = find_tool(&["opt-18", "opt"]) else {
        eprintln!("skipping: LLVM tools (opt/llc) not found in PATH");
        return;
    };
    let Some(llc) = find_tool(&["llc-18", "llc"]) else {
        eprintln!("skipping: LLVM tools (opt/llc) not found in PATH");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let input_ll = dir.path().join("input.ll");
    let rewritten_ll = dir.path().join("rewritten.ll");
    let obj = dir.path().join("out.o");

    fs::write(&input_ll, INPUT_IR).unwrap();

    let status = Command::new(opt)
        .args(["-passes=rewrite-statepoints-for-gc", "-S"])
        .arg(&input_ll)
        .arg("-o")
        .arg(&rewritten_ll)
        .status()
        .unwrap();
    assert!(status.success(), "opt failed");

    let status = Command::new(llc)
        .args([
            "-O0",
            "--fixup-allow-gcptr-in-csr=false",
            "--fixup-max-csr-statepoints=0",
            "-filetype=obj",
        ])
        .arg(&rewritten_ll)
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap();
    assert!(status.success(), "llc failed");

    let file = fs::read(&obj).unwrap();
    let section = elf::stackmaps_section_bytes(&file).unwrap();

    let maps = StackMaps::parse(section).unwrap();
    assert_eq!(maps.callsites().len(), 1, "expected exactly one callsite");

    let callsite = maps.callsites()[0];
    let rec = maps.lookup(callsite.pc).expect("lookup by callsite_pc");
    assert_eq!(rec.callsite_pc, callsite.pc);

    // Validate callsite PC computation: function address + instruction offset.
    assert_eq!(maps.functions.len(), 1);
    assert_eq!(callsite.function_address, maps.functions[0].address);
    assert_eq!(callsite.stack_size, maps.functions[0].stack_size);
    let expected_pc = maps.functions[0]
        .address
        .checked_add(rec.instruction_offset as u64)
        .unwrap();
    assert_eq!(expected_pc, callsite.pc);

    // Validate statepoint layout decoding.
    let sp = maps
        .lookup_statepoint(callsite.pc)
        .expect("statepoint decode");
    assert_eq!(sp.call_conv, 0);
    assert_eq!(sp.flags, 0);
    assert_eq!(sp.deopt_args.len(), 0);
    assert_eq!(sp.num_gc_roots(), 1);

    let pairs: Vec<_> = sp.gc_root_pairs().collect();
    assert_eq!(pairs.len(), 1);

    // Both base and derived should be indirect stack slots (typically SP-relative, but LLVM may
    // also choose FP-relative spill slots depending on code shape/opts).
    match (pairs[0].base, pairs[0].derived) {
        (
            Location::Indirect {
                size: base_size,
                dwarf_reg: base_reg,
                offset: base_off,
            },
            Location::Indirect {
                size: derived_size,
                dwarf_reg: derived_reg,
                offset: derived_off,
            },
        ) => {
            assert_eq!(*base_size, 8);
            assert_eq!(*derived_size, 8);
            assert_eq!(*base_reg, *derived_reg);
            assert_eq!(*base_off, *derived_off);

            // Stack slots may be SP- or FP-relative.
            let (expected_sp_reg, expected_fp_reg): (u16, u16) = if cfg!(target_arch = "x86_64") {
                (7, 6) // DWARF RSP, RBP
            } else if cfg!(target_arch = "aarch64") {
                (31, 29) // DWARF SP, X29
            } else {
                return;
            };
            assert!(
                *base_reg == expected_sp_reg || *base_reg == expected_fp_reg,
                "expected SP/FP-relative root slot (dwarf_reg in [{expected_sp_reg}, {expected_fp_reg}]), got dwarf_reg={base_reg}"
            );
        }
        other => panic!("unexpected root pair: {other:?}"),
    }

    // Verify the full ELF->section extraction path used by the offline verifier binary.
    let verifier = env!("CARGO_BIN_EXE_verify_stackmaps");
    let out = Command::new(verifier)
        .arg("--elf")
        .arg(&obj)
        .output()
        .expect("run verify_stackmaps");
    assert!(
        out.status.success(),
        "verify_stackmaps failed (status={})\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"ok\":true"),
        "expected ok=true in verifier JSON output, got: {stdout}"
    );
}
