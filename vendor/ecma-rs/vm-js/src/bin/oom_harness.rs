use vm_js::{
  Agent, Budget, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError, VmOptions,
};
use std::process;

fn usage() -> ! {
  eprintln!("usage: oom_harness <scenario> <len_code_units> <filler_bytes>");
  eprintln!("  scenario: eval | function | number | parseFloat");
  process::exit(2);
}

fn parse_usize(arg: Option<String>) -> usize {
  arg
    .as_deref()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or_else(|| usage())
}

fn main() {
  let mut args = std::env::args().skip(1);
  let Some(scenario) = args.next() else {
    usage();
  };
  let len_code_units = parse_usize(args.next());
  let filler_bytes = parse_usize(args.next());

  // Allocate a large filler buffer to reduce available headroom under a process-wide RLIMIT_AS. The
  // tests drive this harness under a small address-space limit so attacker-triggered allocations
  // (UTF-16→UTF-8 conversion, Function constructor source building, etc) fail with a *recoverable*
  // `VmError::OutOfMemory` rather than aborting the process.
  let mut filler: Vec<u8> = Vec::new();
  if filler_bytes != 0 {
    if filler.try_reserve_exact(filler_bytes).is_err() {
      eprintln!("oom_harness: failed to allocate filler buffer");
      process::exit(1);
    }
    filler.resize(filler_bytes, 0);
  }

  let mut agent = match Agent::with_options(
    VmOptions::default(),
    // Use very large heap limits: this harness is intentionally driven by the OS address-space
    // limit (RLIMIT_AS), not by the VM heap accounting, so we can exercise fallible host-side
    // allocations.
    HeapLimits::new(1024 * 1024 * 1024, 1024 * 1024 * 1024),
  ) {
    Ok(agent) => agent,
    Err(err) => {
      eprintln!("oom_harness: failed to create agent: {err:?}");
      process::exit(1);
    }
  };

  // Create a large heap string and install it as `globalThis.S` so the script itself can be tiny.
  let fill_unit = match scenario.as_str() {
    "parseFloat" => b'1' as u16,
    // Use U+0800 so UTF-8 encoding expands to 3 bytes per code unit. This allows triggering
    // host-side allocation failures with smaller UTF-16 strings.
    _ => 0x0800u16,
  };

  let s = {
    let mut units: Vec<u16> = Vec::new();
    if units.try_reserve_exact(len_code_units).is_err() {
      eprintln!("oom_harness: failed to allocate input string buffer");
      process::exit(1);
    }
    units.resize(len_code_units, fill_unit);

    let mut scope = agent.heap_mut().scope();
    match scope.alloc_string_from_u16_vec(units) {
      Ok(s) => s,
      Err(err) => {
        eprintln!("oom_harness: failed to allocate heap string: {err:?}");
        process::exit(1);
      }
    }
  };

  {
    let global = agent.realm().global_object();
    let mut scope = agent.heap_mut().scope();
    if scope.push_roots(&[Value::Object(global), Value::String(s)]).is_err() {
      eprintln!("oom_harness: failed to root inputs");
      process::exit(1);
    }

    let key_s = match scope.alloc_string("S") {
      Ok(k) => k,
      Err(err) => {
        eprintln!("oom_harness: failed to allocate global key string: {err:?}");
        process::exit(1);
      }
    };
    if scope.push_root(Value::String(key_s)).is_err() {
      eprintln!("oom_harness: failed to root global key string");
      process::exit(1);
    }

    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(s),
        writable: true,
      },
    };
    let key = PropertyKey::from_string(key_s);
    if scope.define_property(global, key, desc).is_err() {
      eprintln!("oom_harness: failed to define global property");
      process::exit(1);
    }
  }

  let script = match scenario.as_str() {
    "eval" => "eval(S)",
    "function" => "Function(S, 'return 1;')",
    "number" => "Number(S)",
    "parseFloat" => "parseFloat(S)",
    other => {
      eprintln!("oom_harness: unknown scenario: {other}");
      process::exit(2);
    }
  };

  // Keep `filler` alive for the duration of the run.
  let _keep = &filler;

  let result = agent.run_script("oom_harness.js", script, Budget::unlimited(1), None);
  match result {
    Err(VmError::OutOfMemory) => process::exit(0),
    // `parseFloat` is allowed to succeed here: upstream implementations may parse directly from
    // UTF-16 without allocating an intermediate `String`. The key invariant is that it must not
    // abort the process under memory pressure.
    Ok(_) if scenario == "parseFloat" || scenario == "number" => process::exit(0),
    Ok(v) => {
      eprintln!("oom_harness: unexpected success: {v:?}");
      process::exit(1);
    }
    Err(err) => {
      eprintln!("oom_harness: unexpected error: {err:?}");
      process::exit(1);
    }
  }
}
