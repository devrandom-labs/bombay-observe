//! Extended performance harness for observepass.
//!
//! Emits `METRIC name=value` lines consumed by `autoresearch.sh`. Every
//! workload uses fixed operation counts and disjoint key ranges, with no RNG,
//! so runs are reproducible.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use observepass::ObservationSpace;

/// Operations per throughput scenario.
const N: u64 = 1_000_000;
/// Operations for the percentile-latency scenario.
const N_LAT: u64 = 200_000;
/// Observers per subject in the fanout scenario.
const FANOUT: usize = 8;
/// Subjects in the fanout scenario.
const M_FANOUT: u64 = 100_000;

fn main() {
    throughput("seq_observe_first", seq_observe_first);
    throughput("seq_complete_first", seq_complete_first);
    throughput("seq_retire_recreate", seq_retire_recreate);
    throughput("seq_cancel_observer", seq_cancel_observer);
    fanout();
    latency();
    wait_latency();
    // Scaling: 1/2/4/8/16 threads (16 oversubscribes the machine's 12
    // physical cores, exposing scheduling degradation).
    for threads in [1_usize, 2, 4, 8, 16] {
        contention(threads);
    }
}

/// Observe-first registration (the shape of the frozen workload).
fn seq_observe_first() -> Duration {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..N {
        let mut subject = space.subject(key).expect("fresh key");
        let observation = space.observe(&key).expect("subject retained");
        subject.complete(key);
        black_box(observation.try_get());
    }
    started.elapsed()
}

/// Complete-first: the late observer reads a retained outcome.
fn seq_complete_first() -> Duration {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..N {
        let mut subject = space.subject(key).expect("fresh key");
        subject.complete(key);
        let observation = space.observe(&key).expect("subject retained");
        black_box(observation.try_get());
    }
    started.elapsed()
}

/// Rapid address reuse: register then retire the same key repeatedly.
fn seq_retire_recreate() -> Duration {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..N {
        space.subject(key).expect("fresh key");
    }
    started.elapsed()
}

/// Cancellation: dropping observers must not obstruct completion.
fn seq_cancel_observer() -> Duration {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..N {
        let mut subject = space.subject(key).expect("fresh key");
        let observation = space.observe(&key).expect("subject retained");
        drop(observation);
        subject.complete(key);
    }
    started.elapsed()
}

/// Peer fanout: one subject, FANOUT observers, complete, all read.
fn fanout() {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..M_FANOUT {
        let mut subject = space.subject(key).expect("fresh key");
        let mut observations = Vec::with_capacity(FANOUT);
        for _ in 0..FANOUT {
            observations.push(space.observe(&key).expect("subject retained"));
        }
        subject.complete(key);
        for observation in &observations {
            black_box(observation.try_get());
        }
    }
    let elapsed = started.elapsed();
    emit_rate(
        "fanout_observations_per_second",
        M_FANOUT * u64::try_from(FANOUT).expect("fits u64"),
        elapsed,
    );
}

/// Latency percentiles of the hot path (observe-first).
fn latency() {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let mut latencies = Vec::with_capacity(usize::try_from(N_LAT).expect("fits usize"));
    for key in 0..N_LAT {
        let started = Instant::now();
        let mut subject = space.subject(key).expect("fresh key");
        let observation = space.observe(&key).expect("subject retained");
        subject.complete(key);
        black_box(observation.try_get());
        latencies.push(started.elapsed());
    }
    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2].as_nanos();
    let p99 = latencies[latencies.len() - latencies.len() / 100].as_nanos();
    println!("METRIC hot_path_p50_ns={p50}");
    println!("METRIC hot_path_p99_ns={p99}");
}

/// Wait round-trip latency: one persistent waiter thread races completion
/// across rounds; measures the blocking wait path end to end.
fn wait_latency() {
    use std::sync::atomic::{AtomicU64, Ordering};

    const ROUNDS: u64 = 50_000;
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    // The waiter signals after observing each round's subject, so the main
    // thread never retires a subject the waiter has not yet observed.
    let observed = Arc::new(AtomicU64::new(0));
    let waiter = std::thread::spawn({
        let space = space.clone();
        let observed = Arc::clone(&observed);
        move || {
            let mut latencies = Vec::with_capacity(usize::try_from(ROUNDS).expect("fits usize"));
            for key in 0..ROUNDS {
                let observation = loop {
                    if let Ok(observation) = space.observe(&key) {
                        break observation;
                    }
                    std::thread::yield_now();
                };
                observed.store(key + 1, Ordering::Release);
                let started = Instant::now();
                let outcome = observation.wait();
                latencies.push(started.elapsed());
                assert_eq!(outcome, key);
            }
            latencies
        }
    });
    for key in 0..ROUNDS {
        let mut subject = space.subject(key).expect("fresh key");
        while observed.load(Ordering::Acquire) < key + 1 {
            std::thread::yield_now();
        }
        subject.complete(key);
    }
    let mut latencies = waiter.join().expect("waiter completes");
    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2].as_nanos();
    let p99 = latencies[latencies.len() - latencies.len() / 100].as_nanos();
    println!("METRIC wait_roundtrip_p50_ns={p50}");
    println!("METRIC wait_roundtrip_p99_ns={p99}");
}

/// Contention scaling: disjoint key ranges raced by `threads` workers.
fn contention(threads: usize) {
    let space: ObservationSpace<u64, u64> = ObservationSpace::new();
    let per_thread = N / u64::try_from(threads).expect("fits u64");
    let barrier = Barrier::new(threads + 1);
    let started = std::thread::scope(|scope| {
        for worker in 0..threads {
            let space = space.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let mut checked = 0_u64;
                let base = u64::try_from(worker).expect("fits u64") * per_thread;
                for offset in 0..per_thread {
                    let key = base + offset;
                    let mut subject = space.subject(key).expect("disjoint keys");
                    let observation = space.observe(&key).expect("subject retained");
                    subject.complete(key);
                    if observation.try_get().is_some() {
                        checked += 1;
                    }
                }
                black_box(checked);
            });
        }
        barrier.wait();
        Instant::now()
    });
    let elapsed = started.elapsed();
    emit_rate(&format!("contention_{threads}t_ops_per_second"), N, elapsed);
}

fn throughput(name: &str, run: impl FnOnce() -> Duration) {
    let elapsed = run();
    emit_rate(&format!("{name}_ops_per_second"), N, elapsed);
}

#[allow(
    clippy::cast_precision_loss,
    reason = "operation counts are <= 2^32 and exact in f64"
)]
fn emit_rate(name: &str, operations: u64, elapsed: Duration) {
    let rate = operations as f64 / elapsed.as_secs_f64();
    println!("METRIC {name}={rate:.3}");
}
