#define _POSIX_C_SOURCE 200809L

#include "runtime_native.h"

#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define BYTES_LIT(s) ((const uint8_t*)(s)), (sizeof(s) - 1)

static int check(int cond) {
  return cond ? 0 : 1;
}

static void set_int(uint8_t* data) {
  int* flag = (int*)data;
  *flag = 1;
}

static void* enqueue_microtask_from_thread(void* arg) {
  // Register this OS thread with the runtime so allocator + safepoint machinery
  // is initialized. This also documents that embedders should treat queue
  // operations as "mutator" interactions.
  rt_thread_init(1);

  // Give the main thread time to enter `rt_async_wait()` so this exercise the
  // cross-thread wakeup path.
  struct timespec delay = {
    .tv_sec = 0,
    .tv_nsec = 50 * 1000 * 1000,
  };
  (void)nanosleep(&delay, NULL);

  int* flag = (int*)arg;
  Microtask task = {
    .func = set_int,
    .data = (uint8_t*)flag,
  };
  rt_queue_microtask(task);

  rt_thread_deinit();
  return NULL;
}

typedef struct MicrotaskDropCtx {
  int ran;
  int dropped;
} MicrotaskDropCtx;

static void microtask_mark_ran(uint8_t* data) {
  MicrotaskDropCtx* ctx = (MicrotaskDropCtx*)data;
  ctx->ran = 1;
}

static void microtask_mark_dropped(uint8_t* data) {
  MicrotaskDropCtx* ctx = (MicrotaskDropCtx*)data;
  ctx->dropped = 1;
}

static GcPtr expected_root = NULL;
static int rooted_microtask_ran = 0;

static void microtask_check_root(uint8_t* data) {
  rooted_microtask_ran = (data == expected_root) ? 1 : 2;
}

static GcPtr handle_microtask_seen = NULL;
static int handle_microtask_ran = 0;
static GcPtr handle_microtask_dropped = NULL;
static int handle_microtask_drop_count = 0;

static void handle_microtask_record(GcPtr data) {
  handle_microtask_ran = 1;
  handle_microtask_seen = data;
}

static void handle_microtask_drop(GcPtr data) {
  handle_microtask_drop_count += 1;
  handle_microtask_dropped = data;
}

static void par_for_body(size_t i, uint8_t* data) {
  uint32_t* out = (uint32_t*)data;
  out[i] = (uint32_t)(i * 3u + 1u);
}

// -----------------------------------------------------------------------------
// Native async ABI smoke test (CoroutineId + rt_async_spawn)
// -----------------------------------------------------------------------------
typedef struct NativeAsyncSmokeCoro {
  Coroutine header;
  int* ran;
  int* destroyed;
} NativeAsyncSmokeCoro;

static CoroutineStep native_async_smoke_resume(Coroutine* coro) {
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->ran) {
    *c->ran = 1;
  }
  // The runtime must have written the result promise pointer before first resume.
  if (coro->promise == NULL) {
    if (c->ran) {
      *c->ran = 2;
    }
    return (CoroutineStep){RT_CORO_STEP_COMPLETE, NULL};
  }
  // Fulfill the promise and complete. The runtime should then free the CoroutineId handle.
  if (!rt_promise_try_fulfill(coro->promise)) {
    if (c->ran) {
      *c->ran = 3;
    }
  }
  return (CoroutineStep){RT_CORO_STEP_COMPLETE, NULL};
}

static void native_async_smoke_destroy(CoroutineRef coro) {
  // Stack-owned coroutine frames must never be destroyed by the runtime.
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->destroyed) {
    *c->destroyed = 1;
  }
}

static const CoroutineVTable NATIVE_ASYNC_SMOKE_VTABLE = {
  .resume = native_async_smoke_resume,
  .destroy = native_async_smoke_destroy,
  // Use conservative values: this smoke test treats PromiseHeader as opaque.
  .promise_size = 64,
  .promise_align = 16,
  .promise_shape_id = 0,
  .abi_version = RT_ASYNC_ABI_VERSION,
  .reserved = {0, 0, 0, 0},
};

static void native_async_heap_destroy(CoroutineRef coro) {
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->destroyed) {
    *c->destroyed = 1;
  }
  free(c);
}

static const CoroutineVTable NATIVE_ASYNC_HEAP_VTABLE = {
  .resume = native_async_smoke_resume,
  .destroy = native_async_heap_destroy,
  .promise_size = 64,
  .promise_align = 16,
  .promise_shape_id = 0,
  .abi_version = RT_ASYNC_ABI_VERSION,
  .reserved = {0, 0, 0, 0},
};

int main(void) {
  // The runtime expects mutator threads to register before executing compiled
  // code or participating in GC safepoints.
  rt_thread_init(0);
  // Ensure strict-await configuration entrypoint is present/callable.
  rt_async_set_strict_await_yields(false);
  int rc = 0;
  pthread_t wake_thread;
  int wake_thread_started = 0;
  static const RtShapeDescriptor kShapes[1] = {
    {
      .size = 16,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    },
  };
  rt_register_shape_table(kShapes, 1);

  RtShapeId shape = (RtShapeId)1;
  GcPtr obj1 = rt_alloc_pinned(16, shape);
  GcPtr obj2 = rt_alloc_pinned(16, shape);
  if (check(obj1 != NULL)) { rc = 6; goto done; }
  if (check(obj2 != NULL)) { rc = 7; goto done; }

  // Temporary shadow-stack roots: embedders/native glue can register addressable
  // GC pointer slots so a moving GC can update them in-place.
  //
  // This smoke test doesn't exercise relocation (pinned objects never move),
  // but it ensures the symbol is present and callable from C.
  GcPtr tmp = obj1;
  rt_root_push(&tmp);
  rt_gc_safepoint();
  rt_root_pop(&tmp);
  // `rt_keep_alive_gc_ref` is the native equivalent of Go's `runtime.KeepAlive`: it exists to
  // extend the liveness of GC objects when native/compiled code uses derived raw pointers.
  // This smoke test doesn't model those derived pointers, but it does ensure the symbol is present
  // and callable from C.
  rt_keep_alive_gc_ref(obj1);

  // Persistent handle API: stable u64 ids for keeping GC objects alive across
  // async / OS boundaries.
  HandleId h = rt_handle_alloc(obj1);
  if (check(rt_handle_load(h) == obj1)) { rc = 8; goto done; }
  rt_handle_store(h, obj2);
  if (check(rt_handle_load(h) == obj2)) { rc = 9; goto done; }
  rt_handle_free(h);
  if (check(rt_handle_load(h) == NULL)) { rc = 10; goto done; }
  rt_handle_store(h, obj1);
  if (check(rt_handle_load(h) == NULL)) { rc = 11; goto done; }

  // Native async ABI: allocate a CoroutineId handle and spawn a coroutine.
  //
  // This is stack-owned and must complete synchronously; the runtime should *not*
  // call `destroy`, but it must still free the CoroutineId handle.
  int async_ran = 0;
  int async_destroyed = 0;
  NativeAsyncSmokeCoro async_coro = {
    .header =
      {
        .vtable = &NATIVE_ASYNC_SMOKE_VTABLE,
        .promise = NULL,
        .next_waiter = NULL,
        .flags = 0,
      },
    .ran = &async_ran,
    .destroyed = &async_destroyed,
  };
  CoroutineId coro_id = rt_handle_alloc((GcPtr)&async_coro);
  PromiseRef p = rt_async_spawn(coro_id);
  if (check(async_destroyed == 0)) { rc = 24; goto done; }
  if (check(async_ran == 1)) { rc = 25; goto done; }
  if (check(p != NULL)) { rc = 26; goto done; }
  if (check(p == async_coro.header.promise)) { rc = 27; goto done; }
  if (check(!rt_promise_try_fulfill(p))) { rc = 28; goto done; }
  if (check(rt_handle_load(coro_id) == NULL)) { rc = 29; goto done; }
  // Blocking wait helper should return immediately for already-settled promises.
  rt_async_block_on(p);

  // Deferred spawn: must schedule the first resume as a microtask.
  int deferred_ran = 0;
  int deferred_destroyed = 0;
  NativeAsyncSmokeCoro* deferred_coro = (NativeAsyncSmokeCoro*)malloc(sizeof(NativeAsyncSmokeCoro));
  if (deferred_coro == NULL) { rc = 30; goto done; }
  *deferred_coro = (NativeAsyncSmokeCoro){
    .header =
      {
        .vtable = &NATIVE_ASYNC_HEAP_VTABLE,
        .promise = NULL,
        .next_waiter = NULL,
        .flags = CORO_FLAG_RUNTIME_OWNS_FRAME,
      },
    .ran = &deferred_ran,
    .destroyed = &deferred_destroyed,
  };
  CoroutineId deferred_id = rt_handle_alloc((GcPtr)deferred_coro);
  PromiseRef deferred_promise = rt_async_spawn_deferred(deferred_id);
  if (check(deferred_promise != NULL)) { rc = 31; goto done; }
  if (check(deferred_ran == 0)) { rc = 32; goto done; }
  if (check(deferred_promise == deferred_coro->header.promise)) { rc = 33; goto done; }
  rt_drain_microtasks();
  if (check(deferred_ran == 1)) { rc = 34; goto done; }
  if (check(deferred_destroyed == 1)) { rc = 35; goto done; }
  if (check(rt_handle_load(deferred_id) == NULL)) { rc = 36; goto done; }
  if (check(!rt_promise_try_fulfill(deferred_promise))) { rc = 37; goto done; }

  // Cancellation: a deferred runtime-owned coroutine that never runs must still be destroyed and
  // have its CoroutineId handle freed (and its scheduled resume microtask discarded).
  int cancel_ran = 0;
  int cancel_destroyed = 0;
  NativeAsyncSmokeCoro* cancel_coro = (NativeAsyncSmokeCoro*)malloc(sizeof(NativeAsyncSmokeCoro));
  if (cancel_coro == NULL) { rc = 38; goto done; }
  *cancel_coro = (NativeAsyncSmokeCoro){
    .header =
      {
        .vtable = &NATIVE_ASYNC_HEAP_VTABLE,
        .promise = NULL,
        .next_waiter = NULL,
        .flags = CORO_FLAG_RUNTIME_OWNS_FRAME,
      },
    .ran = &cancel_ran,
    .destroyed = &cancel_destroyed,
  };
  CoroutineId cancel_id = rt_handle_alloc((GcPtr)cancel_coro);
  PromiseRef cancel_promise = rt_async_spawn_deferred(cancel_id);
  if (check(cancel_promise != NULL)) { rc = 39; goto done; }
  if (check(cancel_promise == cancel_coro->header.promise)) { rc = 40; goto done; }
  if (check(cancel_ran == 0)) { rc = 41; goto done; }
  if (check(cancel_destroyed == 0)) { rc = 42; goto done; }
  rt_async_cancel_all();
  if (check(cancel_ran == 0)) { rc = 43; goto done; }
  if (check(cancel_destroyed == 1)) { rc = 44; goto done; }
  if (check(rt_handle_load(cancel_id) == NULL)) { rc = 45; goto done; }
  // Draining after cancellation should be a no-op and must not run stale resume microtasks.
  rt_drain_microtasks();
  if (check(cancel_ran == 0)) { rc = 46; goto done; }

  InternedId id1 = rt_string_intern(BYTES_LIT("hello"));
  InternedId id2 = rt_string_intern(BYTES_LIT("hello"));
  if (check(id1 == id2)) { rc = 1; goto done; }
  rt_string_pin_interned(id1);

  StringRef ab = rt_string_concat(BYTES_LIT("a"), BYTES_LIT("b"));
  if (check(ab.len == 2)) { rc = 2; goto done; }
  if (check(ab.ptr != NULL)) { rc = 3; goto done; }
  if (check(memcmp(ab.ptr, "ab", 2) == 0)) { rc = 4; goto done; }

  // Microtask API: microtasks should not run synchronously when queued, and should run once drained.
  int microtask_ran = 0;
  Microtask mt = {
    .func = set_int,
    .data = (uint8_t*)&microtask_ran,
  };
  rt_queue_microtask(mt);
  if (check(microtask_ran == 0)) { rc = 12; goto done; }

  MicrotaskDropCtx drop_ctx = {0, 0};
  rt_queue_microtask_with_drop(microtask_mark_ran, (uint8_t*)&drop_ctx, microtask_mark_dropped);
  if (check(drop_ctx.ran == 0)) { rc = 14; goto done; }
  if (check(drop_ctx.dropped == 0)) { rc = 15; goto done; }

  expected_root = obj1;
  rooted_microtask_ran = 0;
  rt_queue_microtask_rooted(microtask_check_root, obj1);
  if (check(rooted_microtask_ran == 0)) { rc = 18; goto done; }

  if (check(rt_async_run_until_idle())) { rc = 47; goto done; }
  if (check(microtask_ran == 1)) { rc = 13; goto done; }
  if (check(drop_ctx.ran == 1)) { rc = 16; goto done; }
  // The microtask drop hook should only run if the microtask is discarded without executing
  // (e.g. `rt_async_cancel_all`), not after a normal run.
  if (check(drop_ctx.dropped == 0)) { rc = 17; goto done; }
  if (check(rooted_microtask_ran == 1)) { rc = 19; goto done; }

  // Handle-based microtasks: the runtime consumes a `HandleId` and frees it when the work item is
  // torn down (after execution or cancellation).
  GcPtr handle_obj = rt_alloc_pinned(16, shape);
  if (check(handle_obj != NULL)) { rc = 38; goto done; }
  HandleId handle_id = rt_handle_alloc(handle_obj);
  handle_microtask_ran = 0;
  handle_microtask_seen = NULL;
  rt_queue_microtask_handle(handle_microtask_record, handle_id);
  if (check(handle_microtask_ran == 0)) { rc = 39; goto done; }
  rt_drain_microtasks();
  if (check(handle_microtask_ran == 1)) { rc = 40; goto done; }
  if (check(handle_microtask_seen == handle_obj)) { rc = 41; goto done; }
  if (check(rt_handle_load(handle_id) == NULL)) { rc = 42; goto done; }

  GcPtr handle_obj2 = rt_alloc_pinned(16, shape);
  if (check(handle_obj2 != NULL)) { rc = 43; goto done; }
  HandleId handle_id2 = rt_handle_alloc(handle_obj2);
  handle_microtask_ran = 0;
  handle_microtask_seen = NULL;
  handle_microtask_drop_count = 0;
  handle_microtask_dropped = NULL;
  rt_queue_microtask_handle_with_drop(handle_microtask_record, handle_id2, handle_microtask_drop);
  if (check(handle_microtask_ran == 0)) { rc = 44; goto done; }
  if (check(handle_microtask_drop_count == 0)) { rc = 45; goto done; }
  rt_drain_microtasks();
  if (check(handle_microtask_ran == 1)) { rc = 46; goto done; }
  if (check(handle_microtask_seen == handle_obj2)) { rc = 47; goto done; }
  if (check(handle_microtask_drop_count == 1)) { rc = 48; goto done; }
  if (check(handle_microtask_dropped == handle_obj2)) { rc = 49; goto done; }
  if (check(rt_handle_load(handle_id2) == NULL)) { rc = 50; goto done; }

  // Cross-thread enqueue should wake an event loop thread blocked in `rt_async_wait`.
  int wake_microtask_ran = 0;
  if (pthread_create(&wake_thread, NULL, enqueue_microtask_from_thread, &wake_microtask_ran) != 0) {
    rc = 20;
    goto done;
  }
  wake_thread_started = 1;

  rt_async_wait();
  // `rt_async_wait` should only park/unpark; it must not run microtasks itself.
  if (check(wake_microtask_ran == 0)) { rc = 21; goto done; }

  rt_drain_microtasks();
  if (check(wake_microtask_ran == 1)) { rc = 22; goto done; }

  if (pthread_join(wake_thread, NULL) != 0) { rc = 23; goto done; }
  wake_thread_started = 0;

  enum { N = 4096 };
  uint32_t out[N];
  memset(out, 0, sizeof(out));
  rt_parallel_for(0, N, par_for_body, (uint8_t*)out);
  for (size_t i = 0; i < N; i++) {
    if (check(out[i] == (uint32_t)(i * 3u + 1u))) { rc = 5; goto done; }
  }

  // Microtask drop hook should run when the embedding cancels queued work.
  MicrotaskDropCtx cancel_ctx = {0, 0};
  rt_queue_microtask_with_drop(microtask_mark_ran, (uint8_t*)&cancel_ctx, microtask_mark_dropped);
  if (check(cancel_ctx.ran == 0)) { rc = 30; goto done; }
  if (check(cancel_ctx.dropped == 0)) { rc = 31; goto done; }

  GcPtr cancel_handle_obj = rt_alloc_pinned(16, shape);
  if (check(cancel_handle_obj != NULL)) { rc = 34; goto done; }
  HandleId cancel_handle = rt_handle_alloc(cancel_handle_obj);
  handle_microtask_ran = 0;
  handle_microtask_drop_count = 0;
  handle_microtask_seen = NULL;
  handle_microtask_dropped = NULL;
  rt_queue_microtask_handle_with_drop(handle_microtask_record, cancel_handle, handle_microtask_drop);
  if (check(handle_microtask_ran == 0)) { rc = 35; goto done; }
  if (check(handle_microtask_drop_count == 0)) { rc = 36; goto done; }

  rt_async_cancel_all();
  if (check(cancel_ctx.ran == 0)) { rc = 32; goto done; }
  if (check(cancel_ctx.dropped == 1)) { rc = 33; goto done; }
  if (check(handle_microtask_ran == 0)) { rc = 37; goto done; }
  if (check(handle_microtask_drop_count == 1)) { rc = 38; goto done; }
  if (check(handle_microtask_dropped == cancel_handle_obj)) { rc = 39; goto done; }
  if (check(rt_handle_load(cancel_handle) == NULL)) { rc = 40; goto done; }

done:
  if (wake_thread_started) {
    (void)pthread_join(wake_thread, NULL);
  }
  rt_thread_deinit();
  return rc;
}
