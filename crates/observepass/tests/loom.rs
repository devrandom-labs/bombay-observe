#![cfg(loom)]

use loom::sync::{Arc, Mutex};
use loom::thread;

#[test]
fn registration_racing_completion_cannot_lose_a_published_outcome() {
    loom::model(|| {
        let slot = Arc::new(Mutex::new(None));
        let publisher = slot.clone();
        let observer = slot.clone();
        let publish = thread::spawn(move || *publisher.lock().unwrap() = Some(9));
        let observe = thread::spawn(move || {
            let _snapshot = *observer.lock().unwrap();
        });
        publish.join().unwrap();
        observe.join().unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(9));
    });
}
