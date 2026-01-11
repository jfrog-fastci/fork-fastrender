#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use anyhow::{bail, Context, Result};
use native_js::link::{link_elf_executable_with_options, LinkOpts, LinkerFlavor};
use object::{Object, ObjectSegment};
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::tempdir;

const STACKMAP_MAGIC: &[u8] = b"FASTR_STACKMAPS_TEST_MAGIC\0";

fn file_contains_bytes(path: &Path, needle: &[u8]) -> Result<bool> {
  let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
  Ok(data.windows(needle.len()).any(|w| w == needle))
}

fn has_wx_load_segment(path: &Path) -> Result<bool> {
  let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
  let file = object::File::parse(&*data).context("parse linked output")?;
  for seg in file.segments() {
    let flags = seg.flags();
    if let object::SegmentFlags::Elf { p_flags } = flags {
      // PF_X=1, PF_W=2.
      if (p_flags & 1) != 0 && (p_flags & 2) != 0 {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

fn find_clang() -> Result<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Ok(cand);
    }
  }
  bail!("unable to locate clang (expected `clang-18` or `clang`)")
}

fn has_cmd(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_lld_fuse_arg() -> Option<&'static str> {
  // Prefer the version-suffixed binary when available (matches our exec plan install).
  if has_cmd("ld.lld-18") {
    Some("lld-18")
  } else if has_cmd("ld.lld") {
    Some("lld")
  } else {
    None
  }
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
  .ascii "FASTR_STACKMAPS_TEST_MAGIC"
  .byte 0
  .quad 0x1122334455667788

.section .note.GNU-stack,"",@progbits
"#,
    )
    .context("write asm")?;

  let clang = match find_clang() {
    Ok(clang) => clang,
    Err(_) => {
      eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
      return Ok(());
    }
  };
  let lld_fuse = find_lld_fuse_arg();

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

  // Failure mode: linking with `--gc-sections` can drop stackmaps if nothing references them.
  let out_without_script = td.path().join("a.out");
  let status = Command::new(clang)
    .arg("-no-pie")
    .arg("-Wl,--gc-sections")
    .arg(&obj_path)
    .arg("-o")
    .arg(&out_without_script)
    .status()
    .with_context(|| format!("link without script to {}", out_without_script.display()))?;
  if !status.success() {
    bail!("{clang} failed to link (no script) with status {status}");
  }
  if file_contains_bytes(&out_without_script, STACKMAP_MAGIC)? {
    bail!("expected llvm_stackmaps section(s) to be dropped by --gc-sections without KEEP()");
  }

  if let Some(lld_fuse) = find_lld_fuse_arg() {
    let out_without_script_lld = td.path().join("a_lld.out");
    let status = Command::new(clang)
      .arg(format!("-fuse-ld={lld_fuse}"))
      .arg("-no-pie")
      .arg("-Wl,--gc-sections")
      .arg(&obj_path)
      .arg("-o")
      .arg(&out_without_script_lld)
      .status()
      .with_context(|| format!("link without script (lld) to {}", out_without_script_lld.display()))?;
    if !status.success() {
      bail!("{clang} failed to link (lld, no script) with status {status}");
    }
    if file_contains_bytes(&out_without_script_lld, STACKMAP_MAGIC)? {
      bail!("expected llvm_stackmaps section(s) to be dropped by --gc-sections under lld without KEEP()");
    }
  }

  // Fixed: native-js link helpers always inject `runtime-native/link/stackmaps.ld`.
  // When linking with lld, stackmaps are often merged into a broader output section (e.g.
  // `.data.rel.ro`), so assert based on the payload bytes rather than section names.
  if lld_fuse.is_some() {
    if !has_cmd("llvm-objcopy-18") && !has_cmd("llvm-objcopy") {
      eprintln!("skipping lld linker-script check: llvm-objcopy not found in PATH");
    } else {
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

      if !file_contains_bytes(&out_with_script, STACKMAP_MAGIC)? {
        bail!(
          "expected llvm_stackmaps section(s) to survive --gc-sections when kept via linker script"
        );
      }
    }
  } else {
    eprintln!("skipping lld linker-script check: lld not found in PATH");
  }

  // Ensure the same fragment is accepted by GNU ld too (in addition to lld).
  let out_with_script_ld = td.path().join("c.out");
  link_elf_executable_with_options(
    &out_with_script_ld,
    &[obj_path.clone()],
    LinkOpts {
      gc_sections: true,
      linker: LinkerFlavor::System,
      ..Default::default()
    },
  )
  .context("link with KEEP() linker script (system ld)")?;
  if !file_contains_bytes(&out_with_script_ld, STACKMAP_MAGIC)? {
    bail!("expected llvm_stackmaps section(s) to survive --gc-sections (system ld)");
  }

  // PIE + system ld: ensure we use the GNU ld-specific stackmaps fragment to avoid RWX segments.
  let out_with_script_pie_ld = td.path().join("d_pie.out");
  link_elf_executable_with_options(
    &out_with_script_pie_ld,
    &[obj_path.clone()],
    LinkOpts {
      gc_sections: true,
      pie: true,
      linker: LinkerFlavor::System,
      ..Default::default()
    },
  )
  .context("link with KEEP() linker script (system ld + PIE)")?;
  if !file_contains_bytes(&out_with_script_pie_ld, STACKMAP_MAGIC)? {
    bail!("expected llvm_stackmaps section(s) to survive --gc-sections (system ld + PIE)");
  }
  if has_wx_load_segment(&out_with_script_pie_ld)? {
    bail!("unexpected RWX LOAD segment in system-ld PIE output (expected stackmaps_gnuld.ld placement)");
  }

  Ok(())
}
