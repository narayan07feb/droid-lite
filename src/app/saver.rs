//! The one background thread that writes saves and local history, so the window never waits for
//! the disk (a save syncs the file: tens of milliseconds on macOS, more on a slow disk). Jobs run
//! one at a time in the order they were sent: two saves of one file land in that order.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

type Job = Box<dyn FnOnce() + Send>;

/// Runs `job` on the save thread, after every job sent before it. A job that panics does not stop
/// the thread.
pub(super) fn run(job: impl FnOnce() + Send + 'static) {
    static QUEUE: OnceLock<Sender<Job>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("droid-lite-save".into())
            .spawn(move || {
                for job in rx {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            })
            .expect("cannot start the save thread");
        tx
    });
    let _ = queue.send(Box::new(job));
}

/// Waits until every job sent before has run (tests).
#[cfg(test)]
pub(super) fn wait() {
    while !wait_for(std::time::Duration::from_secs(3600)) {}
}

/// Waits until every job sent before has run, `limit` at most: false when one still runs (a
/// save to a slow or stuck disk).
pub(super) fn wait_for(limit: std::time::Duration) -> bool {
    let (tx, rx) = channel();
    run(move || {
        let _ = tx.send(());
    });
    rx.recv_timeout(limit).is_ok()
}

/// What a tab had when ⌘S (or auto-save) was pressed.
pub(super) struct SaveJob {
    pub path: PathBuf,
    pub text: String,
    /// The text last loaded from or saved to disk.
    pub saved: String,
    pub dirty: bool,
    /// Auto-save: never writes over a change on disk, nor brings back a deleted file.
    pub auto: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    Written,
    /// Not written: the file changed on disk (the message says why).
    DiskChanged(String),
    /// Not written: the file was deleted on disk.
    Deleted(String),
    Failed(String),
}

pub(super) struct SaveDone {
    pub path: PathBuf,
    pub text: String,
    pub outcome: Outcome,
}

thread_local! {
    /// What this thread last wrote to each file, while the window has not applied that save yet:
    /// a second save queued behind the first finds its own text on disk, not someone else's.
    static WRITTEN: RefCell<HashMap<PathBuf, u64>> = RefCell::new(HashMap::new());
}

fn hash(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// Writes `job` with `write` unless that would drop another version: what is on disk goes to
/// local history before it is replaced, and a change on disk the tab has not seen is kept (auto-
/// save, or a tab without edits). Runs on the save thread.
pub(super) fn save(job: &SaveJob, write: fn(&Path, &[u8]) -> std::io::Result<()>) -> Outcome {
    let path = &job.path;
    let mine = |disk: &[u8]| {
        WRITTEN.with_borrow_mut(|w| {
            let last = w.get(path).copied();
            // the window has applied that save: its own record is enough from now on
            if last == Some(hash(job.saved.as_bytes())) {
                w.remove(path);
            }
            last == Some(hash(disk))
        })
    };
    match std::fs::read(path) {
        Ok(disk) if disk == job.text.as_bytes() => {}
        Ok(disk) => {
            // changed on disk since it was loaded or saved here (the watcher may not know yet)
            let theirs = disk != job.saved.as_bytes() && !mine(&disk);
            if theirs && job.auto {
                return Outcome::DiskChanged(format!(
                    "Auto-save did not write {}: it changed on disk; reload or keep yours",
                    path.display()
                ));
            }
            if theirs && !job.dirty {
                return Outcome::DiskChanged("File changed on disk; reload or keep yours".into());
            }
            if let Err(e) = super::comfort::record_text(path, &disk)
                && theirs
            {
                return Outcome::Failed(format!(
                    "{}. Not saved: the other version on disk stays",
                    super::comfort::copy_failed(path, &e)
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if job.auto && !job.saved.is_empty() {
                return Outcome::Deleted(format!(
                    "Auto-save did not write {}: it was deleted on disk",
                    path.display()
                ));
            }
        }
        Err(e) => {
            return Outcome::Failed(format!("Save failed: cannot read {}: {e}", path.display()));
        }
    }
    match write(path, job.text.as_bytes()) {
        Ok(()) => {
            WRITTEN.with_borrow_mut(|w| w.insert(path.clone(), hash(job.text.as_bytes())));
            Outcome::Written
        }
        Err(e) => Outcome::Failed(format!("Save failed: {e}")),
    }
}
