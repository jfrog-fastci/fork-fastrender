use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, io};

fn has_cmd(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn find_tool<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|c| has_cmd(c))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("llvm-stackmaps should live at <workspace>/llvm-stackmaps")
        .to_path_buf()
}

const INPUT_IR: &str = r#"
; ModuleID = 'stackmaps_statepoint_inprocess'

declare ptr addrspace(1) @allocate(i64)

declare void @use(ptr addrspace(1)) #0
attributes #0 = { "gc-leaf-function" }

define ptr addrspace(1) @test(ptr addrspace(1) %p) gc "coreclr" {
entry:
  ; Include both a `"deopt"(...)` operand bundle and a `"gc-transition"(...)` bundle.
  ;
  ; LLVM 18 encodes this in the stackmap record as:
  ; - locations[1] = flags (== 1 when gc-transition is present)
  ; - locations[2] = NumDeoptArgs
  ; - locations[3..] = the deopt operand locations themselves (not GC roots)
  ; - then the GC (base, derived) relocation pairs.
  %obj = call ptr addrspace(1) @allocate(i64 16) [ "deopt"(i64 1, i64 2), "gc-transition"(i64 99) ]
  call void @use(ptr addrspace(1) %p)
  ret ptr addrspace(1) %obj
}
"#;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn inprocess_loader_finds_statepoint_callsite_by_actual_return_address() -> io::Result<()> {
    // Needs LLVM tools to produce a real stackmap table.
    let Some(opt) = find_tool(&["opt-18", "opt"]) else {
        return Ok(());
    };
    let Some(llc) = find_tool(&["llc-18", "llc"]) else {
        return Ok(());
    };

    // Needs clang + lld to link with our stackmaps linker script fragment.
    let Some(clang) = find_tool(&["clang-18", "clang"]) else {
        eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
        return Ok(());
    };
    let Some(lld_fuse) = (if has_cmd("ld.lld-18") {
        Some("lld-18")
    } else if has_cmd("ld.lld") {
        Some("lld")
    } else {
        None
    }) else {
        eprintln!("skipping: lld not found in PATH (need ld.lld-18 or ld.lld)");
        return Ok(());
    };

    let ws_root = workspace_root();
    let stackmaps_ld = ws_root.join("runtime-native").join("link").join("stackmaps.ld");

    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();

    // 1) Compile IR -> rewritten statepoints -> .o
    let input_ll = dir.join("input.ll");
    let rewritten_ll = dir.join("rewritten.ll");
    let obj = dir.join("sp.o");
    fs::write(&input_ll, INPUT_IR)?;

    let status = Command::new(opt)
        .args(["-passes=rewrite-statepoints-for-gc", "-S"])
        .arg(&input_ll)
        .arg("-o")
        .arg(&rewritten_ll)
        .status()?;
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
        .status()?;
    assert!(status.success(), "llc failed");

    // 2) Build a tiny Rust binary that links the object file and, at runtime:
    //    - captures the caller return address inside `allocate`
    //    - parses in-process `.llvm_stackmaps` via `__start_llvm_stackmaps/__stop_llvm_stackmaps`
    //    - looks up the record by the captured return address
    let project_dir = dir.join("project");
    fs::create_dir_all(project_dir.join("src"))?;

    fs::write(
        project_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "stackmaps_statepoint_rt"
version = "0.0.0"
edition = "2021"

[dependencies]
llvm-stackmaps = {{ path = "{}" }}
"#,
            ws_root.join("llvm-stackmaps").display()
        ),
    )?;

    // Note: `use` is a Rust keyword; use a raw identifier so the exported symbol name is `use`.
    //
    // Return address capture (ABI):
    // - x86_64: read from `[rbp + 8]` (requires frame pointers in the Rust binary)
    // - aarch64: read from `x30` (link register)
    fs::write(
        project_dir.join("src/main.rs"),
        r##"use std::sync::atomic::{AtomicUsize, Ordering};

use llvm_stackmaps::{stackmaps_bytes, StackMaps};

static LAST_RA: AtomicUsize = AtomicUsize::new(0);

#[no_mangle]
#[inline(never)]
pub extern "C" fn allocate(_size: i64) -> *mut u8 {
    let mut ra: usize = 0;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "mov {0}, [rbp + 8]",
            out(reg) ra,
            options(nostack, readonly, preserves_flags),
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!(
            "mov {0}, x30",
            out(reg) ra,
            options(nostack, nomem, preserves_flags),
        );
    }
    LAST_RA.store(ra, Ordering::Relaxed);
    0x1234usize as *mut u8
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn r#use(_p: *mut u8) {}

extern "C" {
    fn test(p: *mut u8) -> *mut u8;
}

fn main() {
    unsafe {
        let _ = test(core::ptr::null_mut());
    }

    let ra = LAST_RA.load(Ordering::Relaxed) as u64;
    assert_ne!(ra, 0, "expected allocate() to capture a non-zero return address");

    let bytes = stackmaps_bytes();
    assert!(
        !bytes.is_empty(),
        "expected stackmaps_bytes() to find an in-process .llvm_stackmaps section"
    );
    let maps = StackMaps::parse(bytes).expect("stackmaps should parse");
    assert_eq!(
        maps.callsites().len(),
        1,
        "expected exactly one statepoint callsite in this test program"
    );

    let rec = maps
        .lookup(ra)
        .unwrap_or_else(|| panic!("expected stackmap record for captured return address 0x{ra:x}"));
    assert_eq!(rec.callsite_pc, ra);

    let sp = maps
        .lookup_statepoint(ra)
        .expect("expected record to match statepoint layout");
    assert_eq!(sp.call_conv, 0);
    assert_eq!(sp.flags, 1);
    assert_eq!(sp.deopt_args.len(), 2);
    assert_eq!(sp.deopt_args[0].as_u64(), Some(1));
    assert_eq!(sp.deopt_args[1].as_u64(), Some(2));
    assert_eq!(sp.num_gc_roots(), 1);
}
"##,
    )?;

    let mut target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ws_root.join("target"));
    if target_dir.is_relative() {
        target_dir = ws_root.join(target_dir);
    }
    // Avoid deadlocking on Cargo's target-dir lock: use a separate target dir from the outer
    // `cargo test` process, but keep it stable so the nested builds across tests can reuse artifacts.
    let target_dir = target_dir.join("llvm_stackmaps_nested_cargo_test_target");

    // Force non-PIE output to avoid needing object section flag rewriting for `.llvm_stackmaps`.
    let rustflags = format!(
        "-C debuginfo=0 \
         -C linker={clang} \
         -C link-arg=-fuse-ld={lld_fuse} \
         -C link-arg=-Wl,-no-pie \
         -C link-arg=-Wl,-T,{} \
         -C link-arg=-Wl,--gc-sections \
         -C link-arg={} \
         -C force-frame-pointers=yes",
        stackmaps_ld.display(),
        obj.display(),
    );

    let cargo_agent = ws_root.join("scripts").join("cargo_agent.sh");
    let status = Command::new("bash")
        .arg(cargo_agent)
        .arg("build")
        .arg("--offline")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(project_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", rustflags)
        .status()?;
    assert!(status.success(), "nested build failed");

    let exe_path = target_dir.join("debug/stackmaps_statepoint_rt");

    // Also validate the linked executable via the offline verifier (ELF extraction + parse).
    let verifier = env!("CARGO_BIN_EXE_verify_stackmaps");
    let out = Command::new(verifier)
        .arg("--elf")
        .arg(&exe_path)
        .output()?;
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

    let status = Command::new(&exe_path).status()?;
    assert!(status.success(), "stackmaps_statepoint_rt failed");
    Ok(())
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
#[test]
fn inprocess_loader_finds_statepoint_callsite_by_actual_return_address() -> io::Result<()> {
    Ok(())
}
