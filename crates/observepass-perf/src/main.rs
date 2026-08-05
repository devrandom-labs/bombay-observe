use std::time::Instant;

use observepass::ObservationSpace;

const OPERATIONS: u32 = 1_000_000;

fn main() {
    let space = ObservationSpace::new();
    let started = Instant::now();
    for key in 0..OPERATIONS {
        let mut subject = space.subject(key).unwrap();
        let observation = space.observe(&key).unwrap();
        subject.complete(key);
        std::hint::black_box(observation.try_get());
    }
    let elapsed = started.elapsed();
    let score = f64::from(OPERATIONS) / elapsed.as_secs_f64();
    println!("SCORE={score:.3}");
    println!("OBSERVATIONS_PER_SECOND={score:.3}");
}
