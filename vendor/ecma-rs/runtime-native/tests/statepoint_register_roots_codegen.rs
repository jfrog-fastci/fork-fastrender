#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions, LLVM_STATEPOINT_PATCHPOINT_ID,
};
use runtime_native::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS;
use tempfile::TempDir;

const LLVM_OPT: &str = "opt-18";
const LLVM_LLC: &str = "llc-18";

const TARGET_TRIPLE: &str = "x86_64-pc-linux-gnu";

// Mitigation: do not allow statepoint GC roots to remain in callee-saved registers.
// Required for frame-pointer-only stack walking without libunwind/ucontext.
const LCC_FIXUP_MAX_CSR_STATEPOINTS_0: &str = "--fixup-max-csr-statepoints=0";

fn assert_cmd_available(bin: &str) {
  let out = Command::new(bin)
    .arg("--version")
    .output()
    .unwrap_or_else(|e| panic!("Failed to spawn `{bin}` ({e}). Is LLVM 18 installed and in PATH?"));
  assert!(
    out.status.success(),
    "`{bin} --version` failed (status={}). stdout:\n{}\nstderr:\n{}",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
}

fn run_checked(cwd: &Path, bin: &str, args: &[&str]) -> Output {
  let output = Command::new(bin)
    .current_dir(cwd)
    .args(args)
    .output()
    .unwrap_or_else(|e| panic!("Failed to spawn `{bin}`: {e}"));
  assert!(
    output.status.success(),
    "`{bin} {}` failed (status={}).\nstdout:\n{}\nstderr:\n{}",
    args.join(" "),
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  output
}

fn write_file(path: &Path, contents: &str) {
  std::fs::write(path, contents).unwrap_or_else(|e| panic!("Failed to write {path:?}: {e}"));
}

fn rewrite_statepoints(tmp: &Path, input_ll: &Path, output_ll: &Path) {
  run_checked(
    tmp,
    LLVM_OPT,
    &[
      "-passes=rewrite-statepoints-for-gc",
      "-S",
      input_ll
        .to_str()
        .expect("temporary file paths should be UTF-8"),
      "-o",
      output_ll
        .to_str()
        .expect("temporary file paths should be UTF-8"),
    ],
  );
}

fn llc_to_obj(tmp: &Path, input_ll: &Path, output_obj: &Path, llc_flags: &[&str]) {
  let mut args = vec![
    "-filetype=obj",
    input_ll
      .to_str()
      .expect("temporary file paths should be UTF-8"),
    "-o",
    output_obj
      .to_str()
      .expect("temporary file paths should be UTF-8"),
  ];
  args.extend_from_slice(llc_flags);
  run_checked(tmp, LLVM_LLC, &args);
}

fn read_section(path: &Path, name: &str) -> Vec<u8> {
  let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("Failed to read {path:?}: {e}"));
  let obj = object::File::parse(&*bytes).expect("parse object file");
  let section = obj
    .section_by_name(name)
    .unwrap_or_else(|| panic!("Missing section `{name}` in {path:?}"));
  section.data().expect("read section data").to_vec()
}

fn has_register_roots(stackmap: &StackMap) -> bool {
  for rec in &stackmap.records {
    if rec.patchpoint_id != LLVM_STATEPOINT_PATCHPOINT_ID {
      continue;
    }
    if rec.locations.len() <= LLVM18_STATEPOINT_HEADER_CONSTANTS {
      continue;
    }
    for loc in &rec.locations[LLVM18_STATEPOINT_HEADER_CONSTANTS..] {
      if matches!(loc, Location::Register { .. }) {
        return true;
      }
    }
  }
  false
}

fn make_matrix_ir(max_roots: usize, add_pressure_variants: bool) -> String {
  // Keep the IR generation explicit and deterministic so failures can be
  // reproduced by re-running the same module through `opt-18` + `llc-18`.
  let mut out = String::new();
  out.push_str("; ModuleID = \"statepoint_register_roots_matrix\"\n");
  out.push_str("source_filename = \"statepoint_register_roots_matrix\"\n\n");
  out.push_str(&format!("target triple = \"{TARGET_TRIPLE}\"\n\n"));
  out.push_str("declare void @callee(i64)\n");
  out.push_str("@sink = global i64 0, align 8\n\n");

  for n_roots in 0..=max_roots {
    out.push_str(&make_one_function_ir(
      &format!("plain_{n_roots}"),
      n_roots,
      false,
    ));
    out.push('\n');
    if add_pressure_variants {
      out.push_str(&make_one_function_ir(
        &format!("pressure_{n_roots}"),
        n_roots,
        true,
      ));
      out.push('\n');
    }
  }

  out
}

fn make_one_function_ir(name: &str, n_roots: usize, add_pressure: bool) -> String {
  let mut out = String::new();

  out.push_str(&format!("define void @{name}("));
  out.push_str("i64 %seed");
  for i in 0..n_roots {
    out.push_str(&format!(", ptr addrspace(1) %p{i}"));
  }
  out.push_str(") gc \"coreclr\" {\n");
  out.push_str("entry:\n");

  if add_pressure {
    for i in 0..64 {
      out.push_str(&format!("  %v{i} = add i64 %seed, {i}\n"));
    }
  }

  out.push_str("  call void @callee(i64 1) [\"gc-live\"(");
  for i in 0..n_roots {
    if i > 0 {
      out.push_str(", ");
    }
    out.push_str(&format!("ptr addrspace(1) %p{i}"));
  }
  out.push_str(")]\n");

  if add_pressure {
    out.push_str("  %sum0 = add i64 %v0, %v1\n");
    for i in 2..64 {
      out.push_str(&format!(
        "  %sum{} = add i64 %sum{}, %v{}\n",
        i - 1,
        i - 2,
        i
      ));
    }
    out.push_str("  store volatile i64 %sum62, ptr @sink, align 8\n");
  }

  out.push_str("  ret void\n");
  out.push_str("}\n");
  out
}

fn path_in(dir: &Path, file: &str) -> PathBuf {
  dir.join(file)
}

#[test]
fn statepoint_register_roots_do_not_occur_in_supported_matrix() {
  assert_cmd_available(LLVM_OPT);
  assert_cmd_available(LLVM_LLC);

  let tmp = TempDir::new().expect("create tmpdir");
  let tmp = tmp.path();

  let input_ll = path_in(tmp, "matrix.ll");
  let rewritten_ll = path_in(tmp, "matrix.rewrite.ll");
  write_file(
    &input_ll,
    &make_matrix_ir(/*max_roots=*/ 64, /*add_pressure_variants=*/ true),
  );
  rewrite_statepoints(tmp, &input_ll, &rewritten_ll);

  #[derive(Clone, Copy)]
  struct LlcCfg {
    opt: &'static str,
    frame_pointer: Option<&'static str>,
    restrict_statepoint_remat: bool,
  }

  let cfgs: &[LlcCfg] = &[
    LlcCfg {
      opt: "-O0",
      frame_pointer: None,
      restrict_statepoint_remat: false,
    },
    LlcCfg {
      opt: "-O2",
      frame_pointer: None,
      restrict_statepoint_remat: false,
    },
    LlcCfg {
      opt: "-O3",
      frame_pointer: None,
      restrict_statepoint_remat: false,
    },
    LlcCfg {
      opt: "-O3",
      frame_pointer: Some("--frame-pointer=all"),
      restrict_statepoint_remat: false,
    },
    LlcCfg {
      opt: "-O3",
      frame_pointer: Some("--frame-pointer=all"),
      restrict_statepoint_remat: true,
    },
  ];

  for (cfg_idx, cfg) in cfgs.iter().enumerate() {
    let mut llc_flags = Vec::<&str>::new();
    llc_flags.push(cfg.opt);
    llc_flags.push(LCC_FIXUP_MAX_CSR_STATEPOINTS_0);
    if let Some(fp) = cfg.frame_pointer {
      llc_flags.push(fp);
    }
    if cfg.restrict_statepoint_remat {
      llc_flags.push("--restrict-statepoint-remat");
    }

    let obj = path_in(tmp, &format!("matrix_{cfg_idx}.o"));
    llc_to_obj(tmp, &rewritten_ll, &obj, &llc_flags);

    let section = read_section(&obj, ".llvm_stackmaps");
    let stackmap = StackMap::parse(&section).expect("parse stackmaps section");

    // Hard correctness check: verify the spill-to-stack convention holds.
    verify_statepoint_stackmap(
      &stackmap,
      VerifyStatepointOptions {
        arch: DwarfArch::X86_64,
        mode: VerifyMode::StatepointsOnly,
      },
    )
    .unwrap_or_else(|e| panic!("matrix cfg {cfg_idx} llc_flags={llc_flags:?}: {e}"));

    // Ensure we actually exercised the intended range of GC root counts.
    let mut seen_n = BTreeSet::<usize>::new();
    for rec in &stackmap.records {
      if rec.patchpoint_id != LLVM_STATEPOINT_PATCHPOINT_ID {
        continue;
      }
      let tail = rec.locations.len() - LLVM18_STATEPOINT_HEADER_CONSTANTS;
      assert_eq!(tail % 2, 0, "expected (base, derived) pairs in record");
      seen_n.insert(tail / 2);
    }
    let expected: BTreeSet<usize> = (0..=64).collect();
    assert_eq!(seen_n, expected, "matrix cfg {cfg_idx} missing root counts");
  }
}

#[test]
fn fixup_max_csr_statepoints_0_forces_spills() {
  assert_cmd_available(LLVM_OPT);
  assert_cmd_available(LLVM_LLC);

  let tmp = TempDir::new().expect("create tmpdir");
  let tmp = tmp.path();

  let input_ll = path_in(tmp, "haz.ll");
  let rewritten_ll = path_in(tmp, "haz.rewrite.ll");
  write_file(
    &input_ll,
    &make_matrix_ir(/*max_roots=*/ 6, /*add_pressure_variants=*/ false),
  );
  rewrite_statepoints(tmp, &input_ll, &rewritten_ll);

  // Unsafe flags: allow keeping some statepoint roots in callee-saved registers.
  let dangerous_flags = &[
    "-O3",
    "--fixup-allow-gcptr-in-csr",
    "--max-registers-for-gc-values=100",
  ];

  let dangerous_obj = path_in(tmp, "haz_dangerous.o");
  llc_to_obj(tmp, &rewritten_ll, &dangerous_obj, dangerous_flags);
  let dangerous_section = read_section(&dangerous_obj, ".llvm_stackmaps");
  let dangerous_sm = StackMap::parse(&dangerous_section).expect("parse dangerous stackmaps");
  assert!(
    has_register_roots(&dangerous_sm),
    "expected dangerous flags={dangerous_flags:?} to produce at least one Register root"
  );
  assert!(
    verify_statepoint_stackmap(
      &dangerous_sm,
      VerifyStatepointOptions {
        arch: DwarfArch::X86_64,
        mode: VerifyMode::StatepointsOnly,
      },
    )
    .is_err(),
    "expected verifier to reject register roots under dangerous flags"
  );

  // Mitigated flags: force spills back to stack slots.
  let mitigated_flags = &[
    "-O3",
    "--fixup-allow-gcptr-in-csr",
    "--max-registers-for-gc-values=100",
    LCC_FIXUP_MAX_CSR_STATEPOINTS_0,
  ];

  let mitigated_obj = path_in(tmp, "haz_mitigated.o");
  llc_to_obj(tmp, &rewritten_ll, &mitigated_obj, mitigated_flags);
  let mitigated_section = read_section(&mitigated_obj, ".llvm_stackmaps");
  let mitigated_sm = StackMap::parse(&mitigated_section).expect("parse mitigated stackmaps");
  assert!(
    !has_register_roots(&mitigated_sm),
    "did not expect Register roots under mitigated flags={mitigated_flags:?}"
  );
  verify_statepoint_stackmap(
    &mitigated_sm,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}
