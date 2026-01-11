// Define the `RT_THREAD` TLS symbol that native codegen can access directly.
//
// Rust's `#[thread_local]` static is still unstable on stable toolchains, so we
// define this TLS slot in a tiny C translation unit and expose internal helper
// functions that Rust uses to read/write it during attach/detach.
//
// This keeps `include/runtime_native.h` honest: the `RT_THREAD` symbol is a real
// link-visible TLS variable when linking against `libruntime_native.a` or the
// cdylib.
//
// Note: The `Thread` struct itself is defined in Rust; C only ever sees an
// opaque forward declaration and passes pointers around.
struct Thread;

#if defined(_MSC_VER)
__declspec(thread) struct Thread* RT_THREAD;
#else
__thread struct Thread* RT_THREAD;
#endif

struct Thread* runtime_native_tls_get_rt_thread(void) { return RT_THREAD; }
void runtime_native_tls_set_rt_thread(struct Thread* thread) { RT_THREAD = thread; }
