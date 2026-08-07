use observe::ObservationSpace;

#[test]
fn observe_before_and_after_completion_receive_the_same_outcome() {
    let space = ObservationSpace::new();
    let mut subject = space.subject(7_u64).unwrap();
    let early = space.observe(&7).unwrap();
    subject.complete("stopped");
    let late = space.observe(&7).unwrap();
    assert_eq!(early.try_get(), Some("stopped"));
    assert_eq!(late.try_get(), Some("stopped"));
}

#[test]
fn dropping_subject_retires_only_its_generation() {
    let space = ObservationSpace::<u64, ()>::new();
    let subject = space.subject(7_u64).unwrap();
    drop(subject);
    let replacement = space.subject(7_u64).unwrap();
    assert!(space.observe(&7).is_ok());
    drop(replacement);
}
