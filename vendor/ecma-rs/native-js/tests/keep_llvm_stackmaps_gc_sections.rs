#[cfg(target_os = "linux")]
mod linux {
  use anyhow::{bail, Context, Result};
  use native_js::link::{link_elf_executable_with_options, LinkOpts};
  use object::{Object, ObjectSection};
  use std::path::Path;
  use std::process::{Command, Stdio};
  use tempfile::tempdir;

  fn has_section_containing(path: &Path, needle: &str) -> Result<bool> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file = object::File::parse(&*data).context("parse linked output")?;
    Ok(
      file
        .sections()
        .filter_map(|section| section.name().ok())
        .any(|section_name| section_name.contains(needle)),
    )
  }

  fn find_clang() -> Result<&'static str> {
    for cand in ["clang-18", "clang"] {
      if Command::new(cand)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
      {
        return Ok(cand);
      }
    }
    bail!("unable to locate clang (expected `clang-18` or `clang`)")
  }
 
  #[test]
  fn keep_llvm_stackmaps_with_linker_script_under_gc_sections() -> Result<()> {
    let td = tempdir().context("tempdir")?;
    let asm_path = td.path().join("stackmaps.s");
    let obj_path = td.path().join("stackmaps.o");
 
    std::fs::write(
      &asm_path,
      r#"
.globl main
.type main,@function
main:
  xor %eax,%eax
  ret

.section .llvm_stackmaps,"a",@progbits
  .quad 0x1122334455667788

.section .note.GNU-stack,"",@progbits
"#,
    )
    .context("write asm")?;
 
    let clang = find_clang()?;
 
    let status = Command::new(clang)
      .arg("-c")
      .arg(&asm_path)
      .arg("-o")
      .arg(&obj_path)
      .status()
      .with_context(|| format!("{clang} -c {} -o {}", asm_path.display(), obj_path.display()))?;
    if !status.success() {
      bail!("{clang} failed to compile asm with status {status}");
    }
 
    let out_without_script = td.path().join("a.out");
    let status = Command::new(clang)
      .arg("-Wl,--gc-sections")
      .arg(&obj_path)
      .arg("-o")
      .arg(&out_without_script)
      .status()
      .with_context(|| format!("link without script to {}", out_without_script.display()))?;
    if !status.success() {
      bail!("{clang} failed to link (no script) with status {status}");
    }
    if has_section_containing(&out_without_script, "llvm_stackmaps")? {
      bail!("expected llvm_stackmaps section(s) to be dropped by --gc-sections without KEEP()");
    }
 
    let out_with_script = td.path().join("b.out");
    link_elf_executable_with_options(
      &out_with_script,
      &[obj_path.clone()],
      LinkOpts {
        gc_sections: true,
        ..Default::default()
      },
    )
    .context("link with KEEP() linker script")?;

    if !has_section_containing(&out_with_script, "llvm_stackmaps")? {
      bail!("expected llvm_stackmaps section(s) to survive --gc-sections when kept via linker script");
    }

    Ok(())
  }
}
