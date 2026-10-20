//! A panic under one lock must not take every later request with it.
use std::sync::{Mutex, MutexGuard, PoisonError};

pub trait Locked<T> {
    /// The guard even when a holder panicked: the state is plain data, so keep serving.
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_lock_still_opens() {
        let m = Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.locked();
            panic!("poison");
        })
        .join();
        assert!(m.is_poisoned());
        *m.locked() += 1;
        assert_eq!(*m.locked(), 2);
    }
}
