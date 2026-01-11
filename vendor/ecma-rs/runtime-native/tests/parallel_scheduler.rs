use runtime_native::abi::TaskId;
use runtime_native::{rt_parallel_join, rt_parallel_spawn};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

static TEST_INIT: Once = Once::new();

fn init() {
    TEST_INIT.call_once(|| {
        // Keep tests deterministic: ensure we have multiple worker threads so
        // the steal behavior test is meaningful.
        std::env::set_var("RT_NUM_THREADS", "4");
    });
}

extern "C" fn inc_counter(data: *mut u8) {
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn basic_spawn_join() {
    init();

    let counter = AtomicUsize::new(0);
    let mut tasks: Vec<TaskId> = Vec::new();
    for _ in 0..1_000 {
        tasks.push(rt_parallel_spawn(inc_counter, (&counter as *const AtomicUsize) as *mut u8));
    }

    rt_parallel_join(tasks.as_ptr(), tasks.len());
    assert_eq!(counter.load(Ordering::Relaxed), 1_000);
}

#[repr(C)]
struct NestedData {
    counter: *const AtomicUsize,
    inner: usize,
}

extern "C" fn nested_task(data: *mut u8) {
    let data = unsafe { Box::from_raw(data as *mut NestedData) };

    let mut tasks = Vec::with_capacity(data.inner);
    for _ in 0..data.inner {
        tasks.push(rt_parallel_spawn(inc_counter, data.counter as *mut u8));
    }
    rt_parallel_join(tasks.as_ptr(), tasks.len());
}

#[test]
fn nested_parallelism_does_not_deadlock() {
    init();

    let counter = AtomicUsize::new(0);

    let outer = 64;
    let inner = 64;

    let mut tasks = Vec::with_capacity(outer);
    for _ in 0..outer {
        let data = Box::new(NestedData {
            counter: &counter as *const AtomicUsize,
            inner,
        });
        tasks.push(rt_parallel_spawn(nested_task, Box::into_raw(data) as *mut u8));
    }

    rt_parallel_join(tasks.as_ptr(), tasks.len());
    assert_eq!(counter.load(Ordering::Relaxed), outer * inner);
}

#[repr(C)]
struct ThreadRecord {
    threads: Mutex<HashSet<std::thread::ThreadId>>,
}

extern "C" fn record_thread(data: *mut u8) {
    let data = unsafe { &*(data as *const ThreadRecord) };
    let mut set = data.threads.lock().unwrap();
    set.insert(std::thread::current().id());
}

#[test]
fn steal_behavior_smoke() {
    init();

    let record = ThreadRecord {
        threads: Mutex::new(HashSet::new()),
    };

    let mut tasks = Vec::new();
    for _ in 0..10_000 {
        tasks.push(rt_parallel_spawn(
            record_thread,
            (&record as *const ThreadRecord) as *mut u8,
        ));
    }

    rt_parallel_join(tasks.as_ptr(), tasks.len());

    let set = record.threads.lock().unwrap();
    assert!(
        set.len() > 1,
        "expected >1 worker threads to execute tasks, got {}",
        set.len()
    );
}

#[test]
fn stress_spawn_join_many_times() {
    init();

    let counter = AtomicUsize::new(0);
    for _ in 0..100 {
        let mut tasks: Vec<TaskId> = Vec::new();
        for _ in 0..1_000 {
            tasks.push(rt_parallel_spawn(inc_counter, (&counter as *const AtomicUsize) as *mut u8));
        }
        rt_parallel_join(tasks.as_ptr(), tasks.len());
    }

    assert_eq!(counter.load(Ordering::Relaxed), 100 * 1_000);
}

#[test]
fn join_tolerates_duplicate_task_ids() {
    init();

    let counter = AtomicUsize::new(0);
    let task = rt_parallel_spawn(inc_counter, (&counter as *const AtomicUsize) as *mut u8);

    let tasks = vec![task, task, task];
    rt_parallel_join(tasks.as_ptr(), tasks.len());
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}
