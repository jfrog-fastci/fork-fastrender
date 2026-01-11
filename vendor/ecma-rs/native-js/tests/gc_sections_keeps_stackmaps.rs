#[cfg(target_os = "linux")]
mod linux {
  use anyhow::{bail, Context, Result};
  use native_js::link::{link_elf_executable_with_options, LinkOpts};
  use object::{Object as _, ObjectSection as _};
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::process::{Command, Stdio};
  use tempfile::tempdir;

  const STACKMAP_SECTION_CANDIDATES: [&str; 2] = [".data.rel.ro.llvm_stackmaps", ".llvm_stackmaps"];

  fn cmd_works(cmd: &str) -> bool {
    Command::new(cmd)
      .arg("--version")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
  }

  fn find_clang() -> Option<&'static str> {
    for cand in ["clang-18", "clang"] {
      if cmd_works(cand) {
        return Some(cand);
      }
    }
    None
  }

  fn read_elf_section_size_any(path: &Path, names: &[&str]) -> Result<(String, u64)> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file = object::File::parse(&*bytes).context("parse linked ELF")?;
    for name in names {
      if let Some(section) = file.section_by_name(name) {
        return Ok(((*name).to_string(), section.size()));
      }
    }
    bail!(
      "ELF {} does not contain any of the candidate stackmaps sections: {names:?}",
      path.display()
    );
  }

  fn clang_compile_ll_to_object(clang: &str, ll: &Path, obj: &Path) -> Result<()> {
    let status = Command::new(clang)
      .arg("-c")
      .arg(ll)
      .arg("-o")
      .arg(obj)
      .status()
      .with_context(|| format!("{clang} -c {} -o {}", ll.display(), obj.display()))?;
    if !status.success() {
      bail!("{clang} failed with status {status}");
    }
    Ok(())
  }

  #[test]
  fn keeps_llvm_stackmaps_under_gc_sections() -> Result<()> {
    let Some(clang) = find_clang() else {
      eprintln!("skipping: clang not found in PATH");
      return Ok(());
    };
    if !cmd_works("ld.lld-18") && !cmd_works("ld.lld") {
      eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
      return Ok(());
    }

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
    clang_compile_ll_to_object(clang, &ll, &obj)?;

    let exe = td.path().join("stackmap_gc");
    link_elf_executable_with_options(
      &exe,
      &[obj],
      LinkOpts {
        gc_sections: true,
        ..Default::default()
      },
    )?;
 
    let (section, stackmaps_size) = read_elf_section_size_any(&exe, &STACKMAP_SECTION_CANDIDATES)?;
    if stackmaps_size == 0 {
      bail!("linked ELF contains empty {section} section");
    }

    let status = Command::new(&exe)
      .status()
      .with_context(|| format!("run {}", exe.display()))?;
    if !status.success() {
      bail!("linked binary failed with status {status}");
    }

    if cmd_works("strip") {
      let exe_stripped: PathBuf = td.path().join("stackmap_gc.stripped");
      fs::copy(&exe, &exe_stripped).context("copy binary for strip")?;
      let strip_status = Command::new("strip")
        .arg(&exe_stripped)
        .status()
        .context("run strip")?;
      if !strip_status.success() {
        bail!("strip failed with status {strip_status}");
      }

      let (stripped_section, stripped_stackmaps_size) =
        read_elf_section_size_any(&exe_stripped, &STACKMAP_SECTION_CANDIDATES)?;
      if stripped_stackmaps_size == 0 {
        bail!("stripped ELF contains empty {stripped_section} section");
      }
    } else {
      eprintln!("skipping strip check: strip not found in PATH");
    }

    Ok(())
  }
}
