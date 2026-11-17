//! The one background worker for code insight: when a file is opened (or edited), what each of
//! its names means, where each of its declarations is used and its Hilt links are worked out
//! there, in short chunks under the index's read lock so the editor can keep updating the index.
//! One thread, started once; a newer request of the same kind cancels the older one; finished
//! results are kept per file and reused while neither the file nor anything it depends on
//! changed. A request someone waits for (Find in Files, a hover's usages) runs between the
//! chunks of a long file pass, on the same thread. A task that panics is logged and dropped;
//! the thread goes on.

use super::FoundLine;
use crate::index::{Hit, Shared};
use crate::resolve::{Cache, DeclRef, HiltGraph, Res, Resolver, Texts};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// The longest the worker held the index's read lock, in microseconds: an edit waits at most
/// that long to update the index.
pub(super) static LONGEST_LOCK: AtomicU64 = AtomicU64::new(0);

/// How long one chunk holds the index's read lock.
const CHUNK: Duration = Duration::from_millis(8);

/// How much of the file one step of the name pass reads.
const STEP: usize = 512;

/// How many files are read from disk at a time, before the lock is taken.
const AHEAD: usize = 16;

/// The kind of the open file's pass: it gives way to every other kind, which someone waits for.
const FILE: &str = "file";

/// A file's names and their uses.
pub(super) type Usages = HashMap<(Option<String>, String), Vec<Hit>>;

/// A file's code insight, for one version of its text.
#[derive(Clone, Default)]
pub(super) struct FileXref {
    pub path: PathBuf,
    pub rev: u64,
    /// What each name means: start, end and meaning, in file order.
    pub refs: Arc<Vec<(usize, usize, Res)>>,
    /// Uses of each declaration of the file, by container and name.
    pub usages: Arc<Usages>,
    pub hilt: Vec<HiltLink>,
}

impl FileXref {
    /// The meaning of the name that covers byte `at`.
    pub fn at(&self, at: usize) -> Option<&Res> {
        let i = self.refs.partition_point(|r| r.1 < at);
        self.refs
            .get(i)
            .filter(|r| r.0 <= at && at <= r.1)
            .map(|r| &r.2)
    }
}

/// One Hilt link above a declaration.
#[derive(Clone, Debug)]
pub(super) struct HiltLink {
    /// Offset of the declaration (or parameter) the link belongs to.
    pub at: usize,
    /// "Injected in": the binding's injection sites; else the providers of a site.
    pub injected: bool,
    pub title: String,
    pub hits: Vec<Hit>,
}

type Task = Box<dyn FnOnce(&State, &dyn Fn() -> bool) + Send>;
type Queued = (&'static str, Task);

/// The index as a pass starts: its generation, the code files under the project and a digest
/// of the open file's own entry.
struct Snapshot {
    generation: u64,
    files: Arc<Vec<PathBuf>>,
    own: u64,
}

/// A file's size and time on disk.
type Stamp = (u64, std::time::SystemTime);

fn stamp(p: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// The words a text spells, as 4096 bits (two per word): "maybe" or "no".
type Spelling = [u64; 64];

fn word_bits(word: &str) -> [usize; 2] {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    word.hash(&mut h);
    let h = h.finish();
    [(h % 4096) as usize, ((h >> 32) % 4096) as usize]
}

fn spelling(text: &str) -> Box<Spelling> {
    let mut bits = Box::new([0u64; 64]);
    for w in words_of(text) {
        for b in word_bits(w) {
            bits[b / 64] |= 1 << (b % 64);
        }
    }
    bits
}

/// Whether `bits` may spell `name` (a name that is not one plain word always may).
fn may_spell(bits: &Spelling, name: &str) -> bool {
    words_of(name).into_iter().ne([name])
        || word_bits(name)
            .iter()
            .all(|&b| bits[b / 64] & (1 << (b % 64)) != 0)
}

/// What the worker keeps between tasks.
pub(super) struct State {
    index: Shared,
    graph: RefCell<HiltGraph>,
    /// Finished file results with what they depend on.
    done: RefCell<HashMap<PathBuf, Kept>>,
    /// Counts uses of kept results, so the least recently used one goes first.
    uses: Cell<u64>,
    /// The names each project file on disk spells, with its size and time then: an unchanged
    /// file that spells none of the names looked for is not read again.
    spells: RefCell<HashMap<PathBuf, (Stamp, Box<Spelling>)>>,
    ctx: Option<egui::Context>,
    /// How long one chunk may hold the index's read lock.
    budget: Duration,
    /// How many times the lock was taken.
    holds: Cell<u64>,
    /// Where tasks arrive, and the ones that arrived and wait.
    inbox: Option<Receiver<Queued>>,
    waiting: RefCell<VecDeque<Queued>>,
    /// A waiting task runs between chunks now (no task inside another inside another).
    serving: Cell<bool>,
}

/// A file's names and usages (Hilt links are worked out again each time: the graph moves).
struct Kept {
    /// Hash of the text and the declarations it was worked out for (a reopened tab starts its
    /// version count again; declarations read from an older text sit at other offsets).
    hash: u64,
    /// The index generation it is known good at.
    generation: u64,
    /// What the index held for the file itself then ([`own_digest`]).
    own: u64,
    /// When it was last used ([`State::uses`]).
    used: u64,
    /// Files it read: where its names are declared, their types, their supertypes.
    deps: HashSet<PathBuf>,
    /// The file's declaration names: a changed file that spells one may use it.
    names: Vec<String>,
    /// Each name the file spells, found or not, with a digest of its declarations in the
    /// project: a changed file that adds one may change what the name means here.
    words: HashMap<String, u64>,
    refs: Arc<Vec<(usize, usize, Res)>>,
    usages: Arc<Usages>,
}

impl State {
    fn new(index: Shared, ctx: Option<egui::Context>) -> State {
        State {
            index,
            graph: RefCell::default(),
            done: RefCell::default(),
            uses: Cell::new(0),
            spells: RefCell::default(),
            ctx,
            budget: CHUNK,
            holds: Cell::new(0),
            inbox: None,
            waiting: RefCell::default(),
            serving: Cell::new(false),
        }
    }

    /// Runs `f` with a resolver under a short read lock. The disk is not read meanwhile: a file
    /// `f` wants that was not read ahead is noted ([`Texts::missed`]); what `f` worked out from
    /// then on is to be thrown away and worked out again after [`State::read_missed`].
    fn with<R>(&self, texts: &Texts, cache: &Cache, f: impl FnOnce(&Resolver) -> R) -> R {
        // the editor's queued changes go in first, under a short write lock
        self.index.flush();
        let t = Instant::now();
        let index = self.index.read();
        self.holds.set(self.holds.get() + 1);
        texts.set_locked(true);
        let r = Resolver::with(&index, texts, cache);
        let out = f(&r);
        texts.set_locked(false);
        drop(index);
        LONGEST_LOCK.fetch_max(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        out
    }

    /// Reads the files missed under the lock, without it; the types worked out with a file
    /// missing are dropped.
    fn read_missed(&self, texts: &Texts, cache: &Cache) -> bool {
        let missed = texts.read_missed();
        if missed {
            cache.forget_types();
        }
        missed
    }

    /// [`State::with`] again until no file was missed.
    fn resolve<R>(&self, texts: &Texts, cache: &Cache, f: impl Fn(&Resolver) -> R) -> R {
        loop {
            let out = self.with(texts, cache, &f);
            if !self.read_missed(texts, cache) {
                return out;
            }
        }
    }

    /// The project's code files: what the index holds under `root`, sorted (worked out here,
    /// not on the editor's thread).
    pub(super) fn code_files(&self, root: &Path) -> Arc<Vec<PathBuf>> {
        self.snapshot(root, root).files
    }

    /// What a pass over `path` starts from, read under one lock: a file added later is newer
    /// than the generation, so a kept result sees it changed.
    fn snapshot(&self, root: &Path, path: &Path) -> Snapshot {
        self.index.flush();
        let index = self.index.read();
        Snapshot {
            generation: index.generation,
            files: Arc::new(index.files_under(root)),
            own: own_digest(&index, path),
        }
    }

    /// Drops what the worker kept of the old project (another one opens).
    fn forget(&self) {
        self.done.borrow_mut().clear();
        self.spells.borrow_mut().clear();
        self.graph.replace(HiltGraph::default());
    }

    fn touch(&self) -> u64 {
        self.uses.set(self.uses.get() + 1);
        self.uses.get()
    }

    /// The next task: one someone waits for first, else the oldest; only the newest of each
    /// kind is kept. `block` waits for one; else `None` when none of those waits.
    fn next(&self, block: bool) -> Option<Queued> {
        let inbox = self.inbox.as_ref()?;
        let mut waiting = self.waiting.borrow_mut();
        let add = |w: &mut VecDeque<Queued>, t: Queued| {
            w.retain(|(k, _)| *k != t.0);
            w.push_back(t);
        };
        while let Ok(t) = inbox.try_recv() {
            add(&mut waiting, t);
        }
        if waiting.is_empty() && block {
            add(&mut waiting, inbox.recv().ok()?);
        }
        let at = match waiting.iter().position(|(k, _)| *k != FILE) {
            Some(i) => i,
            None if block => 0,
            None => return None,
        };
        waiting.remove(at)
    }

    /// Runs the tasks someone waits for that arrived meanwhile (between two chunks of a long
    /// pass).
    fn serve_waiting(&self) {
        if self.serving.replace(true) {
            return;
        }
        while let Some((kind, task)) = self.next(false) {
            run(self, kind, task);
        }
        self.serving.set(false);
    }
}

/// Runs `step` under the index's read lock until it says it is done, as many steps as fit in
/// the time budget per hold (at least one), letting the lock go between holds; `None` when
/// cancelled. A step that missed a file ([`Texts::missed`]) keeps nothing and says it is not
/// done: it runs again once the file is read.
fn in_chunks(
    s: &State,
    texts: &Texts,
    cache: &Cache,
    stale: &dyn Fn() -> bool,
    mut step: impl FnMut(&Resolver) -> bool,
) -> Option<()> {
    let mut done = false;
    while !done {
        if stale() {
            return None;
        }
        let t = Instant::now();
        s.with(texts, cache, |r| {
            while !done {
                done = step(r);
                if t.elapsed() >= s.budget || texts.missed() {
                    break;
                }
            }
        });
        s.read_missed(texts, cache);
        s.serve_waiting();
        std::thread::yield_now();
    }
    Some(())
}

/// The uses of `decls` in `files`: each file is read once, from disk before the lock is taken,
/// and `hit` gets its text with each use (index into `decls`, offset). `None` when cancelled.
fn uses_in_files(
    s: &State,
    texts: &Texts,
    cache: &Cache,
    stale: &dyn Fn() -> bool,
    files: &[PathBuf],
    decls: &[DeclRef],
    mut hit: impl FnMut(&Path, &str, usize, usize),
) -> Option<()> {
    let names: Vec<String> = s
        .resolve(texts, cache, |r| r.names_of(decls))
        .into_iter()
        .flatten()
        .collect();
    let (mut next, mut ready) = (0, VecDeque::new());
    loop {
        if stale() {
            return None;
        }
        // without the lock: the next files that spell one of the names
        while ready.is_empty() && next < files.len() {
            let end = (next + AHEAD).min(files.len());
            for f in &files[next..end] {
                let on_disk = (!texts.open.contains_key(f)).then(|| stamp(f)).flatten();
                if let Some(st) = on_disk
                    && let Some((was, bits)) = s.spells.borrow().get(f)
                    && *was == st
                    && !names.iter().any(|n| may_spell(bits, n))
                {
                    continue;
                }
                let Some(t) = texts.read(f) else { continue };
                if let Some(st) = on_disk {
                    s.spells.borrow_mut().insert(f.clone(), (st, spelling(&t)));
                }
                if names.iter().any(|n| t.contains(n.as_str())) {
                    ready.push_back((f, t));
                }
            }
            next = end;
        }
        if ready.is_empty() {
            return Some(());
        }
        let t = Instant::now();
        s.with(texts, cache, |r| {
            while let Some((f, text)) = ready.pop_front() {
                let found = r.usages_in(decls, f, &text);
                if texts.missed() {
                    ready.push_front((f, text));
                    break;
                }
                for (ti, at) in found {
                    hit(f, &text, ti, at);
                }
                if t.elapsed() >= s.budget {
                    break;
                }
            }
        });
        s.read_missed(texts, cache);
        s.serve_waiting();
        std::thread::yield_now();
    }
}

/// The handle the editor keeps: sends tasks to the worker thread.
pub(super) struct Xref {
    tx: Sender<Queued>,
    latest: HashMap<&'static str, Arc<AtomicU64>>,
    seq: u64,
    /// The newest file result, and where the next ones arrive.
    pub file: Option<FileXref>,
    file_rx: Option<Receiver<FileXref>>,
    /// (path, rev, index generation) last asked for.
    asked: Option<(PathBuf, u64, u64)>,
}

impl Xref {
    pub fn start(index: Shared, ctx: Option<egui::Context>) -> Xref {
        let (tx, rx) = channel::<Queued>();
        let spawned = std::thread::Builder::new()
            .name("droid-lite-xref".into())
            // name resolution recurses through receivers and supertypes
            .stack_size(64 << 20)
            .spawn(move || {
                let mut state = State::new(index, ctx);
                state.inbox = Some(rx);
                while let Some((kind, task)) = state.next(true) {
                    run(&state, kind, task);
                }
            });
        if spawned.is_err() {
            eprintln!("droid-lite: could not start the code insight worker");
        }
        Xref {
            tx,
            latest: HashMap::new(),
            seq: 0,
            file: None,
            file_rx: None,
            asked: None,
        }
    }

    /// Queues `f` as the newest task of `kind`; an older one of that kind stops at its next check.
    pub fn submit(
        &mut self,
        kind: &'static str,
        f: impl FnOnce(&State, &dyn Fn() -> bool) + Send + 'static,
    ) {
        self.seq += 1;
        let seq = self.seq;
        let latest = self.latest.entry(kind).or_default().clone();
        latest.store(seq, Ordering::SeqCst);
        let task: Task = Box::new(move |s, _| {
            let stale = || latest.load(Ordering::SeqCst) != seq;
            if !stale() {
                f(s, &stale);
            }
        });
        let _ = self.tx.send((kind, task));
    }

    /// Stops every queued or running task (another project opens): none of them answers, and
    /// what the worker kept of the old project is dropped.
    pub fn cancel_all(&mut self) {
        for latest in self.latest.values() {
            latest.store(0, Ordering::SeqCst);
        }
        self.file = None;
        self.file_rx = None;
        self.asked = None;
        self.submit("forget", |s, _| s.forget());
    }

    /// Takes a finished file result.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.file_rx else {
            return false;
        };
        let mut got = false;
        loop {
            match rx.try_recv() {
                Ok(r) => {
                    self.file = Some(r);
                    got = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // cancelled or failed: asked again when the file or the index changes
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.file_rx = None;
                    break;
                }
            }
        }
        got
    }

    /// Whether the file's insight was already asked for at this text and index version.
    pub fn asked_for(&self, path: &Path, rev: u64, generation: u64) -> bool {
        self.asked
            .as_ref()
            .is_some_and(|(p, r, g)| p == path && *r == rev && *g == generation)
    }

    /// The newest result for this file at this text version.
    pub fn for_file(&self, path: &Path, rev: u64) -> Option<&FileXref> {
        self.file
            .as_ref()
            .filter(|f| f.path == path && f.rev == rev)
    }

    /// Asks for the file's insight when its text or the index changed since the last ask.
    #[allow(clippy::too_many_arguments)]
    pub fn want_file(
        &mut self,
        path: &Path,
        rev: u64,
        generation: u64,
        text: &str,
        open: HashMap<PathBuf, Arc<str>>,
        root: PathBuf,
        targets: Vec<((Option<String>, String), usize)>,
    ) {
        let key = (path.to_path_buf(), rev, generation);
        if self.asked.as_ref() == Some(&key) {
            return;
        }
        self.asked = Some(key);
        let (tx, rx) = channel();
        self.file_rx = Some(rx);
        let path = path.to_path_buf();
        let text: Arc<str> = Arc::from(text);
        self.submit(FILE, move |s, stale| {
            let snap = s.snapshot(&root, &path);
            if let Some(r) = file_pass(s, &path, rev, text, open, &snap, &root, &targets, stale) {
                let _ = tx.send(r);
                if let Some(c) = &s.ctx {
                    c.request_repaint();
                }
            }
        });
    }

    /// Finds the uses of `target` in the project's code files (under `root`) on the worker;
    /// `reply` gets them.
    pub fn find_usages(
        &mut self,
        kind: &'static str,
        target: DeclRef,
        open: HashMap<PathBuf, Arc<str>>,
        root: PathBuf,
        reply: impl FnOnce(Vec<FoundLine>) + Send + 'static,
    ) {
        self.submit(kind, move |s, stale| {
            let files = s.code_files(&root);
            let texts = Texts::new(open);
            let cache = Cache::default();
            let mut found = Vec::new();
            let target = std::slice::from_ref(&target);
            let _ = uses_in_files(
                s,
                &texts,
                &cache,
                stale,
                &files,
                target,
                |f, text, _, at| {
                    found.push(super::found_line(f, text, at));
                },
            );
            // the declaration is listed too, like the plain search does (the hover card and the
            // usages dropdown leave it out)
            if let Some(text) = texts.read(&target[0].0) {
                found.push(super::found_line(&target[0].0, &text, target[0].1));
                found.sort_by(|a, b| a.path.cmp(&b.path).then(a.offset.cmp(&b.offset)));
            }
            if !stale() {
                reply(found);
                if let Some(c) = &s.ctx {
                    c.request_repaint();
                }
            }
        });
    }
}

/// Runs one task; a panic is logged and the task dropped (its reply channel closes, so whoever
/// waits sees it ended), and the worker goes on with the next.
fn run(state: &State, kind: &str, task: Task) {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(state, &|| false)));
    if ran.is_err() {
        eprintln!("droid-lite: a code insight task ({kind}) failed; the worker goes on");
        // a pass cut short may have left the graph half taken
        state.graph.replace(HiltGraph::default());
    }
}

fn key_hash(text: &str, targets: &[((Option<String>, String), usize)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (text, targets).hash(&mut h);
    h.finish()
}

/// Each name the text spells (comments and strings too: more is safe).
fn words_of(text: &str) -> HashSet<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.starts_with(|c: char| c.is_alphabetic() || c == '_'))
        .collect()
}

/// A digest of what the index holds for `path`: its package and each declaration's name, place,
/// container and kind.
fn own_digest(index: &crate::index::Index, path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(f) = index.files.get(path) {
        f.package.hash(&mut h);
        for d in &f.decls {
            (&d.name, d.offset, &d.container, d.kind).hash(&mut h);
        }
    }
    h.finish()
}

/// A digest of the project's declarations named `name`: which files declare it, in which
/// package and container, of which kind and with which modifiers (`private`, `internal`), but
/// not where in the file, so an edit above them keeps it.
fn digest(index: &crate::index::Index, name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    index
        .named(name)
        .map(|(p, f, d)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (p, &f.package, &d.container, d.kind, &d.modifiers).hash(&mut h);
            h.finish()
        })
        .fold(0, u64::wrapping_add)
}

#[allow(clippy::too_many_arguments)]
fn file_pass(
    s: &State,
    path: &Path,
    rev: u64,
    text: Arc<str>,
    mut open: HashMap<PathBuf, Arc<str>>,
    snap: &Snapshot,
    root: &Path,
    targets: &[((Option<String>, String), usize)],
    stale: &dyn Fn() -> bool,
) -> Option<FileXref> {
    open.insert(path.to_path_buf(), text.clone());
    let texts = Texts::new(open);
    let cache = Cache::default();
    let started = Instant::now();
    let hash = key_hash(&text, targets);
    let valid = s
        .done
        .borrow()
        .get(path)
        .filter(|k| k.hash == hash)
        .and_then(|k| still_valid(s, path, k, &texts));
    let kept = valid.and_then(|generation| {
        let mut done = s.done.borrow_mut();
        let k = done.get_mut(path)?;
        k.generation = generation;
        k.used = s.touch();
        Some((k.refs.clone(), k.usages.clone()))
    });
    let (refs, usages) = match kept {
        Some(kept) => kept,
        None => {
            let mut kept = names_and_usages(
                s, path, hash, &text, &texts, &cache, snap, root, targets, stale,
            )?;
            kept.used = s.touch();
            let out = (kept.refs.clone(), kept.usages.clone());
            let mut done = s.done.borrow_mut();
            done.insert(path.to_path_buf(), kept);
            if done.len() > 24 {
                // the least recently used go
                let mut by_use: Vec<(u64, PathBuf)> =
                    done.iter().map(|(p, k)| (k.used, p.clone())).collect();
                by_use.sort();
                for (_, p) in by_use.into_iter().take(4) {
                    done.remove(&p);
                }
            }
            out
        }
    };
    // Hilt links every time: a change to a module elsewhere moves them
    let hilt = hilt_pass(s, &texts, &cache, path, root, stale)?;
    if std::env::var_os("DROID_LITE_XREF_LOG").is_some() {
        eprintln!("xref {} in {:?}", path.display(), started.elapsed());
    }
    Some(FileXref {
        path: path.to_path_buf(),
        rev,
        refs,
        usages,
        hilt,
    })
}

/// What each name of the file means and where each of its declarations is used, with what that
/// depends on; `None` when cancelled.
#[allow(clippy::too_many_arguments)]
fn names_and_usages(
    s: &State,
    path: &Path,
    hash: u64,
    text: &str,
    texts: &Texts,
    cache: &Cache,
    snap: &Snapshot,
    root: &Path,
    targets: &[((Option<String>, String), usize)],
    stale: &dyn Fn() -> bool,
) -> Option<Kept> {
    // what each name in the file means, a step at a time
    let mut refs = Vec::new();
    let mut from = 0;
    in_chunks(s, texts, cache, stale, |r| {
        let to = (from + STEP).min(text.len());
        let got = r.refs_between(path, from, to);
        if texts.missed() {
            return false;
        }
        refs.extend(got);
        from = to;
        from == text.len()
    })?;
    // the declarations behind each name it spells
    let spelled: Vec<&str> = words_of(text).into_iter().collect();
    let mut words = HashMap::new();
    in_chunks(s, texts, cache, stale, |r| {
        let Some(w) = spelled.get(words.len()) else {
            return true;
        };
        words.insert(w.to_string(), digest(r.index, w));
        words.len() == spelled.len()
    })?;
    // where each declaration is used: one read of each file for all of them
    let decls: Vec<DeclRef> = targets
        .iter()
        .map(|(_, o)| (path.to_path_buf(), *o))
        .collect();
    let names: Vec<String> = targets.iter().map(|((_, n), _)| n.clone()).collect();
    let mut usages: Usages = targets
        .iter()
        .map(|(k, _)| (k.clone(), Vec::new()))
        .collect();
    uses_in_files(
        s,
        texts,
        cache,
        stale,
        &snap.files,
        &decls,
        |f, text, ti, at| {
            let l = super::found_line(f, text, at);
            let rel = f.strip_prefix(root).unwrap_or(f);
            usages.get_mut(&targets[ti].0).unwrap().push(Hit {
                name: l.text,
                detail: format!("{}:{}", rel.display(), l.line),
                path: f.to_path_buf(),
                offset: at,
                score: 0,
            });
        },
    )?;
    // where its names are declared, and every file read on the way (the type of a receiver,
    // a supertype)
    let mut deps = cache.files_read();
    for (_, _, r) in &refs {
        if let Res::Decls(v) = r {
            deps.extend(v.iter().map(|d| d.0.clone()));
        }
    }
    Some(Kept {
        hash,
        generation: snap.generation,
        own: snap.own,
        used: 0,
        deps,
        names,
        words,
        refs: Arc::new(refs),
        usages: Arc::new(usages),
    })
}

/// The index generation a kept result is good at: no file changed since that it read, that
/// now names one of its declarations, or that declares a name it spells anew. `None` when it
/// must be worked out again.
fn still_valid(s: &State, path: &Path, k: &Kept, texts: &Texts) -> Option<u64> {
    // under the lock only lookups; reading the changed files waits until it is let go
    let (now, changed) = {
        let index = s.index.read();
        let changed: Vec<Arc<Path>> = index.changed_since(k.generation)?;
        // the file's own entry: the declarations it was worked out for
        let (own, changed): (Vec<_>, Vec<_>) = changed.into_iter().partition(|p| **p == *path);
        if !own.is_empty() && own_digest(&index, path) != k.own {
            return None;
        }
        for p in &changed {
            if k.deps.contains(&**p) {
                return None;
            }
            let declared = index.files.get(&**p).into_iter().flat_map(|f| &f.decls);
            for d in declared {
                if k.words
                    .get(&d.name)
                    .is_some_and(|g| *g != digest(&index, &d.name))
                {
                    return None;
                }
            }
        }
        (index.generation, changed)
    };
    let names_one = |p: &Path| {
        texts
            .read(p)
            .is_some_and(|t| k.names.iter().any(|n| t.contains(n.as_str())))
    };
    (!changed.iter().any(|p| names_one(p))).then_some(now)
}

fn to_hit(r: &Resolver, d: &DeclRef, root: &Path) -> Hit {
    let (name, detail) = r.describe(d).unwrap_or_default();
    let rel = d.0.strip_prefix(root).unwrap_or(&d.0);
    Hit {
        name,
        detail: if detail.is_empty() {
            rel.display().to_string()
        } else {
            detail
        },
        path: d.0.clone(),
        offset: d.1,
        score: 0,
    }
}

/// One Hilt link to describe, from the graph alone.
enum Want {
    /// A binding (at this offset) and the owner and place of each site that gets it.
    Injected(usize, Vec<(DeclRef, usize)>),
    /// An injection site: its owner, parameter and providers.
    Site(DeclRef, Option<usize>, Vec<DeclRef>),
}

/// What the graph says about the declarations of `path`; `None` when cancelled.
fn hilt_wants(graph: &HiltGraph, path: &Path, stale: &dyn Fn() -> bool) -> Option<Vec<Want>> {
    let mut wants = Vec::new();
    let Some((_, part)) = graph.files.get(path) else {
        return Some(wants);
    };
    let keys = graph.keys();
    for b in &part.providers {
        if stale() {
            return None;
        }
        let sites: Vec<(DeclRef, usize)> = keys
            .injected_in(b)
            .iter()
            .map(|s| (s.owner.clone(), s.at))
            .collect();
        if !sites.is_empty() {
            wants.push(Want::Injected(b.decl.1, sites));
        }
    }
    for site in &part.sites {
        if stale() {
            return None;
        }
        let providers: Vec<DeclRef> = keys
            .providers_of(site)
            .iter()
            .map(|b| b.decl.clone())
            .collect();
        if !providers.is_empty() {
            wants.push(Want::Site(site.owner.clone(), site.param, providers));
        }
    }
    Some(wants)
}

/// The Hilt links of one file: the project's graph brought up to date (only changed files are
/// read again, from disk before the lock is taken), then "injected in N places" on each
/// binding, and on each injection site the binding it gets. `None` when cancelled.
fn hilt_pass(
    s: &State,
    texts: &Texts,
    cache: &Cache,
    path: &Path,
    root: &Path,
    stale: &dyn Fn() -> bool,
) -> Option<Vec<HiltLink>> {
    let mut graph = s.graph.take();
    loop {
        if stale() {
            s.graph.replace(graph);
            return None;
        }
        texts.read_ahead(graph.upcoming(AHEAD));
        let t = Instant::now();
        let finished = s.with(texts, cache, |r| {
            graph.refresh(r, &|| t.elapsed() >= s.budget || stale())
        });
        s.read_missed(texts, cache);
        s.serve_waiting();
        std::thread::yield_now();
        if finished {
            break;
        }
    }
    // what the graph says, without the lock
    let wants = hilt_wants(&graph, path, stale);
    s.graph.replace(graph);
    let wants = wants?;
    // their names and types, a few at a time under the lock
    let mut links = Vec::new();
    // one link per declaration: a constructor's sites together, each to its providers
    let mut by_owner: Vec<(DeclRef, Vec<Hit>, Vec<String>)> = Vec::new();
    let mut i = 0;
    in_chunks(s, texts, cache, stale, |r| {
        let Some(want) = wants.get(i) else {
            return true;
        };
        match want {
            Want::Injected(at, sites) => {
                let n = sites.len();
                let hits = sites
                    .iter()
                    .map(|(owner, at)| {
                        let mut h = to_hit(r, owner, root);
                        h.offset = *at;
                        h
                    })
                    .collect();
                if texts.missed() {
                    return false;
                }
                links.push(HiltLink {
                    at: *at,
                    injected: true,
                    title: format!("Injected in {n} place{}", if n == 1 { "" } else { "s" }),
                    hits,
                });
            }
            Want::Site(owner, param, providers) => {
                let hits: Vec<Hit> = providers.iter().map(|d| to_hit(r, d, root)).collect();
                let ty = param.and_then(|i| r.param_type(owner, i));
                if texts.missed() {
                    return false;
                }
                match by_owner.iter_mut().find(|(o, _, _)| o == owner) {
                    Some((_, h, t)) => {
                        h.extend(hits);
                        t.extend(ty);
                    }
                    None => by_owner.push((owner.clone(), hits, ty.into_iter().collect())),
                }
            }
        }
        i += 1;
        i == wants.len()
    })?;
    for (owner, mut hits, types) in by_owner {
        hits.dedup_by(|a, b| a.path == b.path && a.offset == b.offset);
        let title = match (&hits[..], types.is_empty()) {
            // a constructor or `@Provides` function: what it needs
            (_, false) => format!("Dependencies: {}", types.join(", ")),
            ([one], true) => format!("Provided by {}", one.name),
            (_, true) => format!("{} providers", hits.len()),
        };
        links.push(HiltLink {
            at: owner.1,
            injected: false,
            title,
            hits,
        });
    }
    Some(links)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    /// The shared index of `files` (path, text) and their texts as open tabs.
    fn project(files: &[(&str, &str)]) -> (Shared, HashMap<PathBuf, Arc<str>>) {
        let (index, open) = build(files);
        let shared = Shared::default();
        shared.set(index);
        shared.settle();
        (shared, open)
    }

    fn build(files: &[(&str, &str)]) -> (Index, HashMap<PathBuf, Arc<str>>) {
        let mut index = Index::default();
        let mut open = HashMap::new();
        for (p, t) in files {
            index.update(Path::new(p), t);
            open.insert(PathBuf::from(p), Arc::from(*t));
        }
        (index, open)
    }

    /// A file changed: in the index and the open texts.
    fn change(shared: &Shared, open: &mut HashMap<PathBuf, Arc<str>>, path: &str, text: &str) {
        let (p, t) = (PathBuf::from(path), text.to_string());
        shared.change(move |i| i.update(&p, &t));
        shared.settle();
        open.insert(PathBuf::from(path), Arc::from(text));
    }

    fn pass(s: &State, path: &str, rev: u64, open: &HashMap<PathBuf, Arc<str>>) -> FileXref {
        let path = Path::new(path);
        let text = open[path].clone();
        let targets: Vec<((Option<String>, String), usize)> = crate::kotlin::scan_file(path, &text)
            .decls
            .iter()
            .map(|d| {
                (
                    (d.container.as_deref().map(String::from), d.name.clone()),
                    d.offset,
                )
            })
            .collect();
        let snap = s.snapshot(Path::new("/p"), path);
        file_pass(
            s,
            path,
            rev,
            text,
            open.clone(),
            &snap,
            Path::new("/p"),
            &targets,
            &|| false,
        )
        .unwrap()
    }

    /// What the `n`-th `name` (from 1) of `path` means, as `file:line` of each declaration.
    fn meaning(
        x: &FileXref,
        open: &HashMap<PathBuf, Arc<str>>,
        path: &str,
        name: &str,
        n: usize,
    ) -> String {
        let o = crate::refactor::occurrences(&open[Path::new(path)], name)[n - 1].start;
        match x.at(o) {
            Some(Res::Decls(v)) => v
                .iter()
                .map(|(p, at)| {
                    format!(
                        "{}:{}",
                        p.display(),
                        open[p][..*at].matches('\n').count() + 1
                    )
                })
                .collect::<Vec<_>>()
                .join(" "),
            Some(Res::Local(_)) => "local".into(),
            Some(Res::None) | None => "-".into(),
        }
    }

    #[test]
    fn a_project_read_again_never_reuses_a_result_of_the_old_index() {
        let (a, u) = ("/p/A.kt", "/p/U.kt");
        let use_a = "package a\nfun zz(x: A) = 1\n";
        let (shared, mut open) = project(&[(a, "package a\nclass A\n"), (u, use_a)]);
        let s = State::new(shared.clone(), None);
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:2"
        );
        // a checkout in a terminal moved A; the project is opened again (a fresh index, the
        // tab's version count starts again)
        let (index, again) = build(&[(a, "package a\n\n// moved\n\nclass A\n"), (u, use_a)]);
        shared.set(index);
        shared.settle();
        open = again;
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:5"
        );
    }

    #[test]
    fn a_tab_with_other_text_at_the_same_version_is_worked_out_again() {
        let (a, u) = ("/p/A.kt", "/p/U.kt");
        let (shared, mut open) = project(&[
            (a, "package a\nclass A\n"),
            (u, "package a\nfun zz(x: A) = 1\n"),
        ]);
        let s = State::new(shared, None);
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:2"
        );
        // the index did not change, the tab's text did (a tab opened again)
        open.insert(
            PathBuf::from(u),
            Arc::from("package a\n\n\n\nfun zz(x: A) = 1\n"),
        );
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:2"
        );
    }

    #[test]
    fn a_kept_result_follows_a_package_or_visibility_change_elsewhere() {
        let (f, u) = ("/p/F.kt", "/p/U.kt");
        let (shared, mut open) = project(&[
            (f, "package b\nclass Foo\n"),
            (u, "package a\nfun zz() = Foo()\n"),
        ]);
        let s = State::new(shared.clone(), None);
        assert_eq!(meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1), "-");
        // only the `package` line of the other file changes
        change(&shared, &mut open, f, "package a\nclass Foo\n");
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1),
            "/p/F.kt:2"
        );
        change(&shared, &mut open, f, "package a\nprivate class Foo\n");
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1),
            "-",
            "private to its file"
        );
    }

    #[test]
    fn a_kept_result_follows_a_change_to_a_supertype_file_it_read() {
        let u = "/p/U.kt";
        let (shared, mut open) = project(&[
            (
                "/p/Base.kt",
                "package a\nopen class Base {\n    open fun go() = 1\n}\n",
            ),
            (
                "/p/Other.kt",
                "package a\nopen class Other {\n    open fun go() = 2\n}\n",
            ),
            ("/p/M.kt", "package a\nopen class M : Base()\n"),
            ("/p/C.kt", "package a\nclass C : M()\n"),
            (u, "package a\nfun zz(c: C) = c.go()\n"),
        ]);
        let s = State::new(shared.clone(), None);
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "go", 1),
            "/p/Base.kt:3"
        );
        // the class between C and the member changes its supertype; U spells neither
        change(
            &shared,
            &mut open,
            "/p/M.kt",
            "package a\nopen class M : Other()\n",
        );
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "go", 1),
            "/p/Other.kt:3"
        );
    }

    #[test]
    fn a_name_whose_file_is_deleted_means_nothing_any_more() {
        let (f, u) = ("/p/F.kt", "/p/U.kt");
        let (shared, mut open) = project(&[
            (f, "package a\nclass Foo\n"),
            (u, "package a\nfun zz() = Foo()\n"),
        ]);
        let s = State::new(shared.clone(), None);
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1),
            "/p/F.kt:2"
        );
        shared.change(move |i| i.remove(Path::new(f)));
        shared.settle();
        open.remove(Path::new(f));
        assert_eq!(meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1), "-");
    }

    #[test]
    fn a_kept_result_shares_its_names_and_usages_not_a_copy() {
        let u = "/p/U.kt";
        let (shared, open) = project(&[(u, "package a\nclass A\nfun zz(x: A) = 1\n")]);
        let s = State::new(shared, None);
        let (one, two) = (pass(&s, u, 1, &open), pass(&s, u, 2, &open));
        assert!(Arc::ptr_eq(&one.refs, &two.refs));
        assert!(Arc::ptr_eq(&one.usages, &two.usages));
    }

    #[test]
    fn a_request_someone_waits_for_runs_between_chunks_of_a_long_pass() {
        use std::sync::atomic::AtomicBool;
        let mut x = Xref::start(Shared::default(), None);
        let served = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = channel();
        let flag = served.clone();
        x.submit(FILE, move |s, stale| {
            let (texts, cache) = (Texts::default(), Cache::default());
            let t = Instant::now();
            // chunk after chunk until the request ran (or ten seconds went by)
            let _ = in_chunks(s, &texts, &cache, stale, |_| {
                std::thread::sleep(Duration::from_millis(1));
                flag.load(Ordering::SeqCst) || t.elapsed() > Duration::from_secs(10)
            });
            let _ = done_tx.send(flag.load(Ordering::SeqCst));
        });
        std::thread::sleep(Duration::from_millis(50));
        let (tx, rx) = channel();
        let flag = served.clone();
        x.submit("find", move |_, _| {
            flag.store(true, Ordering::SeqCst);
            let _ = tx.send(());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(()),
            "answered before the pass ended"
        );
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(15)),
            Ok(true),
            "the pass went on after it"
        );
    }

    #[test]
    fn a_kept_result_gets_the_hilt_links_of_now() {
        let (m, h) = ("/p/src/main/java/a/Mod.kt", "/p/src/main/java/a/Holder.kt");
        let module = |q: &str| {
            format!(
                "package a\n@Module\nobject Mod {{\n    @Provides @Named(\"{q}\") fun io(): String = \"\"\n}}\n"
            )
        };
        let holder =
            "package a\nclass Holder @Inject constructor(@Named(\"a\") val value: String)\n";
        let (shared, mut open) = project(&[(m, &module("a")), (h, holder)]);
        let s = State::new(shared.clone(), None);
        let links = |x: &FileXref| -> Vec<String> {
            x.hilt
                .iter()
                .filter(|l| !l.injected)
                .map(|l| l.title.clone())
                .collect()
        };
        assert_eq!(links(&pass(&s, h, 1, &open)), ["Dependencies: String"]);
        change(&shared, &mut open, m, &module("b"));
        assert!(
            links(&pass(&s, h, 1, &open)).is_empty(),
            "@Named(\"b\") is not what it asks for"
        );
        change(&shared, &mut open, m, &module("a"));
        assert_eq!(links(&pass(&s, h, 1, &open)), ["Dependencies: String"]);
    }

    #[test]
    fn a_name_declared_later_is_found() {
        let u = "/p/U.kt";
        let (shared, mut open) = project(&[(u, "package a\nfun zz() = Foo()\n")]);
        let s = State::new(shared.clone(), None);
        assert_eq!(meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1), "-");
        // an unrelated change keeps the kept result
        change(&shared, &mut open, "/p/B.kt", "package a\nclass Bar\n");
        let kept = s.done.borrow()[Path::new(u)].generation;
        pass(&s, u, 1, &open);
        assert!(
            s.done.borrow()[Path::new(u)].generation > kept,
            "reused, now known good later"
        );
        change(&shared, &mut open, "/p/F.kt", "package a\nclass Foo\n");
        assert_eq!(
            meaning(&pass(&s, u, 1, &open), &open, u, "Foo", 1),
            "/p/F.kt:2"
        );
    }

    #[test]
    fn short_lock_holds_give_the_same_result() {
        let a = "/p/A.kt";
        let mut text = String::from("package a\nclass A {\n    fun go() = 1\n}\n");
        for n in 0..80 {
            text.push_str(&format!("fun f{n}(x: A) = x.go()\n"));
        }
        let (shared, open) = project(&[(a, &text)]);
        let mut long = State::new(shared.clone(), None);
        long.budget = Duration::from_secs(60);
        let one = pass(&long, a, 1, &open);
        let mut short = State::new(shared, None);
        short.budget = Duration::ZERO;
        let other = pass(&short, a, 1, &open);
        assert_eq!(format!("{:?}", one.refs), format!("{:?}", other.refs));
        assert_eq!(one.usages[&(None, "A".to_string())].len(), 80);
        assert_eq!(other.usages[&(None, "A".to_string())].len(), 80);
        assert!(
            short.holds.get() >= (text.len() / STEP) as u64 + 2 * long.holds.get(),
            "{} holds with no time to spare, {} with a minute",
            short.holds.get(),
            long.holds.get()
        );
    }

    #[test]
    fn a_task_that_panics_leaves_the_worker_running() {
        let mut x = Xref::start(Shared::default(), None);
        let (tx, rx) = channel::<u8>();
        x.submit("a", move |_, _| {
            let _reply = tx;
            panic!("a bug in a task");
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "its reply closes, nobody waits forever"
        );
        let (tx, rx) = channel();
        x.submit("b", move |_, _| {
            let _ = tx.send(1);
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(1));
        // and cancelled ones never answer
        let (tx, rx) = channel::<u8>();
        let (gate_tx, gate) = channel::<()>();
        x.submit("c", move |_, _| {
            let _ = gate.recv();
        });
        x.submit("d", move |_, _| {
            let _ = tx.send(1);
        });
        x.cancel_all();
        // "c" may be cancelled before it starts: then nobody waits at the gate
        let _ = gate_tx.send(());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        );
    }

    /// The declarations of `text` as a tab passes them.
    fn targets_of(path: &str, text: &str) -> Vec<((Option<String>, String), usize)> {
        crate::kotlin::scan_file(Path::new(path), text)
            .decls
            .iter()
            .map(|d| {
                (
                    (d.container.as_deref().map(String::from), d.name.clone()),
                    d.offset,
                )
            })
            .collect()
    }

    fn uses_of_a(x: &FileXref) -> usize {
        x.usages[&(None, "A".to_string())].len()
    }

    #[test]
    fn a_result_for_declarations_of_an_older_text_is_not_kept_for_the_new_ones() {
        let (u, v) = ("/p/U.kt", "/p/V.kt");
        let old = "package a\nclass A\n";
        let new = "package a\n// a line typed above\nclass A\n";
        let (shared, mut open) = project(&[(u, old), (v, "package a\nfun yy(x: A) = 2\n")]);
        let s = State::new(shared.clone(), None);
        change(&shared, &mut open, u, new);
        // the tab's symbols are still those of the old text (offsets above the typed line)
        let snap = s.snapshot(Path::new("/p"), Path::new(u));
        let text = open[Path::new(u)].clone();
        let stale_targets = targets_of(u, old);
        let first = file_pass(
            &s,
            Path::new(u),
            1,
            text,
            open.clone(),
            &snap,
            Path::new("/p"),
            &stale_targets,
            &|| false,
        )
        .unwrap();
        assert!(
            first
                .usages
                .get(&(None, "A".to_string()))
                .is_none_or(|v| v.is_empty())
        );
        // then the tab settles: same text, its own declarations
        assert_eq!(uses_of_a(&pass(&s, u, 2, &open)), 1);
    }

    #[test]
    fn a_result_worked_out_before_the_index_had_the_file_is_not_kept() {
        let (u, v) = ("/p/U.kt", "/p/V.kt");
        let new = "package a\n// a line typed above\nclass A\n";
        let (shared, mut open) = project(&[
            (u, "package a\nclass A\n"),
            (v, "package a\nfun yy(x: A) = 2\n"),
        ]);
        let s = State::new(shared.clone(), None);
        // the tab's text and symbols changed; the index still has the old place of A
        open.insert(PathBuf::from(u), Arc::from(new));
        pass(&s, u, 1, &open);
        // the index catches up; the text is the same
        change(&shared, &mut open, u, new);
        assert_eq!(uses_of_a(&pass(&s, u, 1, &open)), 1);
    }

    #[test]
    fn a_file_added_after_the_pass_started_is_seen_by_the_next_pass() {
        let u = "/p/U.kt";
        let (shared, mut open) = project(&[(u, "package a\nclass A\n")]);
        let s = State::new(shared.clone(), None);
        let snap = s.snapshot(Path::new("/p"), Path::new(u));
        // created between the file list and the rest of the pass
        change(
            &shared,
            &mut open,
            "/p/V.kt",
            "package a\nfun yy(x: A) = 2\n",
        );
        let text = open[Path::new(u)].clone();
        let targets = targets_of(u, &text);
        let first = file_pass(
            &s,
            Path::new(u),
            1,
            text,
            open.clone(),
            &snap,
            Path::new("/p"),
            &targets,
            &|| false,
        )
        .unwrap();
        assert_eq!(uses_of_a(&first), 0, "not in the list it started from");
        assert_eq!(uses_of_a(&pass(&s, u, 1, &open)), 1);
    }

    #[test]
    fn the_least_recently_used_kept_result_goes_first() {
        let names: Vec<String> = (0..25).map(|n| format!("/p/F{n}.kt")).collect();
        let texts: Vec<String> = (0..25)
            .map(|n| format!("package a\nclass C{n}\n"))
            .collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .zip(&texts)
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let (shared, open) = project(&files);
        let s = State::new(shared, None);
        for p in &names[..24] {
            pass(&s, p, 1, &open);
        }
        // the oldest one is used again, then a 25th comes
        pass(&s, &names[0], 1, &open);
        pass(&s, &names[24], 1, &open);
        let done = s.done.borrow();
        assert!(done.contains_key(Path::new(&names[0])), "used last but one");
        assert!(!done.contains_key(Path::new(&names[1])));
        assert!(done.contains_key(Path::new(&names[24])));
    }

    #[test]
    fn a_project_switch_drops_what_the_worker_kept() {
        let u = "/p/src/main/java/a/U.kt";
        let text = "package a\nclass U @Inject constructor()\n";
        let (shared, open) = project(&[(u, text)]);
        let mut x = Xref::start(shared, None);
        let (tx, rx) = channel();
        x.submit("probe", move |s, _| {
            let snap = s.snapshot(Path::new("/p"), Path::new(u));
            let text = open[Path::new(u)].clone();
            let targets = targets_of(u, &text);
            file_pass(
                s,
                Path::new(u),
                1,
                text,
                open.clone(),
                &snap,
                Path::new("/p"),
                &targets,
                &|| false,
            );
            let _ = tx.send((s.done.borrow().len(), s.graph.borrow().files.len()));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok((1, 1)));
        x.cancel_all();
        let (tx, rx) = channel();
        x.submit("probe", move |s, _| {
            let _ = tx.send((s.done.borrow().len(), s.graph.borrow().files.len()));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok((0, 0)));
    }

    #[test]
    fn a_file_from_disk_is_read_while_the_editor_can_change_the_index() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("droid-lite-xref-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (b, c, u) = (dir.join("Base.kt"), dir.join("C.kt"), dir.join("U.kt"));
        let base = "package a\nopen class Base {\n    fun go() = 1\n}\n";
        let (c_text, u_text) = (
            "package a\nclass C : Base()\n",
            "package a\nfun zz(c: C) = c.go()\n",
        );
        std::fs::write(&c, c_text).unwrap();
        // a slow disk: reading Base.kt waits until it is written
        let fifo = std::ffi::CString::new(b.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut index = Index::default();
        for (p, t) in [(&b, base), (&c, c_text), (&u, u_text)] {
            index.update(p, t);
        }
        let shared = Shared::default();
        shared.set(index);
        shared.settle();
        let open: HashMap<PathBuf, Arc<str>> = [(u.clone(), Arc::from(u_text))].into();
        let (inner, path) = (shared.clone(), u.clone());
        let pass = std::thread::spawn(move || {
            let s = State::new(inner, None);
            let snap = Snapshot {
                generation: s.index.read().generation,
                files: Arc::new(vec![path.clone()]),
                own: 0,
            };
            let text = open[&path].clone();
            let targets = targets_of(path.to_str().unwrap(), &text);
            let root = path.parent().unwrap().to_path_buf();
            let x = file_pass(
                &s,
                &path,
                1,
                text,
                open.clone(),
                &snap,
                &root,
                &targets,
                &|| false,
            );
            (x.unwrap(), open)
        });
        std::thread::sleep(Duration::from_millis(300));
        // the editor queues a change while the worker waits for the disk
        shared.update(&dir.join("D.kt"), "package a\nclass D\n");
        let free = shared.flush();
        std::fs::write(&b, base).unwrap();
        let (x, mut open) = pass.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(free, "the index was locked while the disk was read");
        open.insert(b.clone(), Arc::from(base));
        let go = meaning(&x, &open, u.to_str().unwrap(), "go", 1);
        assert_eq!(go, format!("{}:3", b.display()));
    }

    #[test]
    fn hilt_links_stop_when_cancelled() {
        let h = "/p/src/main/java/a/Holder.kt";
        let text = "package a\nclass Dep @Inject constructor()\nclass Holder @Inject constructor(val dep: Dep)\n";
        let (shared, open) = project(&[(h, text)]);
        let index = shared.read();
        let texts = Texts::new(open);
        let r = Resolver::new(&index, &texts);
        let mut g = HiltGraph::default();
        while !g.refresh(&r, &|| false) {}
        assert!(hilt_wants(&g, Path::new(h), &|| false).is_some_and(|w| w.len() == 2));
        assert!(hilt_wants(&g, Path::new(h), &|| true).is_none());
    }

    #[test]
    fn an_unchanged_file_that_spells_none_of_the_names_is_not_read_again() {
        let dir =
            std::env::temp_dir().join(format!("droid-lite-xref-spell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (u, v) = (dir.join("U.kt"), dir.join("V.kt"));
        let before = "package a\nfun yy(x: Int) = 2\n";
        std::fs::write(&v, before).unwrap();
        let shared = Shared::default();
        let mut index = Index::default();
        index.update(&v, before);
        index.update(&u, "package a\nclass A\n");
        shared.set(index);
        shared.settle();
        let s = State::new(shared, None);
        let root = dir.clone();
        let pass_u = |text: &str| {
            let open: HashMap<PathBuf, Arc<str>> = [(u.clone(), Arc::from(text))].into();
            let snap = s.snapshot(&root, &u);
            let targets = targets_of(u.to_str().unwrap(), text);
            file_pass(
                &s,
                &u,
                1,
                Arc::from(text),
                open,
                &snap,
                &root,
                &targets,
                &|| false,
            )
            .unwrap()
        };
        assert_eq!(uses_of_a(&pass_u("package a\nclass A\n")), 0);
        // V now uses A, with the same size and time on disk: what was read of it still holds
        let time = std::fs::metadata(&v).unwrap().modified().unwrap();
        let after = "package a\nfun yy(x: A)   = 2\n";
        assert_eq!(after.len(), before.len());
        std::fs::write(&v, after).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&v)
            .unwrap()
            .set_modified(time)
            .unwrap();
        // the open file changes below A: its usages are looked for again
        let not_read = uses_of_a(&pass_u("package a\nclass A\n// 1\n"));
        // a changed time: read again
        std::fs::File::options()
            .write(true)
            .open(&v)
            .unwrap()
            .set_modified(time + Duration::from_secs(5))
            .unwrap();
        let read = uses_of_a(&pass_u("package a\nclass A\n// 2\n"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((not_read, read), (0, 1));
    }

    /// The pass over the biggest files of a real project: time, and the longest the index's
    /// lock was held (what an edit could wait). `DROID_LITE_ACCURACY_PROJECT=<dir>`.
    #[test]
    #[ignore = "needs a real project"]
    fn file_pass_on_a_real_project() {
        let Ok(root) = std::env::var("DROID_LITE_ACCURACY_PROJECT") else {
            return;
        };
        let root = std::fs::canonicalize(root).unwrap();
        let p = crate::project::Project::open(&root);
        let files: Vec<PathBuf> = p
            .files
            .iter()
            .filter(|f| crate::project::is_code(f))
            .cloned()
            .collect();
        let index = Shared::default();
        index.set(crate::index::Index::build(&files));
        index.settle();
        let mut by_size: Vec<(u64, PathBuf)> = files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "kt"))
            .map(|f| (std::fs::metadata(f).map_or(0, |m| m.len()), f.clone()))
            .collect();
        by_size.sort();
        let state = State::new(index.clone(), None);
        // what the editor does on its thread per request: copy the file list, look a name up
        let t = Instant::now();
        let copy: Vec<PathBuf> = files
            .iter()
            .filter(|f| crate::project::is_code(f))
            .cloned()
            .collect();
        eprintln!(
            "editor thread: request copy of {} paths {:?}",
            copy.len(),
            t.elapsed()
        );
        for (_, path) in by_size
            .iter()
            .rev()
            .take(3)
            .chain(by_size.iter().rev().skip(400).take(2))
        {
            let text: Arc<str> = Arc::from(std::fs::read_to_string(path).unwrap());
            let syms = crate::kotlin::scan_file(path, &text);
            let targets: Vec<((Option<String>, String), usize)> = syms
                .decls
                .iter()
                .map(|d| {
                    (
                        (d.container.as_deref().map(String::from), d.name.clone()),
                        d.offset,
                    )
                })
                .collect();
            LONGEST_LOCK.store(0, Ordering::Relaxed);
            let t = Instant::now();
            let r = file_pass(
                &state,
                path,
                1,
                text.clone(),
                HashMap::new(),
                &state.snapshot(&root, path),
                &root,
                &targets,
                &|| false,
            )
            .unwrap();
            eprintln!(
                "{} ({} KB, {} declarations): {:?}, {} names read, longest lock {} µs",
                path.file_name().unwrap().to_string_lossy(),
                text.len() / 1024,
                targets.len(),
                t.elapsed(),
                r.refs.len(),
                LONGEST_LOCK.load(Ordering::Relaxed)
            );
            let t = Instant::now();
            for o in (0..text.len()).step_by(97) {
                std::hint::black_box(r.at(o));
            }
            eprintln!(
                "  editor thread: {} lookups {:?}",
                text.len() / 97,
                t.elapsed()
            );
            // the same file again: from the kept result
            let t = Instant::now();
            file_pass(
                &state,
                path,
                1,
                text,
                HashMap::new(),
                &state.snapshot(&root, path),
                &root,
                &targets,
                &|| false,
            )
            .unwrap();
            eprintln!("  again: {:?}", t.elapsed());
        }
    }
}
