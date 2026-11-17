//! Project-wide symbol index: declarations of every Kotlin/Java file, fuzzy search, definitions, completion.

use crate::kotlin::{Atom, Decl, FileSyms, Kind, scan_file};
use crate::project::is_code;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Default)]
pub struct Index {
    /// Each file's symbols, shared with the tab that read them (no copy per typing pause).
    pub files: HashMap<PathBuf, Arc<FileSyms>>,
    /// Each name's declarations (file and place in its list), so a lookup never scans a file's
    /// other declarations (lenses of a file with 20k declarations look up each one).
    by_name: HashMap<String, Vec<(Arc<Path>, u32)>>,
    /// Files with a class that names each supertype, for "implemented by" lookups.
    subtypes: HashMap<String, Vec<Arc<Path>>>,
    /// One shared copy of each file's path for the maps above (a copy per name cost ~20 MB on a
    /// 12k-file project), sorted, so the files under a folder are one range.
    shared: BTreeMap<PathBuf, Arc<Path>>,
    /// Bumped on every change, so callers can cache results.
    pub generation: u64,
    /// The latest changed files with the generation each change made, so a cached result can
    /// tell whether a file it depends on changed (bounded; older changes are only counted).
    changes: std::collections::VecDeque<(u64, Arc<Path>)>,
    /// Generations up to this one belong to an index this one replaced (another project, or the
    /// same one read again): nothing changed since then can be told.
    floor: u64,
    /// Counts the indexes [`Shared::set`] put in: a file read again for one never goes into
    /// the next.
    epoch: u64,
}

/// How many file changes the index remembers.
const CHANGES_KEPT: usize = 2048;

/// The index shared by the editor and the one background worker. The worker reads it a short
/// chunk at a time; the editor never waits for it: its changes queue up and are applied by the
/// next [`Shared::flush`] that finds the index free (the editor's next frame, or the worker
/// before its next chunk). A poisoned lock still holds a usable index.
#[derive(Clone, Default)]
pub struct Shared {
    index: Arc<std::sync::RwLock<Index>>,
    queue: Arc<std::sync::Mutex<Vec<Change>>>,
    /// Files a failed change took out, to read again from disk on the worker ([`Shared::rescan`]),
    /// each with the index it was taken out of ([`Index::epoch`]): one of an index
    /// [`Shared::set`] replaced since is not put into the new one.
    rescan: Arc<std::sync::Mutex<Vec<(u64, PathBuf)>>>,
    /// A failed change may have left names without their file: the name maps are rebuilt from
    /// the files on the worker ([`Shared::rescan`]), never under the lock on the editor's thread.
    repair: Arc<std::sync::atomic::AtomicBool>,
    /// When that rebuild may start: after one lost to a newer change (typing) or failed, it waits
    /// instead of starting a full rebuild every frame.
    repair_wait: Arc<std::sync::Mutex<RepairWait>>,
    /// Asks the editor for a frame ([`Shared::on_wake`]): this thread cannot apply the queued
    /// changes at the editor's priority, its next frame does.
    wake: Arc<std::sync::Mutex<Option<Wake>>>,
}

/// What [`Shared::on_wake`] calls.
type Wake = Box<dyn Fn() + Send>;

/// The pause before a rebuild of the name maps after one that lost or failed
/// ([`Shared::repair`]); it doubles with each loss in a row, up to 16 times.
const REPAIR_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// When the next rebuild of the name maps may start.
#[derive(Default)]
struct RepairWait {
    not_before: Option<std::time::Instant>,
    /// Rebuilds in a row a newer change made useless.
    lost: u32,
    /// The last rebuild failed: it is tried once more, then left (lookups skip names without
    /// their file).
    failed: bool,
}

/// What a change changes, which says what to do when it fails halfway.
enum Of {
    /// Anything: the index's name maps are rebuilt from its files.
    Any,
    /// One file: it is taken out and read again from disk.
    File(PathBuf),
    /// That reading again: the file is taken out (no second try).
    Rescan(PathBuf),
}

type Change = (Of, Box<dyn FnOnce(&mut Index) + Send>);

impl Shared {
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Index> {
        self.index.read().unwrap_or_else(|e| e.into_inner())
    }
    /// Queues a change and applies it now when nothing reads the index.
    pub fn change(&self, f: impl FnOnce(&mut Index) + Send + 'static) {
        self.queue(Of::Any, Box::new(f));
    }
    fn queue(&self, of: Of, f: Box<dyn FnOnce(&mut Index) + Send>) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((of, f));
        self.flush();
    }
    /// A file's text changed: its symbols are read here, before queueing, so the index's lock
    /// is held only to put them in.
    pub fn update(&self, path: &Path, text: &str) {
        if is_code(path) {
            self.put(path.to_path_buf(), scan_file(path, text));
        }
    }
    /// [`Shared::update`] with the symbols already read (a tab read them for its own use).
    pub fn put(&self, path: PathBuf, syms: impl Into<Arc<FileSyms>>) {
        let syms = syms.into();
        if is_code(&path) {
            let of = Of::File(path.clone());
            self.queue(of, Box::new(move |i| i.insert_scanned(path, syms)));
        }
    }
    /// A file is gone (deleted, or moved away).
    pub fn remove(&self, path: PathBuf) {
        let of = Of::File(path.clone());
        self.queue(of, Box::new(move |i| i.remove(&path)));
    }
    /// Applies the queued changes if the index is free; returns whether none are left. The
    /// queue is let go before they run: queueing another never waits for them. A change that
    /// panics is logged and skipped; the ones after it still go in. The file a change of one
    /// file left half done is taken out, and the worker reads it again from disk; names any
    /// failed change left without their file are dropped when the worker rebuilds the name maps.
    pub fn flush(&self) -> bool {
        if self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return true;
        }
        // the editor may wait on this lock: its holder runs at the editor's priority, taken
        // before the queue's lock so queueing never waits for it
        let Some(_boost) = Boost::start() else {
            // the editor's next frame applies them
            if let Some(wake) = &*self.wake.lock().unwrap_or_else(|e| e.into_inner()) {
                wake();
            }
            return false;
        };
        let mut index = match self.index.try_write() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let changes = std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()));
        let mut again = Vec::new();
        for (of, f) in changes {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut index)));
            if ran.is_ok() {
                continue;
            }
            eprintln!("droid-lite: an index change failed; the others go on");
            // the file out with its names; the name maps are rebuilt on the worker
            if let Of::File(p) | Of::Rescan(p) = &of {
                index.take_out(p);
            }
            index.floor = index.generation;
            index.changes.clear();
            self.repair.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Of::File(p) = of {
                again.push((index.epoch, p));
            }
        }
        drop(index);
        if !again.is_empty() {
            self.rescan
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append(&mut again);
        }
        true
    }
    /// Calls `wake` when a thread that cannot apply the queued changes leaves them for the
    /// editor (Linux: a worker below the editor's priority cannot raise it again).
    pub fn on_wake(&self, wake: impl Fn() + Send + 'static) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(wake));
    }
    /// Whether the worker has files to read again or name maps to rebuild now
    /// ([`Shared::rescan`]).
    pub fn rescan_due(&self) -> bool {
        (self.repair.load(std::sync::atomic::Ordering::SeqCst) && self.repair_wait().is_none())
            || !self
                .rescan
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }
    /// How long a rebuild of the name maps that is due waits yet, after one that lost or failed.
    pub fn repair_wait(&self) -> Option<std::time::Duration> {
        if !self.repair.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let w = self.repair_wait.lock().unwrap_or_else(|e| e.into_inner());
        w.not_before?.checked_duration_since(std::time::Instant::now())
    }
    /// Ends the wait before the next rebuild of the name maps (tests: no sleeping).
    #[cfg(test)]
    pub fn skip_repair_wait(&self) {
        self.repair_wait
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .not_before = None;
    }
    /// On the worker, never the editor's thread: reads the files a failed change took out again
    /// from disk and puts each back (unless a newer change put it in meanwhile, or another index
    /// replaced that one), then rebuilds the name maps a failed change may have broken.
    pub fn rescan(&self) {
        let files = std::mem::take(&mut *self.rescan.lock().unwrap_or_else(|e| e.into_inner()));
        let read: Vec<_> = files
            .into_iter()
            .filter_map(|(epoch, p)| {
                let text = std::fs::read_to_string(&p).ok()?;
                let syms = std::panic::catch_unwind(|| scan_file(&p, &text)).ok()?;
                Some((epoch, p, syms))
            })
            .collect();
        #[cfg(test)]
        if let Some(f) = test_hooks::WHILE_READING_AGAIN.take() {
            f();
        }
        for (epoch, p, syms) in read {
            let of = Of::Rescan(p.clone());
            self.queue(
                of,
                Box::new(move |i| {
                    if i.epoch == epoch && !i.files.contains_key(&p) {
                        i.insert_scanned(p, syms);
                    }
                }),
            );
        }
        self.repair();
    }
    /// Rebuilds the name maps from the files, off the lock; they go in only when nothing
    /// changed the index meanwhile (else they are rebuilt again after a pause, longer with each
    /// loss in a row). A rebuild that fails keeps the maps the index has and is tried once more
    /// after a pause.
    fn repair(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.repair_wait().is_some() || !self.repair.swap(false, SeqCst) {
            return;
        }
        self.flush();
        let (files, generation, epoch) = {
            let i = self.read();
            (i.files.clone(), i.generation, i.epoch)
        };
        #[cfg(test)]
        if let Some(f) = test_hooks::WHILE_REBUILDING.take() {
            f();
        }
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut n = Index::default();
            for (p, f) in files {
                n.insert_scanned(p, f);
            }
            n
        }));
        let Ok(n) = built else {
            let mut w = self.repair_wait.lock().unwrap_or_else(|e| e.into_inner());
            if w.failed {
                eprintln!(
                    "droid-lite: the index's names could not be rebuilt; they stay as they are"
                );
                *w = RepairWait::default();
            } else {
                w.failed = true;
                w.not_before = Some(std::time::Instant::now() + REPAIR_PAUSE);
                self.repair.store(true, SeqCst);
            }
            return;
        };
        let due = self.repair.clone();
        let wait = self.repair_wait.clone();
        self.change(move |i| {
            // another index replaced that one ([`Shared::set`]): nothing to repair in it
            if i.epoch != epoch {
                return;
            }
            let mut w = wait.lock().unwrap_or_else(|e| e.into_inner());
            if i.generation != generation {
                w.lost = (w.lost + 1).min(5);
                w.not_before =
                    Some(std::time::Instant::now() + REPAIR_PAUSE * (1 << (w.lost - 1)));
                due.store(true, SeqCst);
                return;
            }
            *w = RepairWait::default();
            i.by_name = n.by_name;
            i.subtypes = n.subtypes;
            i.shared = n.shared;
            i.generation += 1;
            i.floor = i.generation;
            i.changes.clear();
        });
    }
    /// Replaces the index. Generations keep counting up from the old one's, so a result cached
    /// against the old index is never taken for one of the new. Files the old one waits to read
    /// again are not read into it.
    pub fn set(&self, mut index: Index) {
        self.rescan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // its name maps are built from its files: nothing to rebuild
        self.repair.store(false, std::sync::atomic::Ordering::SeqCst);
        *self.repair_wait.lock().unwrap_or_else(|e| e.into_inner()) = RepairWait::default();
        self.change(move |i| {
            index.generation += i.generation + 1;
            index.floor = index.generation;
            index.epoch = i.epoch + 1;
            index.changes.clear();
            *i = index;
        });
    }
    /// Applies a change of `p` that dies halfway (tests).
    #[cfg(test)]
    pub fn fail_change_of(&self, p: &Path) {
        let reading = self.read();
        let q = p.to_path_buf();
        self.queue(
            Of::File(q.clone()),
            Box::new(move |i: &mut Index| {
                i.by_name
                    .entry("B".into())
                    .or_default()
                    .push((Arc::from(q.as_path()), 0));
                panic!("cut short");
            }),
        );
        drop(reading);
        self.settle();
    }
    /// Waits until the queued changes are in (the worker never holds the lock long).
    #[cfg(test)]
    pub fn settle(&self) {
        while !self.flush() {
            std::thread::yield_now();
        }
    }
}

/// Runs the thread at the editor's priority while it lives: a thread below it (the worker)
/// that holds the index's write lock would make the editor wait behind lower work. `None` where
/// that cannot be done (Linux: an unprivileged thread cannot lower its nice value again); the
/// editor's next frame applies the changes then ([`Shared::on_wake`]).
struct Boost {
    #[cfg(target_os = "macos")]
    was: Option<(libc::qos_class_t, i32)>,
}

impl Boost {
    fn start() -> Option<Boost> {
        #[cfg(test)]
        if test_hooks::boost_refused() {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            use libc::qos_class_t::*;
            let mut class = QOS_CLASS_UNSPECIFIED;
            let mut rel = 0;
            unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut rel);
            }
            let low = matches!(class, QOS_CLASS_UTILITY | QOS_CLASS_BACKGROUND);
            if low {
                unsafe {
                    libc::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
                }
            }
            Some(Boost {
                was: low.then_some((class, rel)),
            })
        }
        #[cfg(target_os = "linux")]
        {
            // below the editor: niced past the process's first thread (the editor's); a whole
            // process started with `nice` is not
            let (me, editor) = unsafe {
                (
                    libc::getpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t),
                    libc::getpriority(libc::PRIO_PROCESS, libc::getpid() as libc::id_t),
                )
            };
            (!below_the_editor(me, editor)).then_some(Boost {})
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Some(Boost {})
    }
}

/// Whether a thread at nice value `me` runs below the editor's thread at `editor`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn below_the_editor(me: i32, editor: i32) -> bool {
    me > editor
}

/// Ways for tests to make this thread's next index steps fail or wait.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::{Cell, RefCell};
    use std::sync::mpsc::{Receiver, Sender};

    thread_local! {
        /// [`super::Boost::start`] refuses on this thread (Linux below the editor).
        pub static NO_BOOST: Cell<bool> = const { Cell::new(false) };
        /// [`super::Index::insert_scanned`] panics on this thread.
        pub static PANIC_ON_INSERT: Cell<bool> = const { Cell::new(false) };
        /// [`super::Boost::start`] says it started on the first and waits for the second.
        pub static BOOST_GATE: RefCell<Option<(Sender<()>, Receiver<()>)>> =
            const { RefCell::new(None) };
        /// Runs in [`super::Shared::rescan`] after the files were read, before they are put in.
        pub static WHILE_READING_AGAIN: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
        /// Runs in [`super::Shared::repair`] after the files were read, before the rebuild.
        pub static WHILE_REBUILDING: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
    }

    pub(super) fn boost_refused() -> bool {
        BOOST_GATE.with_borrow(|g| {
            if let Some((entered, go)) = g {
                let _ = entered.send(());
                let _ = go.recv_timeout(std::time::Duration::from_secs(30));
            }
        });
        NO_BOOST.get()
    }
}

impl Drop for Boost {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some((class, rel)) = self.was {
            unsafe {
                libc::pthread_set_qos_class_self_np(class, rel);
            }
        }
    }
}

/// One search hit.
#[derive(Clone, Debug)]
pub struct Hit {
    pub name: String,
    pub detail: String,
    pub path: PathBuf,
    pub offset: usize,
    pub score: u32,
}

impl Index {
    /// Scans all code files on all cores.
    pub fn build(paths: &[PathBuf]) -> Index {
        let code: Vec<&PathBuf> = paths.iter().filter(|p| is_code(p)).collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = code.len().div_ceil(threads).max(1);
        let scanned: Vec<(PathBuf, FileSyms)> = std::thread::scope(|s| {
            let jobs: Vec<_> = code
                .chunks(chunk)
                .map(|c| {
                    s.spawn(move || {
                        let mut seen = HashSet::new();
                        c.iter()
                            .filter_map(|p| {
                                let mut f = scan_file(p, &std::fs::read_to_string(p).ok()?);
                                share(&mut f, &mut seen);
                                Some(((*p).clone(), f))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            jobs.into_iter()
                .flat_map(|j| j.join().unwrap_or_default())
                .collect()
        });
        let mut index = Index::default();
        for (p, f) in scanned {
            index.insert_scanned(p, f);
        }
        index
    }

    /// Adds a file whose symbols were already scanned (e.g. on a worker thread).
    pub fn insert_scanned(&mut self, path: PathBuf, syms: impl Into<Arc<FileSyms>>) {
        #[cfg(test)]
        assert!(!test_hooks::PANIC_ON_INSERT.get(), "a bug while inserting");
        let syms = syms.into();
        self.remove(&path);
        let shared: Arc<Path> = Arc::from(path.as_path());
        for (i, d) in syms.decls.iter().enumerate() {
            let at = (shared.clone(), i as u32);
            self.by_name.entry(d.name.clone()).or_default().push(at);
        }
        for k in supers_of(&syms) {
            self.subtypes.entry(k).or_default().push(shared.clone());
        }
        self.shared.insert(path.clone(), shared.clone());
        self.files.insert(path, syms);
        self.generation += 1;
        self.log_change(shared);
    }

    fn log_change(&mut self, p: Arc<Path>) {
        if self.changes.len() == CHANGES_KEPT {
            self.changes.pop_front();
        }
        self.changes.push_back((self.generation, p));
    }

    /// Files changed after generation `since`, or `None` when that is older than the log.
    pub fn changed_since(&self, since: u64) -> Option<Vec<Arc<Path>>> {
        if since < self.floor {
            return None;
        }
        let oldest = self.changes.front().map_or(self.generation, |(g, _)| *g);
        if since + 1 < oldest && self.changes.len() == CHANGES_KEPT {
            return None;
        }
        Some(
            self.changes
                .iter()
                .filter(|(g, _)| *g > since)
                .map(|(_, p)| p.clone())
                .collect(),
        )
    }

    pub fn update(&mut self, path: &Path, src: &str) {
        if is_code(path) {
            self.insert_scanned(path.to_path_buf(), scan_file(path, src));
        }
    }

    pub fn remove(&mut self, path: &Path) {
        let Some(old) = self.files.remove(path) else {
            return;
        };
        if let Some(p) = self.shared.remove(path) {
            self.generation += 1;
            self.log_change(p);
        }
        for k in names_of(&old) {
            if let Some(v) = self.by_name.get_mut(&k) {
                v.retain(|(p, _)| **p != *path);
                if v.is_empty() {
                    self.by_name.remove(&k);
                }
            }
        }
        for k in supers_of(&old) {
            if let Some(v) = self.subtypes.get_mut(&k) {
                v.retain(|p| **p != *path);
                if v.is_empty() {
                    self.subtypes.remove(&k);
                }
            }
        }
        self.generation += 1;
    }

    /// Takes `path` out of the files after a change of it failed halfway, with every name and
    /// supertype entry of it (the ones the failed change put in too): putting the file back
    /// later adds each of its names once. A scan of the maps, only when a change failed.
    fn take_out(&mut self, path: &Path) {
        self.files.remove(path);
        self.shared.remove(path);
        self.by_name.retain(|_, v| {
            v.retain(|(p, _)| **p != *path);
            !v.is_empty()
        });
        self.subtypes.retain(|_, v| {
            v.retain(|p| **p != *path);
            !v.is_empty()
        });
        self.generation += 1;
        self.log_change(Arc::from(path));
    }

    /// Drops every file under `dir` (a deleted folder): one range of the sorted paths, not a scan
    /// of every file.
    pub fn remove_under(&mut self, dir: &Path) {
        for p in self.files_under(dir) {
            self.remove(&p);
        }
    }

    /// The files under `dir`, sorted.
    pub fn files_under(&self, dir: &Path) -> Vec<PathBuf> {
        self.shared
            .range::<Path, _>((std::ops::Bound::Included(dir), std::ops::Bound::Unbounded))
            .map(|(p, _)| p)
            .take_while(|p| p.starts_with(dir))
            .cloned()
            .collect()
    }

    pub fn decls(&self) -> impl Iterator<Item = (&PathBuf, &FileSyms, &Decl)> {
        self.files
            .iter()
            .flat_map(|(p, f)| f.decls.iter().map(move |d| (p, &**f, d)))
    }

    /// Declarations named exactly `name`, without scanning the whole project.
    pub fn named<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a FileSyms, &'a Decl)> + 'a {
        self.by_name
            .get_key_value(name)
            .into_iter()
            .flat_map(|(name, v)| v.iter().map(move |at| (name, at)))
            .filter_map(move |(name, (p, i))| {
                let (p, f) = self.files.get_key_value(&**p)?;
                // a name a failed change left behind may point at another declaration now
                let d = f.decls.get(*i as usize).filter(|d| d.name == *name)?;
                Some((p, &**f, d))
            })
    }

    /// Declarations whose container is `name` (members of a class).
    pub fn members<'a>(&'a self, container: &str) -> impl Iterator<Item = &'a Decl> + 'a {
        let key = self
            .by_name
            .get_key_value(container)
            .map(|(k, _)| k.as_str());
        self.named(container).flat_map(move |(_, f, _)| {
            f.decls
                .iter()
                .filter(move |d| d.container.as_deref().is_some_and(|c| Some(c) == key))
        })
    }

    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|f| f.decls.len()).sum()
    }

    /// Fuzzy "Go to class" (types only) or "Go to symbol" (everything).
    pub fn search(&self, query: &str, types_only: bool, limit: usize) -> Vec<Hit> {
        let items = self
            .decls()
            .filter(|(_, _, d)| !types_only || d.kind.is_type());
        rank_by(query, items, |(_, _, d)| &d.name, limit)
            .into_iter()
            .map(|(score, (p, f, d))| Hit {
                name: d.name.clone(),
                detail: describe(f, d),
                path: p.clone(),
                offset: d.offset,
                score,
            })
            .collect()
    }

    /// Where `name` is declared. Prefers the current file, then imported and same-package declarations.
    pub fn definitions(&self, name: &str, from: &Path) -> Vec<Hit> {
        let here = self.files.get(from);
        let mut hits: Vec<Hit> = self
            .named(name)
            .map(|(p, f, d)| {
                let fq = qualified(f, d);
                let score = if p == from {
                    3
                } else if here.is_some_and(|h| {
                    h.imports.iter().any(|i| {
                        **i == *fq || i.ends_with(".*") && fq.starts_with(i.trim_end_matches('*'))
                    })
                }) {
                    2
                } else if here.is_some_and(|h| h.package == f.package) {
                    1
                } else {
                    0
                };
                Hit {
                    name: d.name.clone(),
                    detail: describe(f, d),
                    path: p.clone(),
                    offset: d.offset,
                    score,
                }
            })
            .collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
        hits
    }

    /// Completion candidates for an identifier prefix: one per name, exact-case prefix first.
    pub fn complete(&self, prefix: &str, limit: usize) -> Vec<Hit> {
        let names = self.by_name.keys().map(String::as_str);
        let mut top = rank_by(prefix, names, |n| n, limit * 4);
        top.sort_by_key(|(score, n)| (!n.starts_with(prefix), std::cmp::Reverse(*score)));
        top.into_iter()
            .take(limit)
            .filter_map(|(score, n)| {
                let (p, f, d) = self.named(n).next()?;
                Some(Hit {
                    name: d.name.clone(),
                    detail: describe(f, d),
                    path: p.clone(),
                    offset: d.offset,
                    score,
                })
            })
            .collect()
    }

    /// Fully qualified name of a type or top-level declaration, for adding an import.
    pub fn import_for(&self, name: &str) -> Option<String> {
        self.named(name)
            .find(|(_, _, d)| d.kind.is_type() || d.container.is_none())
            .map(|(_, f, d)| qualified(f, d))
    }

    /// Classes that extend or implement `name`, directly or through other classes (breadth first).
    pub fn implementations(&self, name: &str) -> Vec<Hit> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::from([name.to_string()]);
        let fun_interface = self
            .named(name)
            .any(|(_, _, d)| d.kind == Kind::Interface && d.has_modifier("fun"));
        while let Some(n) = queue.pop_front() {
            for p in self.subtypes.get(&n).into_iter().flatten() {
                let Some((p, f)) = self.files.get_key_value(&**p) else {
                    continue;
                };
                if fun_interface && n == name {
                    for &(_, at) in f.sams.iter().filter(|(s, _)| **s == *n) {
                        if seen.insert((p.clone(), at)) {
                            out.push(sam_hit(p, f, &n, at));
                        }
                    }
                }
                for d in f
                    .decls
                    .iter()
                    .filter(|d| d.kind.is_type() && d.supers.contains(&n))
                {
                    if seen.insert((p.clone(), d.offset)) && out.len() < 500 {
                        out.push(hit(p, f, d));
                        queue.push_back(d.name.clone());
                    }
                }
            }
        }
        out
    }

    /// Members named `member` inside the type `container`, with their files.
    fn members_named<'a>(
        &'a self,
        container: &str,
        member: &'a str,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a FileSyms, &'a Decl)> + 'a {
        let key = self
            .by_name
            .get_key_value(container)
            .map(|(k, _)| k.as_str());
        self.named(container).flat_map(move |(p, f, _)| {
            f.decls
                .iter()
                .filter(move |d| {
                    d.name == member && d.container.as_deref().is_some_and(|c| Some(c) == key)
                })
                .map(move |d| (p, f, d))
        })
    }

    /// Kotlin Multiplatform: the `actual`s of an `expect` declaration, or the `expect` of an
    /// `actual` one. They share the name, the package and the enclosing class, in other files;
    /// each hit's detail starts with its source set (`androidMain · …`).
    pub fn counterparts(&self, path: &Path, f: &FileSyms, d: &Decl) -> Vec<Hit> {
        match kmp_role(f, d) {
            Some(role) => self.counterparts_as(path, f, d, role),
            None => Vec::new(),
        }
    }

    /// [`Index::counterparts`] of a declaration whose role (`expect` or `actual`) is known.
    pub fn counterparts_as(&self, path: &Path, f: &FileSyms, d: &Decl, role: &str) -> Vec<Hit> {
        let want = if role == "expect" { "actual" } else { "expect" };
        let mut hits: Vec<Hit> = self
            .named(&d.name)
            .filter(|(p, g, x)| {
                p.as_path() != path
                    && g.package == f.package
                    && x.container == d.container
                    && x.kind == d.kind
                    && kmp_role(g, x) == Some(want)
            })
            .map(|(p, g, x)| {
                let mut h = hit(p, g, x);
                h.detail = format!("{} · {}", source_set(p), h.detail);
                h
            })
            .collect();
        hits.sort_by(|a, b| a.detail.cmp(&b.detail));
        hits
    }

    /// The declaration an `override` member overrides: the nearest supertype of `container`
    /// that declares `member`.
    pub fn overridden(&self, container: &str, member: &str) -> Option<Hit> {
        let mut seen = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = self
            .named(container)
            .filter(|(_, _, d)| d.kind.is_type())
            .flat_map(|(_, _, d)| d.supers.clone())
            .collect();
        while let Some(s) = queue.pop_front() {
            if !seen.insert(s.clone()) {
                continue;
            }
            if let Some((p, f, d)) = self.members_named(&s, member).next() {
                return Some(hit(p, f, d));
            }
            queue.extend(
                self.named(&s)
                    .filter(|(_, _, d)| d.kind.is_type())
                    .flat_map(|(_, _, d)| d.supers.clone()),
            );
        }
        None
    }

    /// Each member of the type `container` (each file once), with its file.
    fn members_in<'a>(
        &'a self,
        container: &'a str,
    ) -> impl Iterator<Item = (&'a PathBuf, &'a FileSyms, &'a Decl)> + 'a {
        let mut files: Vec<(&PathBuf, &FileSyms)> =
            self.named(container).map(|(p, f, _)| (p, f)).collect();
        files.dedup_by(|a, b| a.0 == b.0);
        files.into_iter().flat_map(move |(p, f)| {
            f.decls
                .iter()
                .filter(move |d| d.container.as_deref() == Some(container))
                .map(move |d| (p, f, d))
        })
    }

    /// [`Index::overridden`] for every member name of `container` at once, by one walk up its
    /// supertypes (lenses of a class with thousands of members).
    pub fn overridden_in(&self, container: &str) -> HashMap<String, Hit> {
        let supers = |n: &str| -> Vec<String> {
            self.named(n)
                .filter(|(_, _, d)| d.kind.is_type())
                .flat_map(|(_, _, d)| d.supers.clone())
                .collect()
        };
        let (mut out, mut seen) = (HashMap::new(), HashSet::new());
        let mut queue = std::collections::VecDeque::from(supers(container));
        while let Some(s) = queue.pop_front() {
            if !seen.insert(s.clone()) {
                continue;
            }
            // the nearest supertype that declares a name wins, like `overridden`
            for (p, f, d) in self.members_in(&s) {
                out.entry(d.name.clone()).or_insert_with(|| hit(p, f, d));
            }
            queue.extend(supers(&s));
        }
        out
    }

    /// [`Index::member_implementations`] for every member name of `container` at once.
    pub fn member_implementations_in(&self, container: &str) -> HashMap<String, Vec<Hit>> {
        let mut out: HashMap<String, Vec<Hit>> = HashMap::new();
        for c in self.implementations(container) {
            let class = c.name.rsplit('.').next().unwrap_or(&c.name);
            for (p, f, d) in self.members_in(class) {
                if d.has_modifier("override") {
                    out.entry(d.name.clone()).or_default().push(hit(p, f, d));
                }
            }
        }
        out
    }

    /// `override` members named `member` in the classes that implement `container`.
    pub fn member_implementations(&self, container: &str, member: &str) -> Vec<Hit> {
        self.implementations(container)
            .iter()
            .flat_map(|c| {
                let class = c.name.rsplit('.').next().unwrap_or(&c.name).to_string();
                self.members_named(&class, member)
                    .filter(|(_, _, d)| d.has_modifier("override"))
                    .map(|(p, f, d)| hit(p, f, d))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn find_decl(&self, name: &str, kind: Kind) -> Option<&Decl> {
        self.named(name).map(|(_, _, d)| d).find(|d| d.kind == kind)
    }

    pub fn find_type(&self, name: &str) -> Option<&Decl> {
        self.named(name)
            .map(|(_, _, d)| d)
            .find(|d| d.kind.is_type())
    }
}

/// Points repeated strings (imports, annotations, modifiers, container names) at one copy per
/// worker: on a 12k-file project most imports repeat in hundreds of files.
fn share(f: &mut FileSyms, seen: &mut HashSet<Atom>) {
    let mut one = |a: &mut Atom| match seen.get(a) {
        Some(x) => *a = x.clone(),
        None => {
            seen.insert(a.clone());
        }
    };
    f.imports.iter_mut().for_each(&mut one);
    for d in &mut f.decls {
        d.annotations.iter_mut().for_each(&mut one);
        d.modifiers.iter_mut().for_each(&mut one);
        if let Some(c) = &mut d.container {
            one(c);
        }
    }
}

/// `expect` or `actual`: the declaration's own modifier, or for a member, its class's `expect`
/// (members of an `expect class` carry no modifier of their own).
/// The simple name of a type: `List<Api>?` → `List`, `com.x.Api` → `Api`.
pub fn type_name(ty: &str) -> String {
    let t = ty.trim().trim_end_matches('?');
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit('.').next().unwrap_or(t).trim().to_string()
}

pub fn kmp_role(f: &FileSyms, d: &Decl) -> Option<&'static str> {
    kmp_role_in(d, &expect_types(f))
}

/// The names of a file's `expect` types, for [`kmp_role_in`].
pub fn expect_types(f: &FileSyms) -> HashSet<&str> {
    f.decls
        .iter()
        .filter(|x| x.kind.is_type() && x.has_modifier("expect"))
        .map(|x| x.name.as_str())
        .collect()
}

/// [`kmp_role`] with the file's `expect` types found once (lenses ask for every declaration).
pub fn kmp_role_in(d: &Decl, expect_types: &HashSet<&str>) -> Option<&'static str> {
    if d.has_modifier("expect") {
        return Some("expect");
    }
    if d.has_modifier("actual") {
        return Some("actual");
    }
    expect_types
        .contains(d.container.as_deref()?)
        .then_some("expect")
}

/// The source set of a file: the folder after `src/` (`commonMain`, `androidMain`, `test`).
pub fn source_set(p: &Path) -> String {
    let mut it = p.components().map(|c| c.as_os_str().to_string_lossy());
    it.by_ref().find(|c| c == "src");
    it.next().map(|c| c.into_owned()).unwrap_or_default()
}

fn names_of(f: &FileSyms) -> Vec<String> {
    let mut v: Vec<String> = f.decls.iter().map(|d| d.name.clone()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn supers_of(f: &FileSyms) -> Vec<String> {
    let mut v: Vec<String> = f
        .decls
        .iter()
        .flat_map(|d| d.supers.iter().cloned())
        .chain(f.sams.iter().map(|(n, _)| n.to_string()))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A `Name { … }` lambda, named after the declaration it is in.
fn sam_hit(p: &Path, f: &FileSyms, name: &str, at: usize) -> Hit {
    let inside = f.decls.iter().rfind(|d| d.offset < at);
    Hit {
        name: match inside {
            Some(d) => format!("{name} {{ }} in {}", d.name),
            None => format!("{name} {{ }}"),
        },
        detail: inside.map_or(String::new(), |d| describe(f, d)),
        path: p.to_path_buf(),
        offset: at,
        score: 0,
    }
}

fn hit(p: &Path, f: &FileSyms, d: &Decl) -> Hit {
    Hit {
        name: match &d.container {
            Some(c) => format!("{c}.{}", d.name),
            None => d.name.clone(),
        },
        detail: describe(f, d),
        path: p.to_path_buf(),
        offset: d.offset,
        score: 0,
    }
}

pub fn qualified(f: &FileSyms, d: &Decl) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !f.package.is_empty() {
        parts.push(&f.package);
    }
    if let Some(c) = &d.container {
        parts.push(c);
    }
    parts.push(&d.name);
    parts.join(".")
}

/// Short signature shown next to a symbol: `fun UserCard(user: User, …)  com.example.ui`.
pub fn describe(f: &FileSyms, d: &Decl) -> String {
    let mut s = String::new();
    if d.is_composable() {
        s.push_str("@Composable ");
    }
    s.push_str(if d.kind == Kind::Property && d.mutable {
        "var"
    } else {
        d.kind.label()
    });
    if d.kind == Kind::Fun {
        let ps: Vec<String> = d
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty))
            .collect();
        s.push_str(&format!("({})", ps.join(", ")));
    }
    if !d.ty.is_empty() {
        s.push_str(&format!(": {}", d.ty));
    }
    let place = match &d.container {
        Some(c) if f.package.is_empty() => c.to_string(),
        Some(c) => format!("{}.{}", f.package, c),
        None => f.package.clone(),
    };
    if !place.is_empty() {
        s.push_str(&format!("  {place}"));
    }
    s
}

/// Fuzzy-ranks `items` by `key` and keeps the best `limit`, without allocating per item.
/// Exact matches and prefix matches score higher; shorter names win ties. An empty query keeps the input order.
pub fn rank_by<T>(
    query: &str,
    items: impl Iterator<Item = T>,
    key: impl Fn(&T) -> &str,
    limit: usize,
) -> Vec<(u32, T)> {
    if query.is_empty() {
        return items.take(limit).map(|t| (0, t)).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, T)> = items
        .filter_map(|t| {
            let k = key(&t);
            let s = pattern.score(Utf32Str::new(k, &mut buf), &mut matcher)?;
            let prefix = k
                .get(..query.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(query));
            let bonus = if prefix && k.len() == query.len() {
                10_000
            } else if prefix {
                1_000
            } else {
                0
            };
            Some(((s + bonus).saturating_sub(k.len() as u32), t))
        })
        .collect();
    if scored.len() > limit {
        scored.select_nth_unstable_by_key(limit, |(s, _)| std::cmp::Reverse(*s));
        scored.truncate(limit);
    }
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn a_change_queued_while_changes_apply_does_not_wait_for_them() {
        let shared = Shared::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let inner = shared.clone();
        std::thread::spawn(move || {
            let again = inner.clone();
            // a change that queues another while it runs (the editor queueing during a flush)
            inner.change(move |_| again.change(|i| i.generation += 100));
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "queueing waited for the changes being applied"
        );
        shared.settle();
        assert!(
            shared.read().generation >= 100,
            "the second change went in too"
        );
    }

    #[test]
    fn a_change_that_panics_leaves_the_ones_queued_after_it() {
        let shared = Shared::default();
        // a reader holds the index: the changes queue up
        let reading = shared.read();
        shared.change(|_| panic!("a bug in a change"));
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        drop(reading);
        shared.settle();
        assert_eq!(shared.read().named("A").count(), 1);
    }

    #[test]
    fn a_file_change_that_fails_halfway_leaves_nothing_of_that_file() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        let reading = shared.read();
        // a change of B that dies after putting in one of B's names
        let b = PathBuf::from("/p/B.kt");
        shared.queue(
            Of::File(b.clone()),
            Box::new(move |i: &mut Index| {
                i.by_name
                    .entry("B".into())
                    .or_default()
                    .push((Arc::from(b.as_path()), 0));
                panic!("cut short");
            }),
        );
        drop(reading);
        shared.settle();
        assert_eq!(shared.read().named("B").count(), 0, "lookups skip it");
        assert!(shared.rescan_due(), "the worker rebuilds the names");
        shared.rescan();
        shared.settle();
        let i = shared.read();
        assert_eq!(i.named("A").count(), 1, "other files stay");
        assert!(
            !i.by_name.contains_key("B"),
            "a name of a file not in the index"
        );
    }

    #[test]
    fn a_failed_change_leaves_the_rebuild_of_the_names_to_the_worker() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        let reading = shared.read();
        shared.change(|i| {
            i.by_name
                .entry("Z".into())
                .or_default()
                .push((Arc::from(Path::new("/p/Z.kt")), 0));
            panic!("cut short");
        });
        drop(reading);
        // the flush (the editor's, maybe) only notes it
        shared.settle();
        assert!(
            shared.read().by_name.contains_key("Z"),
            "not rebuilt in the flush"
        );
        assert!(shared.rescan_due());
        shared.rescan();
        shared.settle();
        assert!(
            !shared.read().by_name.contains_key("Z"),
            "rebuilt by the worker"
        );
        assert!(!shared.rescan_due());
    }

    #[test]
    fn a_rebuild_of_the_names_that_fails_keeps_the_files_and_their_names() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        shared.update(Path::new("/p/B.kt"), "package a\nclass B\n");
        test_hooks::PANIC_ON_INSERT.set(true);
        // a change of no one file fails, and so does the rebuild after it
        shared.change(|_| panic!("cut short"));
        shared.settle();
        shared.rescan();
        shared.settle();
        test_hooks::PANIC_ON_INSERT.set(false);
        let i = shared.read();
        assert_eq!(i.named("A").count(), 1);
        assert_eq!(i.named("B").count(), 1);
    }

    #[test]
    fn a_name_left_behind_never_points_at_another_declaration() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        // a dangling "Z" at A.kt's first declaration, which is A
        shared.change(|i| {
            i.by_name
                .entry("Z".into())
                .or_default()
                .push((Arc::from(Path::new("/p/A.kt")), 0));
        });
        shared.settle();
        assert_eq!(shared.read().named("Z").count(), 0);
    }

    #[test]
    fn a_file_waiting_to_be_read_again_is_not_read_into_the_next_index() {
        let shared = Shared::default();
        let b = on_disk("rescan-set", "package a\nclass B\n");
        fail_change_of(&shared, &b);
        // another project opens before the worker reads B again
        shared.set(Index::default());
        shared.settle();
        shared.rescan();
        shared.settle();
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
        assert_eq!(shared.read().named("B").count(), 0);
    }

    #[test]
    fn a_file_read_again_after_another_index_was_set_stays_out_of_it() {
        let shared = Shared::default();
        let b = on_disk("rescan-set-late", "package a\nclass B\n");
        fail_change_of(&shared, &b);
        // the new index goes in while the worker reads B again from disk
        let other = shared.clone();
        test_hooks::WHILE_READING_AGAIN.set(Some(Box::new(move || {
            other.set(Index::default());
            other.settle();
        })));
        shared.rescan();
        shared.settle();
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
        assert_eq!(shared.read().named("B").count(), 0);
    }

    #[test]
    fn queueing_never_waits_for_a_flush_that_takes_the_editors_priority() {
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let shared = Shared::default();
        let reading = shared.read();
        shared.change(|_| {});
        drop(reading);
        let (entered_tx, entered) = channel();
        let (go, go_rx) = channel();
        let inner = shared.clone();
        let flusher = std::thread::spawn(move || {
            test_hooks::BOOST_GATE.set(Some((entered_tx, go_rx)));
            inner.flush()
        });
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        // the flusher is taking the editor's priority: queueing goes on meanwhile
        let (done_tx, done) = channel();
        let q = shared.clone();
        std::thread::spawn(move || {
            q.change(|i| i.generation += 50);
            let _ = done_tx.send(());
        });
        let queued = done.recv_timeout(Duration::from_secs(10));
        let _ = go.send(());
        flusher.join().unwrap();
        assert!(queued.is_ok(), "queueing waited for the flush");
        shared.settle();
        assert!(shared.read().generation >= 50);
    }

    #[test]
    fn a_thread_that_cannot_take_the_editors_priority_wakes_the_editor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let shared = Shared::default();
        let woke = Arc::new(AtomicUsize::new(0));
        let w = woke.clone();
        shared.on_wake(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        test_hooks::NO_BOOST.set(true);
        shared.change(|i| i.generation += 7);
        test_hooks::NO_BOOST.set(false);
        assert_eq!(woke.load(Ordering::SeqCst), 1);
        assert_eq!(shared.read().generation, 0, "left for the editor");
        assert!(shared.flush());
        assert_eq!(shared.read().generation, 7);
    }

    #[test]
    fn a_thread_niced_past_the_editor_is_below_it_and_a_niced_process_is_not() {
        assert!(below_the_editor(10, 0), "the worker");
        assert!(
            !below_the_editor(5, 5),
            "the editor of a process started with nice 5"
        );
        assert!(!below_the_editor(0, 0));
    }

    /// A file on disk with `text`, in its own folder (removed by the caller).
    fn on_disk(name: &str, text: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-index-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("B.kt");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn fail_change_of(shared: &Shared, p: &Path) {
        shared.fail_change_of(p);
    }

    #[test]
    fn a_file_a_failed_change_took_out_comes_back_from_disk_on_rescan() {
        let shared = Shared::default();
        let b = on_disk("rescan", "package a\nclass B\n");
        fail_change_of(&shared, &b);
        assert_eq!(shared.read().named("B").count(), 0, "taken out");
        assert!(shared.rescan_due());
        shared.rescan();
        shared.settle();
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
        assert!(!shared.rescan_due());
        assert_eq!(shared.read().named("B").count(), 1, "read again from disk");
    }

    #[test]
    fn a_rescan_never_replaces_a_newer_change_of_the_file() {
        let shared = Shared::default();
        let b = on_disk("rescan-newer", "package a\nclass B\n");
        fail_change_of(&shared, &b);
        // the tab's newer text went in before the worker read the disk
        shared.update(&b, "package a\nclass Typed\n");
        shared.rescan();
        shared.settle();
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
        let i = shared.read();
        assert_eq!(i.named("Typed").count(), 1);
        assert_eq!(i.named("B").count(), 0, "the disk's older text stayed out");
    }

    #[test]
    fn a_file_put_back_after_its_change_failed_halfway_has_each_name_once() {
        let shared = Shared::default();
        let b = PathBuf::from("/p/B.kt");
        let reading = shared.read();
        let q = b.clone();
        shared.queue(
            Of::File(b.clone()),
            Box::new(move |i: &mut Index| {
                i.by_name
                    .entry("B".into())
                    .or_default()
                    .push((Arc::from(q.as_path()), 0));
                panic!("cut short");
            }),
        );
        drop(reading);
        shared.settle();
        // the tab's text goes in before the worker rebuilds the names
        shared.update(&b, "package a\nclass B\n");
        assert_eq!(shared.read().named("B").count(), 1);
    }

    #[test]
    fn a_rebuild_of_the_names_lost_to_a_newer_change_waits_before_the_next() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        shared.change(|_| panic!("cut short"));
        shared.settle();
        // typing changes the index while the worker rebuilds
        let typing = shared.clone();
        test_hooks::WHILE_REBUILDING.set(Some(Box::new(move || {
            typing.update(Path::new("/p/B.kt"), "package a\nclass B\n")
        })));
        shared.rescan();
        shared.settle();
        assert!(!shared.rescan_due(), "not again on the next frame");
        assert!(shared.repair_wait().is_some());
        shared.skip_repair_wait();
        assert!(shared.rescan_due(), "again after the pause");
        shared.rescan();
        shared.settle();
        assert!(!shared.rescan_due(), "done");
        assert_eq!(shared.read().named("B").count(), 1);
    }

    #[test]
    fn a_rebuild_of_the_names_that_fails_is_tried_once_more_after_a_pause() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        test_hooks::PANIC_ON_INSERT.set(true);
        shared.change(|_| panic!("cut short"));
        shared.settle();
        shared.rescan();
        assert!(!shared.rescan_due(), "not again at once");
        shared.skip_repair_wait();
        assert!(shared.rescan_due(), "once more after the pause");
        shared.rescan();
        shared.skip_repair_wait();
        test_hooks::PANIC_ON_INSERT.set(false);
        assert!(!shared.rescan_due(), "then left");
        assert_eq!(shared.read().named("A").count(), 1);
    }

    #[test]
    fn another_index_set_leaves_no_rebuild_of_the_names_due() {
        let shared = Shared::default();
        shared.change(|_| panic!("cut short"));
        shared.settle();
        assert!(shared.rescan_due());
        shared.set(Index::default());
        shared.settle();
        assert!(!shared.rescan_due());
    }

    #[test]
    fn a_failed_change_of_no_one_file_leaves_no_name_without_its_file() {
        let shared = Shared::default();
        shared.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        let since = shared.read().generation;
        let reading = shared.read();
        shared.change(|i| {
            i.by_name
                .entry("Z".into())
                .or_default()
                .push((Arc::from(Path::new("/p/Z.kt")), 0));
            panic!("cut short");
        });
        drop(reading);
        shared.settle();
        shared.rescan();
        shared.settle();
        let i = shared.read();
        assert!(
            !i.by_name.contains_key("Z"),
            "a name of a file not in the index"
        );
        assert_eq!(i.named("A").count(), 1, "the files stay");
        assert!(
            i.changed_since(since).is_none(),
            "what changed cannot be told: cached results are worked out again"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_thread_below_the_editor_applies_changes_at_the_editors_priority() {
        let shared = Shared::default();
        let reading = shared.read();
        let (tx, rx) = std::sync::mpsc::channel();
        let class = || {
            let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
            let mut rel = 0;
            unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut rel);
            }
            class as u32
        };
        shared.change(move |_| {
            let _ = tx.send(class());
        });
        drop(reading);
        let inner = shared.clone();
        let after = std::thread::spawn(move || {
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
            }
            assert!(inner.flush());
            class()
        })
        .join()
        .unwrap();
        let utility = libc::qos_class_t::QOS_CLASS_UTILITY as u32;
        assert_eq!(
            rx.try_recv(),
            Ok(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE as u32),
            "under the write lock"
        );
        assert_eq!(after, utility, "back below the editor after it");
    }

    #[test]
    fn update_reads_symbols_before_queueing_never_under_the_lock() {
        let shared = Shared::default();
        let scans = || crate::kotlin::SCANS.with(|n| n.get());
        let p = Path::new("/p/A.kt");
        // a reader holds the index: a change can only queue
        let reading = shared.read();
        let before = scans();
        shared.update(p, "package a\nclass A\n");
        assert_eq!(
            scans(),
            before + 1,
            "read while the index could not be changed"
        );
        drop(reading);
        shared.settle();
        assert_eq!(scans(), before + 1, "putting it in reads nothing again");
        shared.update(Path::new("/p/notes.txt"), "class B\n");
        shared.settle();
        assert_eq!(shared.read().named("A").count(), 1);
        assert!(
            !shared.read().files.contains_key(Path::new("/p/notes.txt")),
            "not code"
        );
    }

    #[test]
    fn a_replaced_index_tells_nothing_about_changes_since_the_old_one() {
        let shared = Shared::default();
        let mut a = Index::default();
        a.update(Path::new("/p/A.kt"), "package a\nclass A\n");
        shared.set(a);
        shared.settle();
        let old = shared.read().generation;
        assert_eq!(shared.read().changed_since(old).map(|c| c.len()), Some(0));
        let mut b = Index::default();
        b.update(Path::new("/q/B.kt"), "package b\nclass B\n");
        shared.set(b);
        shared.settle();
        let now = shared.read().generation;
        assert!(now > old, "generations keep counting across a swap");
        assert!(shared.read().changed_since(old).is_none());
        assert_eq!(shared.read().changed_since(now).map(|c| c.len()), Some(0));
        shared.change(|i| i.update(Path::new("/q/B.kt"), "package b\nclass B2\n"));
        shared.settle();
        let changed = shared.read().changed_since(now).unwrap();
        assert!(!changed.is_empty() && changed.iter().all(|p| **p == *Path::new("/q/B.kt")));
    }

    #[test]
    fn pairs_expect_and_actual_across_source_sets() {
        let mut idx = Index::default();
        let common = PathBuf::from("/p/shared/src/commonMain/kotlin/a/Platform.kt");
        let android = PathBuf::from("/p/shared/src/androidMain/kotlin/a/Platform.android.kt");
        let ios = PathBuf::from("/p/shared/src/iosMain/kotlin/a/Platform.ios.kt");
        let other = PathBuf::from("/p/shared/src/androidMain/kotlin/b/Platform.kt");
        idx.update(&common, "package a\nexpect fun platformName(): String\nexpect class Clock {\n    fun now(): Long\n}\n");
        idx.update(&android, "package a\nactual fun platformName(): String = \"Android\"\nactual class Clock {\n    actual fun now(): Long = 0\n}\n");
        idx.update(
            &ios,
            "package a\nactual fun platformName(): String = \"iOS\"\n",
        );
        // same name, other package: not an actual of a.platformName
        idx.update(
            &other,
            "package b\nactual fun platformName(): String = \"x\"\n",
        );
        let decl = |p: &Path, name: &str| {
            let f = &idx.files[p];
            (f, f.decls.iter().find(|d| &*d.name == name).unwrap())
        };
        let (f, d) = decl(&common, "platformName");
        let sets: Vec<String> = idx
            .counterparts(&common, f, d)
            .iter()
            .map(|h| h.detail.split(" · ").next().unwrap().to_string())
            .collect();
        assert_eq!(sets, ["androidMain", "iosMain"]);
        let (f, d) = decl(&ios, "platformName");
        let back = idx.counterparts(&ios, f, d);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].path, common);
        // a member of an `expect class` has no modifier of its own
        let (f, d) = decl(&common, "now");
        assert_eq!(kmp_role(f, d), Some("expect"));
        assert_eq!(idx.counterparts(&common, f, d)[0].path, android);
        // a plain declaration has none
        let (f, d) = decl(&common, "Clock");
        assert_eq!(idx.counterparts(&common, f, d).len(), 1);
    }

    #[test]
    fn a_variable_named_actual_is_not_a_modifier() {
        // `actual` read by a property, then another declaration: still not a modifier of `t`
        let f = crate::kotlin::scan(
            "class T {\n    val expected = 1\n    val actual = 2\n    val copy = actual\n    fun t() { assertEquals(expected, actual) }\n}\n",
        );
        let names: Vec<&str> = f.decls.iter().map(|d| &*d.name).collect();
        assert!(
            names.contains(&"actual") && names.contains(&"expected"),
            "{names:?}"
        );
        assert!(f.decls.iter().all(|d| d.modifiers.is_empty()));
        // an enum entry named `actual` stays an entry
        let e = crate::kotlin::scan(
            "enum class Match {\n    expected,\n    actual\n}\nfun after() {}\n",
        );
        let names: Vec<&str> = e.decls.iter().map(|d| &*d.name).collect();
        assert!(names.contains(&"actual"), "{names:?}");
        assert!(
            e.decls
                .iter()
                .filter(|d| matches!(&*d.name, "actual" | "after"))
                .all(|d| d.modifiers.is_empty()),
            "{names:?}"
        );
    }

    #[test]
    fn files_scanned_together_share_repeated_strings() {
        let dir = std::env::temp_dir().join(format!("droid-lite-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "package a\nimport androidx.compose.runtime.Composable\nclass A {\n    private fun f() {}\n}\n";
        // enough files that the first two go to the same worker (64 files, at most 32 workers)
        let files: Vec<PathBuf> = (0..64)
            .map(|i| {
                let p = dir.join(format!("F{i}.kt"));
                std::fs::write(&p, src.replace("class A", &format!("class A{i}"))).unwrap();
                p
            })
            .collect();
        let idx = Index::build(&files);
        let (a, b) = (&files[0], &files[1]);
        let (fa, fb) = (&idx.files[a], &idx.files[b]);
        assert!(
            std::sync::Arc::ptr_eq(&fa.imports[0], &fb.imports[0]),
            "one copy of the import"
        );
        let m =
            |f: &FileSyms| f.decls.iter().find(|d| &*d.name == "f").unwrap().modifiers[0].clone();
        assert!(
            std::sync::Arc::ptr_eq(&m(fa), &m(fb)),
            "one copy of `private`"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn index() -> Index {
        let mut i = Index::default();
        i.update(Path::new("/p/ui/Card.kt"), "package com.ui\nimport com.data.User\n@Composable\nfun UserCard(user: User) {}\nclass CardState\n");
        i.update(
            Path::new("/p/data/User.kt"),
            "package com.data\ndata class User(val name: String)\nclass UserRepository\n",
        );
        i.update(
            Path::new("/p/other/User.kt"),
            "package com.other\nclass User\n",
        );
        i
    }

    #[test]
    fn fuzzy_class_search_ranks_prefix_first() {
        let hits = index().search("UsRe", true, 10);
        assert_eq!(hits[0].name, "UserRepository");
        let hits = index().search("user", true, 10);
        assert_eq!(hits[0].name, "User");
        assert!(
            hits.iter().all(|h| h.name != "UserCard"),
            "funs are not types"
        );
    }

    #[test]
    fn definition_prefers_imported_declaration() {
        let defs = index().definitions("User", Path::new("/p/ui/Card.kt"));
        assert_eq!(defs[0].path, Path::new("/p/data/User.kt"));
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn hierarchy_implementations_and_overrides() {
        let mut i = Index::default();
        i.update(
            Path::new("/p/Api.kt"),
            "interface Api { fun load(): Int }\nabstract class Base : Api\n",
        );
        i.update(Path::new("/p/Impl.kt"), "class Impl : Base() { override fun load() = 1 }\nclass Other : Api {\n override fun load() = 2\n}\n");
        let names = |v: Vec<Hit>| v.into_iter().map(|h| h.name).collect::<Vec<_>>();
        let mut impls = names(i.implementations("Api"));
        impls.sort();
        assert_eq!(
            impls,
            vec!["Base", "Impl", "Other"],
            "direct and through Base"
        );
        assert_eq!(i.overridden("Impl", "load").unwrap().name, "Api.load");
        assert!(i.overridden("Api", "load").is_none());
        let mut m = names(i.member_implementations("Api", "load"));
        m.sort();
        assert_eq!(m, vec!["Impl.load", "Other.load"]);
        // the same, for every member at once
        assert_eq!(i.overridden_in("Impl")["load"].name, "Api.load");
        assert!(i.overridden_in("Api").is_empty());
        let mut all = names(i.member_implementations_in("Api").remove("load").unwrap());
        all.sort();
        assert_eq!(all, m);
        i.remove(Path::new("/p/Impl.kt"));
        assert_eq!(names(i.implementations("Api")), vec!["Base"]);
    }

    #[test]
    fn fun_interface_is_implemented_by_its_lambdas() {
        let mut i = Index::default();
        i.update(
            Path::new("/p/V.kt"),
            "internal fun interface ScreenValidator {\n    fun validate(s: Int): Int\n}\n\nfun validator(): ScreenValidator = ScreenValidator { s ->\n    s\n}\n",
        );
        i.update(
            Path::new("/p/Use.kt"),
            "fun run() {\n    load(ScreenValidator { it })\n    if (x) { }\n    ScreenValidator { }\n}\ninterface Plain\nval p = Plain { }\n",
        );
        let d = i.named("ScreenValidator").next().unwrap().2;
        assert_eq!(d.kind, Kind::Interface, "`fun interface` is a declaration");
        assert!(d.has_modifier("fun"));
        let mut impls: Vec<String> = i
            .implementations("ScreenValidator")
            .into_iter()
            .map(|h| h.name)
            .collect();
        impls.sort();
        assert_eq!(
            impls,
            [
                "ScreenValidator { } in run",
                "ScreenValidator { } in validator"
            ]
        );
        assert!(
            i.implementations("Plain").is_empty(),
            "only a `fun interface` has lambda implementations"
        );
    }

    #[test]
    fn name_map_follows_updates_and_removals() {
        let mut i = index();
        assert_eq!(i.named("User").count(), 2);
        i.update(
            Path::new("/p/other/User.kt"),
            "package com.other\nclass Person\n",
        );
        assert_eq!(i.named("User").count(), 1);
        assert_eq!(i.named("Person").count(), 1);
        i.remove_under(Path::new("/p/data"));
        assert_eq!(i.named("User").count(), 0);
        assert!(i.find_type("UserRepository").is_none());
        assert_eq!(i.members("Person").count(), 0);
    }

    #[test]
    fn completion_and_imports() {
        let i = index();
        let c = i.complete("UserC", 5);
        assert_eq!(c[0].name, "UserCard");
        assert!(c[0].detail.starts_with("@Composable fun(user: User)"));
        assert_eq!(
            i.import_for("UserRepository").as_deref(),
            Some("com.data.UserRepository")
        );
    }
}
