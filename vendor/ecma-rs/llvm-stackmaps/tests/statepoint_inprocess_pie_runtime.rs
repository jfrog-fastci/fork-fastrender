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

fn elf64_e_type(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 0x12 {
        return None;
    }
    if &bytes[0..4] != b"\x7FELF" {
        return None;
    }
    // EI_CLASS=2 => ELF64
    if bytes[4] != 2 {
        return None;
    }
    // e_type is a u16 at offset 0x10 in the ELF header.
    Some(u16::from_le_bytes([bytes[0x10], bytes[0x11]]))
}

const INPUT_IR: &str = r#"
; ModuleID = 'stackmaps_statepoint_inprocess_pie'

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

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn inprocess_loader_finds_statepoint_callsite_in_pie_binary() -> io::Result<()> {
    // Needs LLVM tools to produce a real stackmap table and patch it for PIE.
    let Some(opt) = find_tool(&["opt-18", "opt"]) else {
        return Ok(());
    };
    let Some(llc) = find_tool(&["llc-18", "llc"]) else {
        return Ok(());
    };
    let Some(objcopy) = find_tool(&["llvm-objcopy-18", "llvm-objcopy"]) else {
        return Ok(());
    };

    // Needs clang + lld to link the final executable with our linker script.
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

    // 1) Compile IR -> rewritten statepoints -> position-independent .o
    let input_ll = dir.join("input.ll");
    let rewritten_ll = dir.join("rewritten.ll");
    let obj = dir.join("sp.o");
    let obj_patched = dir.join("sp_pie.o");
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
            "-relocation-model=pic",
            "-filetype=obj",
        ])
        .arg(&rewritten_ll)
        .arg("-o")
        .arg(&obj)
        .status()?;
    assert!(status.success(), "llc failed");

    // 2) Rewrite `.llvm_stackmaps` -> `.data.rel.ro.llvm_stackmaps` so dynamic relocations are
    // applied to writable memory (avoids DT_TEXTREL).
    fs::copy(&obj, &obj_patched)?;
    let status = Command::new(objcopy)
        .args([
            "--rename-section",
            ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents",
        ])
        .arg(&obj_patched)
        .status()?;
    assert!(status.success(), "llvm-objcopy failed");

    // 3) Build a tiny Rust binary that links the object file and, at runtime:
    //    - captures the caller return address inside `allocate`
    //    - parses in-process stackmaps via `__start_llvm_stackmaps/__stop_llvm_stackmaps`
    //    - looks up the record by the captured return address
    let project_dir = dir.join("project");
    fs::create_dir_all(project_dir.join("src"))?;

    fs::write(
        project_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "stackmaps_statepoint_pie_rt"
version = "0.0.0"
edition = "2021"

[dependencies]
llvm-stackmaps = {{ path = "{}" }}
"#,
            ws_root.join("llvm-stackmaps").display()
        ),
    )?;

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
        "expected stackmaps_bytes() to find an in-process stackmaps section"
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

    // Speed up the nested build: we only care about the link result + in-process parsing (symbols,
    // section retention, and dynamic relocations), not debug info.
    let rustflags = format!(
        "-C debuginfo=0 \
         -C linker={clang} \
         -C link-arg=-fuse-ld={lld_fuse} \
         -C link-arg=-pie \
         -C link-arg=-Wl,-T,{} \
         -C link-arg=-Wl,--gc-sections \
         -C link-arg={} \
         -C force-frame-pointers=yes",
        stackmaps_ld.display(),
        obj_patched.display(),
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

    let exe_path = target_dir.join("debug/stackmaps_statepoint_pie_rt");

    let exe_bytes = fs::read(&exe_path)?;
    let etype = elf64_e_type(&exe_bytes).expect("expected an ELF64 binary");
    const ET_DYN: u16 = 3;
    assert_eq!(
        etype, ET_DYN,
        "expected PIE output (ET_DYN), got e_type={etype}"
    );

    // Best-effort: ensure the binary does not require text relocations.
    if has_cmd("readelf") {
        let out = Command::new("readelf").args(["-d", exe_path.to_str().unwrap()]).output()?;
        assert!(out.status.success(), "readelf -d failed");
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !combined.contains("TEXTREL"),
            "unexpected DT_TEXTREL in PIE output:\n{combined}"
        );
    }

    // Also validate the linked PIE executable via the offline verifier (ELF extraction + parse).
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
    assert!(status.success(), "stackmaps_statepoint_pie_rt failed");
    Ok(())
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
#[test]
fn inprocess_loader_finds_statepoint_callsite_in_pie_binary() -> io::Result<()> {
    Ok(())
}
