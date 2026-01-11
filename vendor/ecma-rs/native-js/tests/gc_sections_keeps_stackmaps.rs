#[cfg(target_os = "linux")]
mod linux {
  use anyhow::{bail, Context, Result};
  use native_js::link::{link_elf_executable_with_options, LinkOpts};
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::process::Command;
  use tempfile::tempdir;

  fn read_elf_section_size(path: &Path, name: &str) -> Result<u64> {
    let out = Command::new("readelf")
      .arg("-W")
      .arg("-S")
      .arg(path)
      .output()
      .with_context(|| format!("readelf -W -S {}", path.display()))?;
    if !out.status.success() {
      bail!(
        "readelf failed: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
      );
    }

    let stdout = String::from_utf8(out.stdout).context("readelf output is not utf-8")?;
    for line in stdout.lines() {
      let fields: Vec<&str> = line.split_whitespace().collect();
      if fields.len() >= 6 && fields[1] == name {
        let size_hex = fields[5];
        return u64::from_str_radix(size_hex, 16)
          .with_context(|| format!("parse section size hex: {size_hex}"));
      }
    }
    bail!("ELF {} does not contain section {name}", path.display());
  }

  fn clang_compile_ll_to_object(ll: &Path, obj: &Path) -> Result<()> {
    let status = Command::new("clang-18")
      .arg("-c")
      .arg(ll)
      .arg("-o")
      .arg(obj)
      .status()
      .with_context(|| format!("clang-18 -c {} -o {}", ll.display(), obj.display()))?;
    if !status.success() {
      bail!("clang-18 failed with status {status}");
    }
    Ok(())
  }

  #[test]
  fn keeps_llvm_stackmaps_under_gc_sections() -> Result<()> {
    let td = tempdir().context("create temp dir")?;

    let ll = td.path().join("stackmap.ll");
    fs::write(
      &ll,
      r#"
; ModuleID = 'stackmap'
source_filename = "stackmap"
target triple = "x86_64-pc-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define i64 @foo(i64 %x) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, i64 %x)
  ret i64 %x
}

define i32 @main() {
entry:
  %r = call i64 @foo(i64 42)
  ret i32 0
}
"#,
    )
    .context("write stackmap.ll")?;

    let obj = td.path().join("stackmap.o");
    clang_compile_ll_to_object(&ll, &obj)?;

    let exe = td.path().join("stackmap_gc");
    link_elf_executable_with_options(
      &exe,
      &[obj],
      LinkOpts {
        gc_sections: true,
        ..Default::default()
      },
    )?;

    let stackmaps_size = read_elf_section_size(&exe, ".llvm_stackmaps")?;
    if stackmaps_size == 0 {
      bail!("linked ELF contains empty .llvm_stackmaps section");
    }

    let status = Command::new(&exe)
      .status()
      .with_context(|| format!("run {}", exe.display()))?;
    if !status.success() {
      bail!("linked binary failed with status {status}");
    }

    let exe_stripped: PathBuf = td.path().join("stackmap_gc.stripped");
    fs::copy(&exe, &exe_stripped).context("copy binary for strip")?;
    let strip_status = Command::new("strip")
      .arg(&exe_stripped)
      .status()
      .context("run strip")?;
    if !strip_status.success() {
      bail!("strip failed with status {strip_status}");
    }

    let stripped_stackmaps_size = read_elf_section_size(&exe_stripped, ".llvm_stackmaps")?;
    if stripped_stackmaps_size == 0 {
      bail!("stripped ELF contains empty .llvm_stackmaps section");
    }

    Ok(())
  }
}
