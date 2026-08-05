use criterion::{Criterion, criterion_group, criterion_main};
use observepass::ObservationSpace;

fn complete_and_observe(c: &mut Criterion) {
    c.bench_function("complete_then_observe", |b| {
        let space = ObservationSpace::new();
        let mut key = 0_u64;
        b.iter(|| {
            key += 1;
            let mut subject = space.subject(key).unwrap();
            subject.complete(key);
            let value = space.observe(&key).unwrap().try_get();
            std::hint::black_box(value);
        });
    });
}
criterion_group!(benches, complete_and_observe);
criterion_main!(benches);
