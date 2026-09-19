//! Concurrency helpers shared across the workspace.

use std::sync::{Mutex, MutexGuard, TryLockError};

/// Mutex locking that survives a poisoned mutex instead of cascading panics.
///
/// `Mutex::lock().unwrap()` turns a `PoisonError` into a panic: once one
/// thread panics while holding the lock, every later caller panics too, which
/// can take a whole repository (or the GUI) down for the rest of the process.
/// `lock_recover` instead reclaims the guard — the protected data is still
/// there — and clears the poison flag so subsequent locks take the normal fast
/// path. See `docs/review-followups.md` (#5).
///
/// The caller is responsible for any data-specific recovery (e.g. discarding
/// an in-memory cache that a panic may have left half-updated); a plain
/// `lock_recover` only assumes the data is usable, which holds when every
/// mutation is otherwise transactional.
pub trait MutexExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;

    /// The guard if the mutex is free *right now*, `None` if another thread
    /// holds it. Poisoning is reclaimed exactly as [`Self::lock_recover`] does:
    /// a poisoned-but-free mutex is a guard, not a `None`.
    ///
    /// For the reader that has something correct to answer without the lock and
    /// only loses freshness by not waiting — waiting behind a long write is the
    /// worse answer, not the safer one.
    fn try_lock_recover(&self) -> Option<MutexGuard<'_, T>>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                // Reclaim the guard and clear the flag so later locks are not
                // permanently forced down this recovery path.
                self.clear_poison();
                poison.into_inner()
            }
        }
    }

    fn try_lock_recover(&self) -> Option<MutexGuard<'_, T>> {
        match self.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poison)) => {
                self.clear_poison();
                Some(poison.into_inner())
            }
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_recover_returns_data_after_a_poisoning_panic() {
        let m = Arc::new(Mutex::new(41));
        let m2 = m.clone();
        // Poison the mutex: panic while holding the guard.
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 42;
            panic!("boom while holding the lock");
        })
        .join();

        assert!(m.lock().is_err(), "mutex should be poisoned");
        // Recovery reclaims the guard (with the write the panicking thread made)
        // and clears the poison flag.
        assert_eq!(*m.lock_recover(), 42);
        assert!(m.lock().is_ok(), "poison flag should be cleared afterwards");
    }

    #[test]
    fn try_lock_recover_gives_up_on_a_held_mutex_and_reclaims_a_poisoned_one() {
        let m = Arc::new(Mutex::new(1));
        {
            let _held = m.lock().unwrap();
            assert!(m.try_lock_recover().is_none(), "a held mutex must not block the caller");
        }
        assert_eq!(*m.try_lock_recover().expect("a free mutex gives its guard"), 1);

        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("boom while holding the lock");
        })
        .join();
        // Poisoned but free: a guard, like `lock_recover`, not a `None`.
        assert_eq!(*m.try_lock_recover().expect("a poisoned free mutex is reclaimed"), 1);
        assert!(m.lock().is_ok(), "poison flag should be cleared afterwards");
    }
}
