use std::process;
use vm_js::{
  Agent, Budget, HeapLimits, LoadedModuleRequest, ModuleGraph, ModuleRequest, ModuleStatus,
  PropertyDescriptor, PropertyKey, PropertyKind, SourceTextModuleRecord, Value, VmError, VmOptions,
  MAX_PROTOTYPE_CHAIN,
};

fn usage() -> ! {
  eprintln!("usage: oom_harness <scenario> <len_code_units> <filler_bytes>");
  eprintln!(
    "  scenario: eval | function | generator | number | parseFloat | regexp_compile | regexp | arrayMap | throw_string_format | getPrototypeOf_proxy_chain | setPrototypeOf_cycle_check | moduleLink"
  );
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

  fn exhaust_address_space() -> Vec<Vec<u8>> {
    // Allocate blocks of decreasing size until further allocations fail. This is used to drive
    // `HashSet` growth paths into allocator OOM without aborting the process.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    // Pre-reserve space for block headers while memory is still available, so `push` cannot abort
    // due to an infallible internal reallocation when we are near the RLIMIT_AS ceiling.
    let _ = blocks.try_reserve(1024);
    let mut chunk = 8 * 1024 * 1024usize;
    while chunk >= 16 {
      loop {
        let mut block: Vec<u8> = Vec::new();
        if block.try_reserve_exact(chunk).is_err() {
          break;
        }
        block.resize(chunk, 0);
        if blocks.try_reserve(1).is_err() {
          // Keep the block alive even if we can't record it.
          std::mem::forget(block);
          return blocks;
        }
        blocks.push(block);
      }
      chunk /= 2;
    }
    blocks
  }

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

  if scenario == "moduleLink" {
    // Allocate a very large module specifier in host memory. Previously `ModuleGraph::link_inner`
    // used `format!(...)` to build error messages containing attacker-controlled strings like
    // module specifiers; that infallibly allocates and can abort the process under allocator OOM.
    //
    // This scenario triggers an indirect-export resolution failure during module linking while the
    // process is under a tight RLIMIT_AS, and asserts we exit cleanly (either `VmError::OutOfMemory`
    // or a thrown `SyntaxError`).
    let alloc_ascii_string = |len: usize| -> Option<String> {
      let mut bytes: Vec<u8> = Vec::new();
      bytes.try_reserve_exact(len).ok()?;
      bytes.resize(len, b'a');
      // Safety: bytes are ASCII.
      Some(unsafe { String::from_utf8_unchecked(bytes) })
    };

    let Some(spec1) = alloc_ascii_string(len_code_units) else {
      eprintln!("oom_harness: failed to allocate large module specifier (entry)");
      process::exit(1);
    };
    let Some(spec2) = alloc_ascii_string(len_code_units) else {
      eprintln!("oom_harness: failed to allocate large module specifier (loaded_modules)");
      process::exit(1);
    };

    let mut graph = ModuleGraph::new();

    let imported = match SourceTextModuleRecord::parse(
      agent.heap_mut(),
      // Provide at least one export so `ResolveExport` has work to do, but not the name we re-export.
      "export const other = 1;",
    ) {
      Ok(mut rec) => {
        rec.status = ModuleStatus::Unlinked;
        rec
      }
      Err(err) => {
        eprintln!("oom_harness: failed to parse imported module: {err:?}");
        process::exit(1);
      }
    };
    let imported_id = graph.add_module(imported);

    let mut root = match SourceTextModuleRecord::parse(
      agent.heap_mut(),
      // Re-export a missing name so linking throws a SyntaxError with a message containing the
      // (attacker-controlled) module specifier.
      "export { missing as x } from 'm';",
    ) {
      Ok(mut rec) => {
        rec.status = ModuleStatus::Unlinked;
        rec
      }
      Err(err) => {
        eprintln!("oom_harness: failed to parse root module: {err:?}");
        process::exit(1);
      }
    };

    // Avoid linking dependency edges from the original parsed specifier (`'m'`). We explicitly
    // supply `[[LoadedModules]]` below for the mutated specifier value.
    root.requested_modules.clear();

    let Some(entry) = root.indirect_export_entries.get_mut(0) else {
      eprintln!("oom_harness: parsed root module did not produce an indirect export entry");
      process::exit(1);
    };
    entry.module_request.specifier = spec1;

    root.loaded_modules = vec![LoadedModuleRequest::new(
      ModuleRequest::new(spec2, Vec::new()),
      imported_id,
    )];

    let root_id = graph.add_module(root);

    let (vm, realm, heap) = agent.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let result = graph.link(vm, heap, global, root_id);
    match result {
      Ok(()) => {
        eprintln!("oom_harness: unexpected success in moduleLink scenario");
        process::exit(1);
      }
      Err(VmError::OutOfMemory) => process::exit(0),
      Err(VmError::Throw(_) | VmError::ThrowWithStack { .. }) => process::exit(0),
      Err(err) => {
        eprintln!("oom_harness: unexpected error in moduleLink scenario: {err:?}");
        process::exit(1);
      }
    }
  }

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

  let result = match scenario.as_str() {
    "getPrototypeOf_proxy_chain" => {
      // Create a Proxy so `Heap::object_prototype` traverses a proxy chain and uses a `HashSet`
      // for cycle detection.
      let proxy = {
        let mut scope = agent.heap_mut().scope();
        let handler = match scope.alloc_object() {
          Ok(o) => o,
          Err(err) => {
            eprintln!("oom_harness: failed to allocate proxy handler: {err:?}");
            process::exit(1);
          }
        };
        let mut target = match scope.alloc_object() {
          Ok(o) => o,
          Err(err) => {
            eprintln!("oom_harness: failed to allocate proxy target: {err:?}");
            process::exit(1);
          }
        };

        // Build a deep proxy chain so the visited `HashSet` needs to grow enough to allocate a
        // large backing table (more likely to hit allocator OOM even when small free-lists exist).
        for _ in 0..MAX_PROTOTYPE_CHAIN {
          target = match scope.alloc_proxy(Some(target), Some(handler)) {
            Ok(p) => p,
            Err(err) => {
              eprintln!("oom_harness: failed to allocate proxy: {err:?}");
              process::exit(1);
            }
          };
        }
        target
      };

      // Consume remaining address space so the `HashSet` inside `object_prototype` hits allocator
      // OOM.
      let pressure = exhaust_address_space();
      let _keep = (&filler, &pressure);

      match agent.heap_mut().object_prototype(proxy) {
        Ok(_) => Ok(Value::Undefined),
        Err(err) => Err(err),
      }
    }
    "setPrototypeOf_cycle_check" => {
      // Create ordinary objects so `Heap::object_set_prototype` walks the prototype chain and uses
      // a `HashSet` for cycle detection.
      let (obj, proto) = {
        let mut scope = agent.heap_mut().scope();
        let obj = match scope.alloc_object() {
          Ok(o) => o,
          Err(err) => {
            eprintln!("oom_harness: failed to allocate object: {err:?}");
            process::exit(1);
          }
        };
        let proto = match scope.alloc_object() {
          Ok(o) => o,
          Err(err) => {
            eprintln!("oom_harness: failed to allocate prototype object: {err:?}");
            process::exit(1);
          }
        };

        // Create a deep prototype chain so `object_set_prototype` has to insert many entries into
        // its visited `HashSet` during cycle detection.
        //
        // Use the unchecked setter to avoid O(N^2) work while building the chain.
        let mut tail = proto;
        for _ in 1..MAX_PROTOTYPE_CHAIN {
          let next = match scope.alloc_object() {
            Ok(o) => o,
            Err(err) => {
              eprintln!("oom_harness: failed to allocate prototype chain node: {err:?}");
              process::exit(1);
            }
          };
          if unsafe { scope.heap_mut().object_set_prototype_unchecked(tail, Some(next)) }.is_err() {
            eprintln!("oom_harness: failed to link prototype chain");
            process::exit(1);
          }
          tail = next;
        }
        (obj, proto)
      };

      // Consume remaining address space so the `HashSet` inside `object_set_prototype` hits
      // allocator OOM.
      let pressure = exhaust_address_space();
      let _keep = (&filler, &pressure);

      match agent.heap_mut().object_set_prototype(obj, Some(proto)) {
        Ok(()) => Ok(Value::Undefined),
        Err(err) => Err(err),
      }
    }
    "eval"
    | "function"
    | "generator"
    | "number"
    | "parseFloat"
    | "regexp_compile"
    | "regexp"
    | "arrayMap"
    | "throw_string_format" => {
      let script = match scenario.as_str() {
        "eval" => "eval(S)",
        "function" => "Function(S, 'return 1;')",
        "generator" => "Object.getPrototypeOf(function*(){}).constructor(S)",
        "number" => "Number(S)",
        "parseFloat" => "parseFloat(S)",
        "regexp_compile" | "regexp" => "new RegExp(S)",
        // Exercise host-visible error formatting (`Agent::format_vm_error`) on a thrown string value
        // that is too large to stringify under RLIMIT_AS pressure.
        "throw_string_format" => "throw S",
        // Trigger per-iteration array index key formatting (`ToString(k)` for each `k < length`) under
        // memory pressure. Previously this used intermediate Rust heap `String` allocations, which can
        // abort the process on allocator OOM.
        "arrayMap" => "Array(S.length).map(() => 0)",
        _ => unreachable!(),
      };

      // Keep `filler` alive for the duration of the run.
      let _keep = &filler;

      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    other => {
      eprintln!("oom_harness: unknown scenario: {other}");
      process::exit(2);
    }
  };

  if scenario == "throw_string_format" {
    match &result {
      Ok(v) => {
        eprintln!("oom_harness: unexpected success: {v:?}");
        process::exit(1);
      }
      Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
        // Force host formatting of the thrown value/stack under memory pressure. The important
        // invariant is that this must not abort the process (it is allowed to return a placeholder
        // string if it cannot allocate).
        let _ = agent.format_vm_error(err);
        process::exit(0);
      }
      Err(err) => {
        eprintln!("oom_harness: unexpected error: {err:?}");
        process::exit(1);
      }
    }
  }

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
