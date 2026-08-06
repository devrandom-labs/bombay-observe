//! Coverage-guided fuzz target: futures, waker registration, polling, and
//! cancellation sequences. Exact wake-count assertions: a registered waker
//! of a live future fires exactly once at completion; a cancelled future's
//! wakers never fire. Futures here always poll with per-future distinct
//! wakers (the shared-waker topology deterministically hits FINDING-001,
//! preserved separately and intentionally out of this target).

#![no_main]

use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use libfuzzer_sys::fuzz_target;
use observepass::{ObservationFuture, ObservationSpace, Subject};
use observepass_autoresearch::probe::{CountWake, poll_once};

const KEYS: u8 = 2;

struct Fut {
    future: Pin<Box<ObservationFuture<u64>>>,
    key: u8,
    epoch: u64,
    wakers: Vec<Arc<CountWake>>,
    cancelled: bool,
}

fuzz_target!(|data: &[u8]| {
    let space = ObservationSpace::<u8, u64>::new();
    let mut subjects: Vec<Option<Subject<u8, u64>>> =
        (0..KEYS).map(|_| None).collect();
    let mut completed = [false; KEYS as usize];
    let mut epochs = [0_u64; KEYS as usize];
    let mut futures: Vec<Fut> = Vec::new();
    // (key, epoch) -> whether that retired epoch completed.
    let mut retired_completed: std::collections::HashMap<(u8, u64), bool> =
        std::collections::HashMap::new();
    // Wakers of cancelled futures with their fire count at cancellation:
    // nothing may fire them afterwards (checked at teardown).
    let mut cancelled_probes: Vec<(Arc<CountWake>, usize)> = Vec::new();

    for pair in data.chunks_exact(2).take(128) {
        let (op, key) = (pair[0] % 6, pair[1] % KEYS);
        let k = usize::from(key);
        match op {
            0 => {
                if let Ok(subject) = space.subject(key) {
                    epochs[k] += 1;
                    completed[k] = false;
                    subjects[k] = Some(subject);
                }
            }
            1 => {
                if let Some(subject) = subjects[k].as_mut() {
                    if !completed[k] {
                        let value = (epochs[k] << 8) | u64::from(key);
                        subject.complete(value);
                        completed[k] = true;
                        // Every live future of this generation: each
                        // distinct registered waker fires exactly once.
                        for f in &futures {
                            if f.key == key && f.epoch == epochs[k] && !f.cancelled {
                                for probe in &f.wakers {
                                    assert_eq!(probe.count(), 1, "waker fired != once");
                                }
                            }
                        }
                    }
                }
            }
            2 => {
                if let Ok(obs) = space.observe(&key) {
                    futures.push(Fut {
                        future: Box::pin(obs.into_future()),
                        key,
                        epoch: epochs[k],
                        wakers: Vec::new(),
                        cancelled: false,
                    });
                }
            }
            3 => {
                if let Some(f) = futures.last_mut() {
                    if !f.cancelled {
                        let (waker, probe) = CountWake::waker();
                        let k2 = usize::from(f.key);
                        let expected = if epochs[k2] == f.epoch {
                            completed[k2]
                        } else {
                            retired_completed.get(&(f.key, f.epoch)) == Some(&true)
                        };
                        match poll_once(f.future.as_mut(), &waker) {
                            Poll::Ready(value) => assert!(expected, "ready while pending: {value}"),
                            Poll::Pending => {
                                assert!(!expected, "pending while completed");
                                f.wakers.push(probe);
                            }
                        }
                    }
                }
            }
            4 => {
                if let Some(subject) = subjects[k].take() {
                    retired_completed.insert((key, epochs[k]), completed[k]);
                    drop(subject); // retire
                }
            }
            _ => {
                if let Some(mut f) = futures.pop() {
                    f.cancelled = true;
                    for probe in &f.wakers {
                        cancelled_probes.push((Arc::clone(probe), probe.count()));
                    }
                    drop(f.future);
                }
            }
        }
    }
    for (probe, at_cancel) in &cancelled_probes {
        assert_eq!(
            probe.count(),
            *at_cancel,
            "a cancelled future's waker fired after cancellation"
        );
    }
});
