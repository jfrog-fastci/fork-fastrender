use std::{env, fs, path::PathBuf, process};

use llvm_stackmaps::{elf, verify, verify::VerifyOptions};

fn usage() -> ! {
    eprintln!(
        "usage: verify_stackmaps (--elf <path-to-elf> | --raw <path-to-llvm_stackmaps-bytes>)\n\
         \n\
         - Writes a human summary to stderr.\n\
         - Writes a deterministic JSON report to stdout.\n\
         - Exits non-zero on verification failure."
    );
    process::exit(2);
}

enum InputKind {
    Elf,
    Raw,
}

fn main() {
    let mut args = env::args().skip(1);

    let mut input_kind: Option<InputKind> = None;
    let mut path: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => usage(),
            "--elf" => {
                input_kind = Some(InputKind::Elf);
                let Some(p) = args.next() else { usage() };
                path = Some(PathBuf::from(p));
            }
            "--raw" => {
                input_kind = Some(InputKind::Raw);
                let Some(p) = args.next() else { usage() };
                path = Some(PathBuf::from(p));
            }
            _ if arg.starts_with("--") => usage(),
            _ => {
                // Back-compat convenience: `verify_stackmaps <path>` defaults to ELF mode.
                if path.is_some() {
                    usage();
                }
                input_kind.get_or_insert(InputKind::Elf);
                path = Some(PathBuf::from(arg));
            }
        }
    }

    let Some(path) = path else { usage() };
    let input_kind = input_kind.unwrap_or(InputKind::Elf);

    let report = match fs::read(&path) {
        Ok(file) => match input_kind {
            InputKind::Elf => match elf::stackmaps_section_bytes(&file) {
                Ok(section) => {
                    // If we can infer the target arch from the ELF header, use it for DWARF
                    // register-policy checks (e.g. SP/FP base regs for Indirect roots).
                    let inferred_arch = verify::infer_arch_from_elf_header(&file)
                        .unwrap_or(verify::TargetArch::host());
                    let opts = VerifyOptions {
                        arch: inferred_arch,
                        ..VerifyOptions::default()
                    };
                    verify::verify_stackmaps_bytes(section, opts)
                }
                Err(e) => verify::VerificationReport {
                    functions: 0,
                    constants: 0,
                    records: 0,
                    callsites: 0,
                    decoded_statepoints: 0,
                    failures: vec![verify::VerificationFailure {
                        kind: "elf_error",
                        message: e.to_string(),
                        offset: None,
                        pc: None,
                        function_address: None,
                        record_index: None,
                    }],
                },
            },
            InputKind::Raw => verify::verify_stackmaps_bytes(&file, VerifyOptions::default()),
        },
        Err(e) => verify::VerificationReport {
            functions: 0,
            constants: 0,
            records: 0,
            callsites: 0,
            decoded_statepoints: 0,
            failures: vec![verify::VerificationFailure {
                kind: "io_error",
                message: e.to_string(),
                offset: None,
                pc: None,
                function_address: None,
                record_index: None,
            }],
        },
    };

    // Human summary for interactive use.
    if report.ok() {
        eprintln!(
            "OK: {} functions, {} constants, {} records, {} callsites, {} decoded statepoints",
            report.functions,
            report.constants,
            report.records,
            report.callsites,
            report.decoded_statepoints
        );
    } else {
        eprintln!(
            "FAIL: {} failure(s) ({} records, {} callsites)",
            report.failures.len(),
            report.records,
            report.callsites
        );
        for f in &report.failures {
            eprintln!("  - {}: {}", f.kind, f.message);
        }
    }

    print!("{}", report.to_json());

    if report.ok() {
        process::exit(0);
    } else {
        process::exit(1);
    }
}
