use std::{env, error::Error, fs, path::PathBuf, process};

use llvm_stackmaps::{elf, StackMaps, StatepointRecordView};

fn usage() -> ! {
    eprintln!("usage: dump_stackmaps <path-to-elf> [--pc 0xADDR]");
    process::exit(2);
}

fn parse_pc(arg: &str) -> Result<u64, Box<dyn Error>> {
    let s = arg.strip_prefix("0x").unwrap_or(arg);
    Ok(u64::from_str_radix(s, 16)?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        usage();
    };
    let path = PathBuf::from(path);

    let mut pc: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pc" => {
                let Some(v) = args.next() else {
                    return Err("--pc expects a value".into());
                };
                pc = Some(parse_pc(&v)?);
            }
            _ if arg.starts_with("--pc=") => {
                let v = arg.trim_start_matches("--pc=");
                pc = Some(parse_pc(v)?);
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }

    let file = fs::read(&path)?;
    let section = elf::stackmaps_section_bytes(&file)?;
    let maps = StackMaps::parse(section)?;

    println!(
        "StackMap v{}: {} functions, {} constants, {} records",
        maps.header.version,
        maps.header.num_functions,
        maps.header.num_constants,
        maps.header.num_records
    );

    for (i, callsite) in maps.callsites().iter().enumerate() {
        let rec = &maps.records[callsite.record_index];
        println!(
            "[{i}] pc=0x{:x} func=0x{:x} stack_size={} id={} locations={} live_outs={}",
            callsite.pc,
            callsite.function_address,
            callsite.stack_size,
            rec.id,
            rec.locations.len(),
            rec.live_outs.len()
        );
    }

    if let Some(pc) = pc {
        println!();
        println!("Lookup pc=0x{pc:x}:");
        let Some(callsite) = maps.lookup_callsite(pc) else {
            println!("  no record");
            return Ok(());
        };
        let rec = &maps.records[callsite.record_index];

        println!(
            "  record id={} instruction_offset={} func=0x{:x} stack_size={}",
            rec.id, rec.instruction_offset, callsite.function_address, callsite.stack_size
        );
        println!("  locations:");
        for (i, loc) in rec.locations().iter().enumerate() {
            println!("    #{:>2} {}", i + 1, loc);
        }

        if let Some(sp) = StatepointRecordView::decode(rec) {
            println!();
            println!("  statepoint:");
            println!("    call_conv={} flags={}", sp.call_conv, sp.flags);
            println!("    deopt_args={} gc_roots={}", sp.deopt_args.len(), sp.num_gc_roots());

            if !sp.deopt_args.is_empty() {
                println!("    deopt:");
                for (i, loc) in sp.deopt_args.iter().enumerate() {
                    println!("      deopt[{i}] {loc}");
                }
            }

            for (i, pair) in sp.gc_root_pairs().enumerate() {
                println!("    root[{i}] base={} derived={}", pair.base, pair.derived);
            }
        } else {
            println!();
            println!("  record does not match expected statepoint layout");
        }
    }

    Ok(())
}
