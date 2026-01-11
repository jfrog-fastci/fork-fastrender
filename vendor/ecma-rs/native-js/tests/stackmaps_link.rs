#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{fs, process::Command};

use anyhow::{anyhow, Context as _, Result};
use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target};
use inkwell::OptimizationLevel;
use object::{Object as _, ObjectSection as _};

use native_js::link::LinkOpts;
use native_js::{emit, llvm::gc};

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

    Target::initialize_native(&InitializationConfig::default())
        .expect("failed to initialize native LLVM target");

    // Build a small statepoint/stackmap PoC module:
    // - `main` calls `test`.
    // - `test` is `gc \"coreclr\"` and keeps a GC pointer live across a call, which forces
    //   statepoint rewriting and `.llvm_stackmaps` emission.
    let context = Context::create();
    let module = context.create_module("stackmaps_link");
    let builder = context.create_builder();

    let gc_ptr = gc::gc_ptr_type(&context);

    let callee_ty = context.void_type().fn_type(&[], false);
    let callee = module.add_function("callee", callee_ty, None);
    let callee_entry = context.append_basic_block(callee, "entry");
    builder.position_at_end(callee_entry);
    builder.build_return(None).unwrap();

    let test_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
    let test_fn = module.add_function("test", test_ty, None);
    gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

    let test_entry = context.append_basic_block(test_fn, "entry");
    builder.position_at_end(test_entry);
    builder.build_call(callee, &[], "call_callee").unwrap();
    let arg0 = test_fn
        .get_first_param()
        .expect("missing arg0")
        .into_pointer_value();
    builder.build_return(Some(&arg0)).unwrap();

    let main_ty = context.i32_type().fn_type(&[], false);
    let main_fn = module.add_function("main", main_ty, None);
    let main_entry = context.append_basic_block(main_fn, "entry");
    builder.position_at_end(main_entry);
    builder
        .build_call(test_fn, &[gc_ptr.const_null().into()], "call_test")
        .unwrap();
    builder
        .build_return(Some(&context.i32_type().const_int(0, false)))
        .unwrap();

    let mut target = emit::TargetConfig::default();
    target.cpu = "generic".to_string();
    target.features = "".to_string();
    target.opt_level = OptimizationLevel::None;
    target.reloc_mode = RelocMode::Default;
    target.code_model = CodeModel::Default;

    let obj_bytes = emit::emit_object_with_statepoints(&module, target)
        .context("emit object with statepoints")?;

    assert_section_present_non_empty(&obj_bytes, ".llvm_stackmaps")?;
    assert_any_section_present_non_empty(&obj_bytes, &[".rela.llvm_stackmaps", ".rel.llvm_stackmaps"])?;

    let tmp = tempfile::tempdir().context("create tempdir")?;
    let exe_path = tmp.path().join("poc_exe");

    native_js::link::link_object_buffers_to_elf_executable(&exe_path, &[obj_bytes.as_slice()], LinkOpts::default())?;

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

    let status = Command::new(&exe_path)
        .status()
        .with_context(|| format!("run {}", exe_path.display()))?;
    if !status.success() {
        return Err(anyhow!("linked executable failed with status {status}"));
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

fn assert_any_section_present_non_empty(bytes: &[u8], names: &[&str]) -> Result<()> {
    for name in names {
        if assert_section_present_non_empty(bytes, name).is_ok() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "expected one of the following sections to exist and be non-empty: {names:?}"
    ))
}

fn assert_section_absent(bytes: &[u8], name: &str) -> Result<()> {
    let file = object::File::parse(bytes).context("parse object/elf")?;
    if file.section_by_name(name).is_some() {
        return Err(anyhow!("expected section {name} to be absent"));
    }
    Ok(())
}
