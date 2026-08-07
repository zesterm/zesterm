//! Reloading the config when it changes on disk.
//!
//! Two things make this less trivial than "notify says the file changed":
//!
//! Editors do not write files, they replace them. A save is often
//! write-temp-then-rename, which arrives as a *remove* of the path being watched
//! and leaves a watcher on a file that no longer exists. Watching the containing
//! directory instead survives that.
//!
//! And one save produces several events. Debouncing is not polish here — reacting
//! to the first of them reads a half-written file, which then fails to parse, and
//! the user gets an error toast for a file that is perfectly fine by the time they
//! look at it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the file must be quiet before a reload.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Watches a config file and calls back when it settles.
///
/// Holds the `notify` watcher: dropping this stops the watching.
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

impl Watcher {
    /// Watch `path`, calling `on_change` after each settled write.
    ///
    /// The callback runs on a background thread, so it must not touch the
    /// renderer. The app's use is to post a wakeup and reload on the main thread.
    pub fn new<F>(path: &Path, on_change: F) -> notify::Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        use notify::Watcher as _;

        let watched = path.to_path_buf();
        // The directory, not the file: an atomic-rename save removes the inode
        // the file watch was attached to, and every later save goes unnoticed.
        let dir = path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        std::fs::create_dir_all(&dir).ok();

        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if event.paths.iter().any(|p| p == &watched) {
                let _ = tx.send(());
            }
        })?;
        watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;

        let thread = std::thread::Builder::new()
            .name("zest-config-watch".into())
            .spawn(move || debounce_loop(&rx, &on_change))
            .expect("spawn config watcher");

        Ok(Self { _inner: watcher, _thread: thread })
    }
}

/// Collapse a burst of events into one callback.
fn debounce_loop<F: Fn()>(rx: &mpsc::Receiver<()>, on_change: &F) {
    while rx.recv().is_ok() {
        // Keep swallowing events until the file has been quiet for DEBOUNCE.
        let mut last = Instant::now();
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(()) => last = Instant::now(),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last.elapsed() >= DEBOUNCE {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        on_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_burst_of_events_produces_one_callback() {
        // The property that keeps a save from being read half-written: several
        // events close together must collapse into a single reload.
        let (tx, rx) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);

        let handle = std::thread::spawn(move || {
            debounce_loop(&rx, &move || {
                seen.fetch_add(1, Ordering::SeqCst);
            });
        });

        for _ in 0..10 {
            tx.send(()).expect("send");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(DEBOUNCE * 3);
        assert_eq!(count.load(Ordering::SeqCst), 1, "a burst was not coalesced");

        // A later, separate save is its own callback.
        tx.send(()).expect("send");
        std::thread::sleep(DEBOUNCE * 3);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        drop(tx);
        handle.join().expect("watcher thread");
    }

    #[test]
    fn a_real_save_is_noticed() {
        let dir = std::env::temp_dir().join("zesterm-watch-test");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# initial\n").expect("write");

        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        let _watcher = Watcher::new(&path, move || {
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watch");

        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&path, "# changed\n").expect("write");

        // Generous: filesystem notifications are not instant, and a flaky test
        // here would get muted rather than fixed.
        let deadline = Instant::now() + Duration::from_secs(5);
        while hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(hits.load(Ordering::SeqCst) >= 1, "the save went unnoticed");

        let _ = std::fs::remove_file(&path);
    }
}
