//! Allocation and retention accounting for observepass.
//!
//! A counting global allocator wraps the system allocator and reports exact
//! total and current heap bytes/blocks. This binary is a measurement tool for
//! the observepass mechanism; it does not touch the mechanism's code. Emits
//! `METRIC name=value` lines consumed by `autoresearch.sh`. Fixed operation
//! counts; no RNG.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use observepass::ObservationSpace;

/// Total allocation events (`alloc` + `realloc`), monotonic.
static TOTAL_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Total bytes handed out (alloc sizes + realloc new sizes), monotonic.
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
/// Live heap blocks; rises on `alloc`, falls on `dealloc`, unchanged by `realloc`.
static CURRENT_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Live heap bytes; rises on `alloc`, falls on `dealloc`, adjusted on `realloc`.
static CURRENT_BYTES: AtomicI64 = AtomicI64::new(0);
/// Global allocator that delegates to [`System`] and counts.
///
/// # Invariants
/// - Every request is forwarded unchanged to the system allocator, so the
///   returned pointers and accepted layouts are exactly what [`System`]
///   would produce; counting never influences allocation decisions.
/// - `alloc`/`dealloc` are paired with the same `Layout` by the caller (the
///   global allocator contract), so live-byte accounting stays consistent.
/// - `realloc` counts one new block, adds the new size to the totals, and
///   adjusts the live bytes by the difference against the old layout's size.
/// - Counters use `Relaxed` ordering: this binary is single-threaded and the
///   counters are only read after the measured work has finished, so no
///   ordering is required between counter updates.
struct CountingAllocator;

// SAFETY: the struct is stateless; every method forwards to System's
// implementation of the same contract and only updates counters.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegated to System with the caller-provided layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let size = u64::try_from(layout.size()).expect("layout size fits u64");
            TOTAL_BLOCKS.fetch_add(1, Ordering::Relaxed);
            TOTAL_BYTES.fetch_add(size, Ordering::Relaxed);
            CURRENT_BLOCKS.fetch_add(1, Ordering::Relaxed);
            CURRENT_BYTES.fetch_add(
                i64::try_from(size).expect("layout size fits i64"),
                Ordering::Relaxed,
            );
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegated to System; the caller guarantees the layout
        // matches the one passed to `alloc`.
        unsafe { System.dealloc(ptr, layout) };
        let size = u64::try_from(layout.size()).expect("layout size fits u64");
        CURRENT_BLOCKS.fetch_sub(1, Ordering::Relaxed);
        CURRENT_BYTES.fetch_sub(
            i64::try_from(size).expect("layout size fits i64"),
            Ordering::Relaxed,
        );
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: delegated to System with the caller-provided pointer/layout.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            TOTAL_BLOCKS.fetch_add(1, Ordering::Relaxed);
            TOTAL_BYTES.fetch_add(
                u64::try_from(new_size).expect("size fits u64"),
                Ordering::Relaxed,
            );
            let new = i64::try_from(new_size).expect("size fits i64");
            let old = i64::try_from(layout.size()).expect("layout size fits i64");
            CURRENT_BYTES.fetch_add(new - old, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// Operations for per-operation allocation accounting.
const N: u64 = 200_000;
/// Live subject+observer pairs held for retention accounting.
const M: u64 = 10_000;

fn main() {
    // Process-start baseline: the after-drop residue below is measured against
    // this, so it reflects only what the mechanism itself retains.
    let start = snapshot();

    // Phase 1: per-operation allocation cost of register + observe + complete
    // + read. Uses the monotonic totals: live bytes stay roughly flat here
    // because every subject/observer is dropped within its own iteration.
    let space = ObservationSpace::new();
    let before = snapshot();
    for key in 0..N {
        let mut subject = space.subject(key).expect("fresh key");
        let observation = space.observe(&key).expect("subject retained");
        subject.complete(key);
        std::hint::black_box(observation.try_get());
    }
    let after = snapshot();
    emit_ratio(
        "alloc_bytes_per_op",
        after.total_bytes - before.total_bytes,
        N,
    );
    emit_ratio(
        "alloc_blocks_per_op",
        after.total_blocks - before.total_blocks,
        N,
    );
    drop(space);

    // Phase 2: retained heap while M subject+observer pairs stay live.
    let mut subjects = Vec::with_capacity(usize::try_from(M).expect("fits usize"));
    let mut observations = Vec::with_capacity(usize::try_from(M).expect("fits usize"));
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let baseline = snapshot();
    for key in 0..M {
        subjects.push(space.subject(key).expect("fresh key"));
        observations.push(space.observe(&key).expect("subject retained"));
    }
    let live = snapshot();
    emit_ratio(
        "retained_bytes_per_subject_observer",
        u64::try_from(live.current_bytes - baseline.current_bytes).expect("live heap grows"),
        M,
    );
    emit_ratio(
        "retained_blocks_per_subject_observer",
        live.current_blocks - baseline.current_blocks,
        M,
    );

    // Sanity: after dropping everything, the live heap must return to the
    // process-start baseline (only the stdout buffer and runtime remain).
    drop(subjects);
    drop(observations);
    drop(space);
    let after_drop = snapshot();
    let residue = (after_drop.current_bytes - start.current_bytes).max(0);
    println!("METRIC retained_after_drop_bytes={residue}");
}

struct Snapshot {
    total_bytes: u64,
    total_blocks: u64,
    current_bytes: i64,
    current_blocks: u64,
}

fn snapshot() -> Snapshot {
    Snapshot {
        total_bytes: TOTAL_BYTES.load(Ordering::Relaxed),
        total_blocks: TOTAL_BLOCKS.load(Ordering::Relaxed),
        current_bytes: CURRENT_BYTES.load(Ordering::Relaxed),
        current_blocks: CURRENT_BLOCKS.load(Ordering::Relaxed),
    }
}

fn emit_ratio(name: &str, total: u64, count: u64) {
    let per = total / count;
    println!("METRIC {name}={per}");
}
