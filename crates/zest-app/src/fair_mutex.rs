//! A mutex that lets a waiter interpose.
//!
//! # Why the standard one is not enough
//!
//! `parking_lot::Mutex` is deliberately *unfair*: a thread releasing the lock
//! can immediately reacquire it without yielding to anyone waiting. That is the
//! right default for throughput, and it is exactly wrong here.
//!
//! The parser thread runs `lock → advance → unlock` in a tight loop. During a
//! flood — `cat` of a large file, a `cargo build` at full tilt — it will
//! reacquire the lock every time and the render thread never gets in. The window
//! freezes solid until the flood ends, which looks like a hang rather than a
//! slow terminal.
//!
//! The fix is the one Alacritty uses: a second "next to access" lock that a
//! waiter takes *first*. A thread that has just released the real lock has to
//! queue behind that waiter to get back in.
//!
//! Two mitigations are needed and this is only one of them. The parser must also
//! bound how much it does per acquisition — see `session.rs`.

use parking_lot::{Mutex, MutexGuard};

/// A mutex that guarantees a waiting thread eventually gets in.
#[derive(Debug, Default)]
pub struct FairMutex<T> {
    /// Held only for the moment it takes to claim the next turn.
    next: Mutex<()>,
    inner: Mutex<T>,
}

impl<T> FairMutex<T> {
    pub fn new(value: T) -> Self {
        Self { next: Mutex::new(()), inner: Mutex::new(value) }
    }

    /// Acquire, yielding to any thread already waiting.
    ///
    /// Taking `next` first is what makes this fair: a thread in a tight
    /// lock/unlock loop must re-queue behind anyone who has claimed the next
    /// turn, instead of barging back in.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let _next = self.next.lock();
        self.inner.lock()
    }

    /// Acquire without queueing.
    ///
    /// For the thread that is *already* looping: it has just released the lock
    /// and wants it back only if nobody else is waiting. This is what the parser
    /// uses, so a render thread waiting on `lock` always wins.
    pub fn lock_unfair(&self) -> MutexGuard<'_, T> {
        self.inner.lock()
    }

    /// Try to acquire without blocking or queueing.
    ///
    /// Wanted by the coming frame-pacing work, which needs to skip a frame
    /// rather than block when the parser is mid-chunk.
    #[allow(dead_code)]
    pub fn try_lock_unfair(&self) -> Option<MutexGuard<'_, T>> {
        self.inner.try_lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn basic_mutual_exclusion() {
        let m = Arc::new(FairMutex::new(0usize));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    *m.lock() += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*m.lock(), 8000);
    }

    /// The property the whole type exists for.
    ///
    /// A thread hammering `lock_unfair` in a tight loop -- the parser during a
    /// flood -- must not prevent a `lock` caller from ever getting in. With a
    /// plain unfair mutex this test hangs until the timeout, which is precisely
    /// the frozen window it is guarding against.
    #[test]
    fn a_tight_loop_cannot_starve_a_waiter() {
        let m = Arc::new(FairMutex::new(0usize));
        let stop = Arc::new(AtomicBool::new(false));
        let got_in = Arc::new(AtomicUsize::new(0));

        let hammer = {
            let m = Arc::clone(&m);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                // Exactly the parser's shape: acquire, do a little work, release,
                // immediately try again.
                while !stop.load(Ordering::Relaxed) {
                    let mut g = m.lock_unfair();
                    *g += 1;
                    drop(g);
                }
            })
        };

        let waiter = {
            let m = Arc::clone(&m);
            let got_in = Arc::clone(&got_in);
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let _g = m.lock();
                    got_in.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        while got_in.load(Ordering::Relaxed) < 50 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        hammer.join().unwrap();
        waiter.join().unwrap();

        assert_eq!(
            got_in.load(Ordering::Relaxed),
            50,
            "the renderer was starved by the parser -- this is the frozen-window bug"
        );
    }

    #[test]
    fn try_lock_reports_contention() {
        let m = FairMutex::new(0usize);
        let guard = m.lock();
        assert!(m.try_lock_unfair().is_none());
        drop(guard);
        assert!(m.try_lock_unfair().is_some());
    }
}
