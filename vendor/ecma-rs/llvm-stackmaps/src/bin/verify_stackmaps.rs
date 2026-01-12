use std::{env, fs, path::PathBuf, process};

use llvm_stackmaps::{elf, verify, verify::VerifyOptions};

fn usage() -> ! {
    eprintln!("usage: verify_stackmaps (--elf <path-to-elf> | --raw <path-to-llvm_stackmaps-bytes>)");
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
                Ok(section) => verify::verify_stackmaps_bytes(section, VerifyOptions::default()),
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

    print!("{}", report.to_json());

    if report.ok() {
        process::exit(0);
    } else {
        process::exit(1);
    }
}

