use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use parse_js::lex::{lex_next, LexMode, Lexer as ParseLexer};
use parse_js::{Dialect as ParseDialect, SourceType as ParseSourceType};
use vm_js::{
  format_stack_trace, job_queue::JobQueue, Agent, Budget, HeapLimits, Job, JobKind,
  JsString, LoadedModuleRequest, MicrotaskQueue,
  ModuleGraph, ModuleId, ModuleRequest, ModuleStatus, PropertyDescriptor, PropertyKey, PropertyKind,
  RootId, SourceTextModuleRecord, StackFrame, Value, VmError, VmHostHooks, VmJobContext, VmOptions,
  MAX_PROTOTYPE_CHAIN,
};

static MICROTASK_ERRORS_REMAINING: AtomicUsize = AtomicUsize::new(0);

fn microtask_job_erroring(
  _ctx: &mut dyn VmJobContext,
  host: &mut dyn VmHostHooks,
) -> Result<(), VmError> {
  let remaining = MICROTASK_ERRORS_REMAINING.fetch_sub(1, Ordering::Relaxed);
  if remaining > 1 {
    let job = Job::new(JobKind::Promise, microtask_job_erroring)?;
    host.host_enqueue_promise_job_fallible(_ctx, job, None)?;
  }
  Err(VmError::TypeError("microtask job error"))
}

struct DummyJobContext;

impl VmJobContext for DummyJobContext {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("oom_harness: DummyJobContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("oom_harness: DummyJobContext::construct"))
  }

  fn add_root(&mut self, _value: Value) -> Result<RootId, VmError> {
    Err(VmError::Unimplemented("oom_harness: DummyJobContext::add_root"))
  }

  fn remove_root(&mut self, _id: RootId) {
  }
}

fn usage() -> ! {
  eprintln!("usage: oom_harness <scenario> <len_code_units> <filler_bytes>");
  eprintln!(
    "  scenario: eval | function | generator | generator_invoke | number | parseFloat | regexp_compile | regexp | arrayMap | allocStringU16SpareCap | jobQueue | jobCallback | stackTrace | throw_string_format | getPrototypeOf_proxy_chain | setPrototypeOf_cycle_check | moduleLink | moduleGraph | labelEarlyError | globalVarDecl | microtask_checkpoint_errors | moduleGetExportedNames | captureStack | internalPromiseReactions | promiseJob | generatorInstance | register_ecma_function"
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

  // Warm up `parse-js`'s lexer tables before we allocate large filler buffers.
  //
  // `parse-js` uses `once_cell::sync::Lazy` to build Aho-Corasick DFAs the first time a script is
  // lexed. Those DFAs are created via infallible allocations inside `aho_corasick`, which can abort
  // the process under a tight process-wide RLIMIT_AS.
  //
  // The OOM regression harness intentionally allocates large buffers to reduce available headroom
  // *after* the VM and its dependencies have initialized. Ensure `parse-js` has performed its
  // one-time lexer initialization while memory is still available so we can reliably reach the
  // target OOM paths for each scenario.
  {
    let mut lexer = ParseLexer::new("/*warmup*/0");
    loop {
      let tok = lex_next(
        &mut lexer,
        LexMode::Standard,
        ParseDialect::Ecma,
        ParseSourceType::Script,
      );
      if matches!(tok.typ, parse_js::token::TT::EOF) {
        break;
      }
    }
  }

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

  if scenario == "jobQueue" {
    // Keep `filler` alive for the duration of the run.
    let _keep = &filler;

    let mut queue = JobQueue::new();
    let mut ctx = DummyJobContext;
    let mut count: usize = 0;
    // Bound the loop to avoid accidental infinite runtime if the address-space limit isn't enforced
    // for some reason.
    const MAX_JOBS: usize = 10_000_000;
    while count < MAX_JOBS {
      // Use a capture-less closure so `Job::new` does not allocate a heap box for it (ZST).
      let job = match Job::new(JobKind::Promise, |_ctx, _host| Ok(())) {
        Ok(job) => job,
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: failed to allocate job: {err:?}");
          process::exit(1);
        }
      };
      match queue.try_push(&mut ctx, job) {
        Ok(()) => count += 1,
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!(
            "oom_harness: unexpected error from JobQueue::try_push after {count} jobs: {err:?}"
          );
          process::exit(1);
        }
      }
    }

    // If we somehow didn't OOM, treat that as success: the key invariant is that we didn't abort.
    process::exit(0);
  }

  if scenario == "jobCallback" {
    // Keep `filler` alive for the duration of the run.
    let _keep = &filler;

    // Stress the default `VmHostHooks::host_make_job_callback` implementation under an OS
    // address-space limit. This hook must be fallible: allocator OOM should surface as
    // `VmError::OutOfMemory` rather than panicking/aborting.
    struct NoopHost;

    impl vm_js::VmHostHooks for NoopHost {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}
    }

    let mut host = NoopHost;

    // Create a callback object handle to store in each JobCallback record.
    let mut heap = vm_js::Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let callback_obj = {
      let mut scope = heap.scope();
      match scope.alloc_object() {
        Ok(o) => o,
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: failed to allocate callback object: {err:?}");
          process::exit(1);
        }
      }
    };

    // Allocate JobCallback records until we hit allocator OOM. Leak each record to ensure memory is
    // not reclaimed.
    const MAX_CALLBACKS: usize = 100_000_000;
    for i in 0..MAX_CALLBACKS {
      match host.host_make_job_callback(callback_obj) {
        Ok(cb) => std::mem::forget(cb),
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: unexpected error after {i} JobCallback allocations: {err:?}");
          process::exit(1);
        }
      }
    }

    // If we somehow didn't OOM, treat that as success: the key invariant is that we didn't abort.
    process::exit(0);
  }

  if scenario == "microtask_checkpoint_errors" {
    // Keep `filler` alive for the duration of the run.
    let _keep = &filler;

    // Run a large number of jobs that each return an error. `MicrotaskQueue::perform_microtask_checkpoint`
    // collects errors into a `Vec`, which must grow fallibly under allocator OOM.
    MICROTASK_ERRORS_REMAINING.store(len_code_units, Ordering::Relaxed);

    let mut queue = MicrotaskQueue::new();
    let mut ctx = DummyJobContext;
    if len_code_units != 0 {
      let job = match Job::new(JobKind::Promise, microtask_job_erroring) {
        Ok(job) => job,
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: failed to allocate microtask job: {err:?}");
          process::exit(1);
        }
      };
      match queue.host_enqueue_promise_job_fallible(&mut ctx, job, None) {
        Ok(()) => {}
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: unexpected error enqueuing microtask job: {err:?}");
          process::exit(1);
        }
      }
    }

    let _errors = queue.perform_microtask_checkpoint(&mut ctx);

    if !queue.is_empty() {
      eprintln!("oom_harness: microtask queue not empty after checkpoint");
      process::exit(1);
    }

    process::exit(0);
  }

  if scenario == "moduleGraph" {
    // Stress `ModuleGraph::{add_module,add_module_with_specifier}` under memory pressure. This is
    // attacker-reachable via dynamic `import()` and host module loading; fallible graph growth must
    // report `VmError::OutOfMemory` rather than aborting on allocator OOM.
    let specifier_len = len_code_units;

    // Build a reusable specifier string (each module registration clones it into an owned
    // `ModuleRequest`).
    let mut specifier = String::new();
    if specifier_len != 0 {
      if specifier.try_reserve_exact(specifier_len).is_err() {
        eprintln!("oom_harness: failed to allocate module specifier string");
        process::exit(1);
      }
      for _ in 0..specifier_len {
        specifier.push('a');
      }
    }

    let mut graph = ModuleGraph::new();
    // Keep `filler` alive for the duration of the run.
    let _keep = &filler;

    // Add modules until we hit OOM. Bound the loop to avoid accidental infinite runtime if the
    // address-space limit isn't enforced for some reason.
    const MAX_MODULES: usize = 1_000_000;
    for _ in 0..MAX_MODULES {
      match graph.add_module_with_specifier(&specifier, SourceTextModuleRecord::default()) {
        Ok(_id) => {}
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: unexpected error from ModuleGraph growth: {err:?}");
          process::exit(1);
        }
      }
    }

    // If we somehow didn't OOM, treat that as success: the key invariant is that we didn't abort.
    process::exit(0);
  }

  if scenario == "internalPromiseReactions" {
    // Attach many async/await-style reactions to a pending *engine-internal* Promise record until
    // allocator OOM. This exercises fallible reaction-list growth in `promise.rs`.
    //
    // Keep `filler` alive for the duration of the run so we stay close to the RLIMIT_AS ceiling.
    let _keep = &filler;

    struct NoopHost;

    impl vm_js::VmHostHooks for NoopHost {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}
    }

    let mut host = NoopHost;
    // Use small heap limits: the internal Promise record allocates on the Rust heap; this Heap is
    // only needed to satisfy `await_value`'s signature.
    let mut heap = vm_js::Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let promise = vm_js::Promise::pending(None);
    let awaitable = vm_js::Awaitable::Promise(promise);

    const MAX_ITERS: usize = 50_000_000;
    for i in 0..MAX_ITERS {
      match vm_js::await_value(&mut host, &mut heap, awaitable.clone(), Value::Undefined, Value::Undefined) {
        Ok(()) => {}
        Err(VmError::OutOfMemory) => process::exit(0),
        Err(err) => {
          eprintln!("oom_harness: unexpected error after {i} await_value calls: {err:?}");
          process::exit(1);
        }
      }
    }

    eprintln!("oom_harness: internalPromiseReactions did not hit OOM within {MAX_ITERS} iterations");
    process::exit(1);
  }

  let mut vm_options = VmOptions::default();
  if scenario == "captureStack" {
    // Allow pre-filling the VM stack with many frames to force `capture_stack` to allocate a large
    // trace vector under memory pressure.
    vm_options.max_stack_depth = 5_000_000;
  }

  let mut agent = match Agent::with_options(
    vm_options,
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

  if scenario == "allocStringU16SpareCap" {
    // Construct a UTF-16 buffer with `capacity() > len()` (via non-exact `try_reserve`), then
    // convert it into a heap `JsString` under RLIMIT_AS pressure. This exercises the
    // `alloc_string_from_u16_vec` path when it needs to trim spare capacity, ensuring it never
    // relies on infallible reallocations that could abort the process on OOM.
    let _keep = &filler;

    let mut units: Vec<u16> = Vec::new();
    let reserve = len_code_units.checked_add(1).unwrap_or_else(|| usage());
    if units.try_reserve(reserve).is_err() {
      eprintln!("oom_harness: failed to allocate input string buffer");
      process::exit(1);
    }
    units.resize(len_code_units, 0x0800u16);

    let mut scope = agent.heap_mut().scope();
    match scope.alloc_string_from_u16_vec(units) {
      Err(VmError::OutOfMemory) => process::exit(0),
      Ok(_) => {
        eprintln!("oom_harness: unexpected success in allocStringU16SpareCap");
        process::exit(1);
      }
      Err(err) => {
        eprintln!("oom_harness: unexpected error in allocStringU16SpareCap: {err:?}");
        process::exit(1);
      }
    }
  }

  if scenario == "moduleLink" {
    // Allocate a very large module specifier in host memory. Previously `ModuleGraph::link_inner`
    // used `format!(...)` to build error messages containing attacker-controlled strings like
    // module specifiers; that infallibly allocates and can abort the process under allocator OOM.
    //
    // This scenario triggers an indirect-export resolution failure during module linking while the
    // process is under a tight RLIMIT_AS, and asserts we exit cleanly (either `VmError::OutOfMemory`
    // or a thrown `SyntaxError`).
    //
    // Keep the filler buffer alive for the duration of linking so the process stays close to the
    // RLIMIT_AS ceiling.
    let _keep = &filler;
    let alloc_ascii_js_string = |len: usize| -> Option<JsString> {
      let mut units: Vec<u16> = Vec::new();
      units.try_reserve_exact(len).ok()?;
      units.resize(len, b'a' as u16);
      JsString::from_u16_vec(units).ok()
    };

    let Some(spec1) = alloc_ascii_js_string(len_code_units) else {
      eprintln!("oom_harness: failed to allocate large module specifier (entry)");
      process::exit(1);
    };
    let Some(spec2) = alloc_ascii_js_string(len_code_units) else {
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
    let imported_id = match graph.add_module(imported) {
      Ok(id) => id,
      Err(VmError::OutOfMemory) => process::exit(0),
      Err(err) => {
        eprintln!("oom_harness: unexpected error adding imported module: {err:?}");
        process::exit(1);
      }
    };

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

    let root_id = match graph.add_module(root) {
      Ok(id) => id,
      Err(VmError::OutOfMemory) => process::exit(0),
      Err(err) => {
        eprintln!("oom_harness: unexpected error adding root module: {err:?}");
        process::exit(1);
      }
    };

    let (vm, realm, heap) = agent.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let result = graph.link(vm, heap, global, realm.id(), root_id);
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
 
  if scenario == "moduleGetExportedNames" {
    // Construct a minimal module record with a single extremely large export name, then run the
    // `GetExportedNames` algorithm under RLIMIT_AS pressure. Previously infallible `String::clone()`
    // in the algorithm could abort the process on allocator OOM.
    let mut record = match SourceTextModuleRecord::parse(agent.heap_mut(), "export const x = 1;") {
      Ok(record) => record,
      Err(err) => {
        eprintln!("oom_harness: failed to parse module record: {err:?}");
        process::exit(1);
      }
    };

    if record.local_export_entries.is_empty() {
      eprintln!("oom_harness: unexpected empty export entry list");
      process::exit(1);
    }

    // Use U+0800 so each character expands to 3 bytes in UTF-8, allowing the test to hit allocator
    // OOM with smaller `len_code_units` values.
    let fill = '\u{0800}';
    let bytes_per = fill.len_utf8();
    let reserve_bytes = len_code_units.checked_mul(bytes_per).unwrap_or(usize::MAX);

    let mut export_name = String::new();
    if export_name.try_reserve_exact(reserve_bytes).is_err() {
      eprintln!("oom_harness: failed to allocate export name string");
      process::exit(1);
    }
    export_name.extend(std::iter::repeat(fill).take(len_code_units));
    record.local_export_entries[0].export_name = export_name;

    // Keep `filler` alive for the duration of the algorithm run.
    let _keep = &filler;

    let graph = ModuleGraph::new();
    let module = ModuleId::from_raw(0);

    match record.get_exported_names_with_vm(agent.vm_mut(), &graph, module) {
      Err(VmError::OutOfMemory) => process::exit(0),
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

  let needs_global_string = matches!(
    scenario.as_str(),
    "eval"
      | "function"
      | "generator"
      | "number"
      | "parseFloat"
      | "regexp_compile"
      | "regexp"
      | "arrayMap"
      | "throw_string_format"
  );

  if needs_global_string {
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
  }

  let result = match scenario.as_str() {
    "labelEarlyError" => {
      // Construct a large script containing an early error involving a huge label identifier. This
      // previously used infallible `format!` in early error formatting, which could abort the
      // process under allocator OOM.
      let total_len = "break ".len()
        .checked_add(len_code_units)
        .and_then(|n| n.checked_add(";".len()))
        .unwrap_or(usize::MAX);
      if total_len == usize::MAX {
        eprintln!("oom_harness: labelEarlyError script length overflow");
        process::exit(1);
      }
      let mut bytes: Vec<u8> = Vec::new();
      if bytes.try_reserve_exact(total_len).is_err() {
        eprintln!("oom_harness: failed to allocate labelEarlyError script buffer");
        process::exit(1);
      }
      bytes.extend_from_slice(b"break ");
      bytes.resize("break ".len() + len_code_units, b'a');
      bytes.extend_from_slice(b";");
      let script = unsafe { String::from_utf8_unchecked(bytes) };

      // Keep `filler` alive for the duration of the run.
      let _keep = &filler;

      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    "globalVarDecl" => {
      // Construct a large script containing a valid global `var` declaration with an enormous
      // identifier name. GlobalDeclarationInstantiation performs multiple name-scans over the
      // script body; those scans must use fallible allocations so allocator OOM does not abort the
      // process (e.g. avoid infallible `String::clone()` on attacker-controlled names).
      let total_len = "var ".len()
        .checked_add(len_code_units)
        .and_then(|n| n.checked_add(";".len()))
        .unwrap_or(usize::MAX);
      if total_len == usize::MAX {
        eprintln!("oom_harness: globalVarDecl script length overflow");
        process::exit(1);
      }
      let mut bytes: Vec<u8> = Vec::new();
      if bytes.try_reserve_exact(total_len).is_err() {
        eprintln!("oom_harness: failed to allocate globalVarDecl script buffer");
        process::exit(1);
      }
      bytes.extend_from_slice(b"var ");
      bytes.resize("var ".len() + len_code_units, b'a');
      bytes.extend_from_slice(b";");
      let script = unsafe { String::from_utf8_unchecked(bytes) };

      // Keep `filler` alive for the duration of the run.
      let _keep = &filler;

      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    "captureStack" => {
      // Force stack capture under allocator OOM pressure. We pre-fill the VM stack with synthetic
      // frames until allocating a duplicate stack trace vector fails, then (best-effort) invoke
      // `Vm::capture_stack` directly before running a throwing script.
      let frame = StackFrame {
        function: None,
        source: Arc::<str>::from("<oom_harness>"),
        line: 0,
        col: 0,
      };

      // Push frames in chunks and probe whether allocating a stack trace vector of the same length
      // succeeds. When the probe reserve fails, `Vm::capture_stack` would have aborted the process
      // before this regression fix.
      const CHUNK: usize = 32 * 1024;
      const MAX_FRAMES: usize = 5_000_000;
      let mut pushed: usize = 0;
      let mut probe_failed = false;
      'fill: loop {
        for _ in 0..CHUNK {
          if pushed >= MAX_FRAMES {
            break 'fill;
          }
          match agent.vm_mut().push_frame(frame.clone()) {
            Ok(()) => pushed += 1,
            Err(VmError::OutOfMemory) => {
              probe_failed = true;
              break 'fill;
            }
            Err(err) => {
              eprintln!("oom_harness: failed to push synthetic stack frame: {err:?}");
              process::exit(1);
            }
          }
        }

        let mut probe: Vec<StackFrame> = Vec::new();
        if probe.try_reserve_exact(pushed).is_err() {
          probe_failed = true;
          break;
        }
      }

      if probe_failed {
        // Execute `capture_stack` directly so the regression is exercised even if script execution
        // returns early due to `VmError::OutOfMemory`.
        let _ = agent.vm().capture_stack();
      }

      agent.run_script("oom_harness.js", "throw 1", Budget::unlimited(1), None)
    }
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
    "stackTrace" => {
      // Trigger stack trace formatting under memory pressure (large `source_name` held alive by the
      // captured stack frames).
      let script = "function f(n) { if (n === 0) throw 1; return f(n - 1); }\nf(64);";
      let _keep = &filler;
 
      // For the stack trace scenario, interpret `len_code_units` as a *byte length* for the source
      // name. This keeps the script text small while making stack frame formatting expensive.
      let mut bytes: Vec<u8> = Vec::new();
      if bytes.try_reserve_exact(len_code_units).is_err() {
        eprintln!("oom_harness: failed to allocate source name buffer");
        process::exit(1);
      }
      bytes.resize(len_code_units, b'a');
      let source_name = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(err) => {
          eprintln!("oom_harness: failed to build source name string: {err:?}");
          process::exit(1);
        }
      };
      agent.run_script(source_name, script, Budget::unlimited(1), None)
    }
    "promiseJob" => {
      // Trigger Promise job creation (`HostEnqueuePromiseJob` / microtask job boxing).
      let script = "Promise.resolve().then(() => 0)";
      let _keep = &filler;
      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    "generatorInstance" => {
      // Trigger generator object creation and continuation boxing.
      let script = "(function*(){ yield 1; })().next()";
      let _keep = &filler;
      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    "register_ecma_function" => {
      // Force many `Vm::register_ecma_function` calls (and thus many `ecma_function_cache` inserts)
      // under an OS address-space limit. Historically `HashMap::insert` here could abort the
      // process on allocator OOM.
      //
      // Interpret `len_code_units` as the number of dynamic `Function` constructor calls to run.
      let func_count = len_code_units;
      let mut script = String::new();
      if script.try_reserve(256).is_err() {
        eprintln!("oom_harness: failed to allocate register_ecma_function script buffer");
        process::exit(1);
      }
      use std::fmt::Write;
      script.push_str("for (let i = 0; i < ");
      if write!(&mut script, "{func_count}").is_err() {
        eprintln!("oom_harness: failed to format register_ecma_function iteration count");
        process::exit(1);
      }
      script.push_str("; i++) { Function(''); }\n0;\n");

      // Keep `filler` alive for the duration of the run.
      let _keep = &filler;
      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    "eval"
    | "function"
    | "generator"
    | "generator_invoke"
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
        // Stress generator *invocation* (creating generator objects / continuations) under
        // allocator OOM. Previously the continuation boxing was infallible (`Box::new`) and could
        // abort the process.
        "generator_invoke" => "const set = new Set(); function* g(){} while(true){ set.add(g()); }",
        "number" => "Number(S)",
        "parseFloat" => "parseFloat(S)",
        "regexp_compile" | "regexp" => "new RegExp(S)",
        // Exercise host-visible error formatting (`Agent::format_vm_error`) on a thrown string value
        // that is too large to stringify under RLIMIT_AS pressure. Use a computed property name so
        // the stack frame attempts to capture a huge function name.
        "throw_string_format" => "const o = { [S]: function() { throw S; } }; o[S]();",
        // Trigger per-iteration array index key formatting (`ToString(k)` for each `k < length`) under
        // memory pressure. Previously this used intermediate Rust heap `String` allocations, which can
        // abort the process on allocator OOM.
        "arrayMap" => "Array(S.length).map(() => 0)",
        _ => unreachable!(),
      };
 
      let _keep = &filler;
      agent.run_script("oom_harness.js", script, Budget::unlimited(1), None)
    }
    other => {
      eprintln!("oom_harness: unknown scenario: {other}");
      process::exit(2);
    }
  };

  if scenario == "throw_string_format" {
    // Keep `filler` alive while formatting the thrown value.
    let _keep = &filler;
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

  if scenario == "stackTrace" {
    match &result {
      Err(VmError::ThrowWithStack { stack, .. }) => {
        if stack.is_empty() {
          eprintln!("oom_harness: expected non-empty stack trace");
          process::exit(1);
        }
        // Ensure stack trace formatting runs under memory pressure and never aborts.
        let _ = format_stack_trace(stack);
        process::exit(0);
      }
      other => {
        eprintln!("oom_harness: unexpected stackTrace result: {other:?}");
        process::exit(1);
      }
    }
  }

  match result {
    Err(VmError::OutOfMemory) => process::exit(0),
    // For job/continuation allocations we primarily care about avoiding process aborts under
    // allocator OOM. Some environments may have enough headroom under the chosen RLIMIT settings for
    // the scenario to succeed; treat success and other (catchable) errors as acceptable outcomes.
    Ok(_) if scenario == "promiseJob" || scenario == "generatorInstance" => process::exit(0),
    Err(_) if scenario == "promiseJob" || scenario == "generatorInstance" => process::exit(0),
    Err(VmError::Syntax(_)) if scenario == "labelEarlyError" => process::exit(0),
    Err(VmError::Throw(_) | VmError::ThrowWithStack { .. }) if scenario == "captureStack" => {
      process::exit(0)
    }
    // Some scenarios are allowed to succeed here: the key invariant for this harness is that we
    // never abort the process under allocator OOM pressure.
    //
    // `parseFloat`/`Number` may parse directly from UTF-16 without allocating an intermediate
    // `String`.
    //
    // `arrayMap` may be able to iterate indices without allocating (e.g. using integer keys rather
    // than formatting each index into a Rust `String`).
    Ok(_) if scenario == "parseFloat" || scenario == "number" || scenario == "arrayMap" => {
      process::exit(0)
    }
    Err(VmError::Termination(_)) if scenario == "register_ecma_function" => process::exit(0),
    Ok(_) if scenario == "register_ecma_function" => process::exit(0),
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
