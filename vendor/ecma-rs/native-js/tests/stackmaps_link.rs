#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{fs, process::Command};

use anyhow::{anyhow, Context as _, Result};
use object::{Object as _, ObjectSection as _};

use native_js::link::LinkOpts;

/// End-to-end test: generate an object file that contains `.llvm_stackmaps`,
/// link it into an executable, and ensure the final binary keeps the stackmaps
/// section without keeping a relocation section for it.
///
/// This is a regression test for PIE linking: when building a PIE, `.llvm_stackmaps`
/// can require runtime relocations which often triggers `DT_TEXTREL` warnings.
#[test]
fn link_preserves_llvm_stackmaps_without_reloc_section() -> Result<()> {
    if !command_works("clang-18") {
        eprintln!("skipping: clang-18 not found in PATH");
        return Ok(());
    }

    if !command_works("llc-18") {
        eprintln!("skipping: llc-18 not found in PATH");
        return Ok(());
    }

    let tmp = tempfile::tempdir().context("create tempdir")?;
    let ll_path = tmp.path().join("poc.ll");
    let obj_path = tmp.path().join("poc.o");
    let exe_path = tmp.path().join("poc_exe");

    fs::write(&ll_path, STATEPOINT_POC_LLVM_IR).context("write llvm ir")?;
    run(Command::new("llc-18").args(["-filetype=obj", ll_path.to_str().unwrap(), "-o"]).arg(&obj_path))
        .context("llc-18")?;

    let obj_bytes = fs::read(&obj_path).context("read object")?;
    assert_section_present_non_empty(&obj_bytes, ".llvm_stackmaps")?;

    // Make sure the object actually contains relocations for the stackmaps section;
    // otherwise the link-stage assertion would be less meaningful.
    assert_section_present_non_empty(&obj_bytes, ".rela.llvm_stackmaps")?;

    let opts = LinkOpts::default();
    native_js::link::link_object_buffers_to_elf_executable(&exe_path, &[obj_bytes.as_slice()], opts)?;

    let exe_bytes = fs::read(&exe_path).context("read linked executable")?;
    // `LinkOpts::default()` should be non-PIE on Linux (ET_EXEC).
    let elf_type = u16::from_le_bytes([exe_bytes[16], exe_bytes[17]]);
    assert_eq!(elf_type, 2, "expected non-PIE ET_EXEC (e_type={elf_type})");

    assert_section_present_non_empty(&exe_bytes, ".llvm_stackmaps")?;
    assert_section_absent(&exe_bytes, ".rela.llvm_stackmaps")?;
    assert_section_absent(&exe_bytes, ".rel.llvm_stackmaps")?;

    // Optional: stripping should not remove the allocated `.llvm_stackmaps` section.
    if command_works("strip") {
        run(Command::new("strip").arg(&exe_path)).context("strip")?;
        let stripped = fs::read(&exe_path).context("read stripped executable")?;
        assert_section_present_non_empty(&stripped, ".llvm_stackmaps")?;
        assert_section_absent(&stripped, ".rela.llvm_stackmaps")?;
    }

    Ok(())
}

fn command_works(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("run {:?}", cmd))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "command failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn assert_section_present_non_empty(bytes: &[u8], name: &str) -> Result<()> {
    let file = object::File::parse(bytes).context("parse object/elf")?;
    let sec = file
        .section_by_name(name)
        .ok_or_else(|| anyhow!("expected section {name} to exist"))?;
    if sec.size() == 0 {
        return Err(anyhow!("expected section {name} to be non-empty"));
    }
    Ok(())
}

fn assert_section_absent(bytes: &[u8], name: &str) -> Result<()> {
    let file = object::File::parse(bytes).context("parse object/elf")?;
    if file.section_by_name(name).is_some() {
        return Err(anyhow!("expected section {name} to be absent"));
    }
    Ok(())
}

const STATEPOINT_POC_LLVM_IR: &str = r#"
; ModuleID = 'native-js-stackmaps-poc'
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define i32 @main() {
entry:
  %x = add i64 40, 2
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 0, i32 0, i64 %x)
  ret i32 0
}
"#;
