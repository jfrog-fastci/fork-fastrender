use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

use crate::abi::TaskId;
use crate::threading::{self, ThreadKind};

use super::Chunking;
use super::ParallelRuntime;
use super::WorkEstimate;

pub(crate) type ParForBody = extern "C" fn(usize, *mut u8);

pub(super) fn min_grain() -> usize {
  static MIN_GRAIN: OnceLock<usize> = OnceLock::new();
  *MIN_GRAIN.get_or_init(|| {
    const DEFAULT: usize = 1024;

    match std::env::var("RT_PAR_FOR_MIN_GRAIN") {
      Ok(v) => match v.parse::<usize>() {
        Ok(0) | Err(_) => DEFAULT,
        Ok(n) => n,
      },
      Err(_) => DEFAULT,
    }
  })
}

fn call_body(body: ParForBody, i: usize, data: *mut u8) {
  // `parallel_for` owns the iteration loop in the runtime. Poll the GC safepoint
  // here so stop-the-world requests don't have to wait for the user callback to
  // hit a compiler-inserted safepoint.
  threading::safepoint_poll();

  let res = catch_unwind(AssertUnwindSafe(|| body(i, data)));
  if res.is_err() {
    // Never unwind across our `extern "C"` boundary.
    std::process::abort();
  }
}

#[repr(C)]
struct ParForChunk {
  start: usize,
  end: usize,
  body: ParForBody,
  data: *mut u8,
}

extern "C" fn par_for_task(data: *mut u8) {
  let chunk = unsafe { Box::from_raw(data as *mut ParForChunk) };
  for i in chunk.start..chunk.end {
    call_body(chunk.body, i, chunk.data);
  }
}

pub(crate) fn parallel_for(
  rt: &ParallelRuntime,
  start: usize,
  end: usize,
  body: ParForBody,
  data: *mut u8,
  chunking: Chunking,
) {
  // Ensure the caller is registered for safepoint coordination even if we fall
  // back to sequential execution.
  threading::register_current_thread(ThreadKind::External);

  if end <= start {
    return;
  }

  let len = end - start;
  let min_grain = min_grain();

  let estimate = WorkEstimate {
    items: len,
    cost: len as u64,
  };
  if !super::should_parallelize(estimate) || rt.worker_count() <= 1 {
    for i in start..end {
      call_body(body, i, data);
    }
    return;
  }

  let chunk_size = match chunking {
    Chunking::Fixed(size) => size.max(1),
    Chunking::Auto => {
      let target_chunks = rt.worker_count().saturating_mul(4).max(1);
      let mut chunk_size = len.div_ceil(target_chunks).max(min_grain);
      if chunk_size == 0 {
        // `max(min_grain)` should prevent this, but keep it defensive.
        chunk_size = 1;
      }
      chunk_size
    }
  };

  if chunk_size >= len {
    for i in start..end {
      call_body(body, i, data);
    }
    return;
  }

  let mut tasks: Vec<TaskId> = Vec::new();
  let mut chunk_start = start;
  while chunk_start < end {
    let chunk_end = end.min(chunk_start.saturating_add(chunk_size));
    let chunk = Box::new(ParForChunk {
      start: chunk_start,
      end: chunk_end,
      body,
      data,
    });
    let id = rt.spawn(par_for_task, Box::into_raw(chunk) as *mut u8);
    tasks.push(id);
    chunk_start = chunk_end;
  }

  rt.join(tasks.as_ptr(), tasks.len());
}
