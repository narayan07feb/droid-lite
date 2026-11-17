//! The one background worker for code insight: when a file is opened (or edited), what each of
//! its names means, where each of its declarations is used and its Hilt links are worked out
//! there, in short chunks under the index's read lock so the editor can keep updating the index.
//! One thread, started once; a newer request of the same kind cancels the older one; finished
//! results are kept per file and reused while neither the file nor anything it depends on
//! changed. A task that panics is logged and dropped; the thread goes on.

use super::FoundLine;
use crate::index::{Hit, Shared};
use crate::resolve::{Cache, DeclRef, HiltGraph, Res, Resolver, Texts};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
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

/// A file's code insight, for one version of its text.
#[derive(Clone, Default)]
pub(super) struct FileXref {
    pub path: PathBuf,
    pub rev: u64,
    /// What each name means: start, end and meaning, in file order.
    pub refs: Vec<(usize, usize, Res)>,
    /// Uses of each declaration of the file, by container and name.
    pub usages: HashMap<(Option<String>, String), Vec<Hit>>,
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

type Task = Box<dyn FnOnce(&mut State, &dyn Fn() -> bool) + Send>;

/// What the worker keeps between tasks.
pub(super) struct State {
    index: Shared,
    graph: HiltGraph,
    /// Finished file results with what they depend on.
    done: HashMap<PathBuf, Kept>,
    ctx: Option<egui::Context>,
    /// How long one chunk may hold the index's read lock.
    budget: Duration,
    /// How many times the lock was taken.
    holds: Cell<u64>,
}

/// A file's names and usages (Hilt links are worked out again each time: the graph moves).
struct Kept {
    /// Hash of the text it was worked out for (a reopened tab starts its version count again).
    hash: u64,
    /// The index generation it is known good at.
    generation: u64,
    /// Files whose declarations it found.
    deps: HashSet<PathBuf>,
    /// The file's declaration names: a changed file that spells one may use it.
    names: Vec<String>,
    /// Each name the file spells, found or not, with a digest of its declarations in the
    /// project: a changed file that adds one may change what the name means here.
    words: HashMap<String, u64>,
    refs: Vec<(usize, usize, Res)>,
    usages: HashMap<(Option<String>, String), Vec<Hit>>,
}

impl State {
    fn new(index: Shared, ctx: Option<egui::Context>) -> State {
        State {
            index,
            graph: HiltGraph::default(),
            done: HashMap::new(),
            ctx,
            budget: CHUNK,
            holds: Cell::new(0),
        }
    }

    /// Runs `f` with a resolver under a short read lock.
    fn with<R>(&self, texts: &Texts, cache: &Cache, f: impl FnOnce(&Resolver) -> R) -> R {
        // the editor's queued changes go in first, under a short write lock
        self.index.flush();
        let t = Instant::now();
        let index = self.index.read();
        self.holds.set(self.holds.get() + 1);
        let r = Resolver::with(&index, texts, cache);
        let out = f(&r);
        drop(index);
        LONGEST_LOCK.fetch_max(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        out
    }

    /// The project's code files: what the index holds under `root`, sorted (worked out here,
    /// not on the editor's thread).
    pub(super) fn code_files(&self, root: &Path) -> Arc<Vec<PathBuf>> {
        self.index.flush();
        Arc::new(self.index.read().files_under(root))
    }
}

/// Runs `step` under the index's read lock until it says it is done, as many steps as fit in
/// the time budget per hold (at least one), letting the lock go between holds; `None` when
/// cancelled.
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
                if t.elapsed() >= s.budget {
                    break;
                }
            }
        });
        std::thread::yield_now();
    }
    Some(())
}

/// The handle the editor keeps: sends tasks to the worker thread.
pub(super) struct Xref {
    tx: Sender<(&'static str, u64, Task)>,
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
        let (tx, rx) = channel::<(&'static str, u64, Task)>();
        let spawned = std::thread::Builder::new()
            .name("droid-lite-xref".into())
            // name resolution recurses through receivers and supertypes
            .stack_size(64 << 20)
            .spawn(move || {
                let mut state = State::new(index, ctx);
                while let Ok(first) = rx.recv() {
                    // only the newest task of each kind runs
                    let mut tasks = vec![first];
                    while let Ok(t) = rx.try_recv() {
                        tasks.retain(|(k, _, _)| *k != t.0);
                        tasks.push(t);
                    }
                    for (kind, _, task) in tasks {
                        run(&mut state, kind, task);
                    }
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
        f: impl FnOnce(&mut State, &dyn Fn() -> bool) + Send + 'static,
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
        let _ = self.tx.send((kind, seq, task));
    }

    /// Stops every queued or running task (another project opens): none of them answers.
    pub fn cancel_all(&mut self) {
        for latest in self.latest.values() {
            latest.store(0, Ordering::SeqCst);
        }
        self.file = None;
        self.file_rx = None;
        self.asked = None;
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
        self.submit("file", move |s, stale| {
            let files = s.code_files(&root);
            if let Some(r) = file_pass(s, &path, rev, text, open, &files, &root, &targets, stale) {
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
            let texts = Texts { open };
            let cache = Cache::default();
            let mut found = Vec::new();
            let mut i = 0;
            let _ = in_chunks(s, &texts, &cache, stale, |r| {
                let Some(f) = files.get(i) else { return true };
                for (_, at) in r.usages_in(std::slice::from_ref(&target), f) {
                    if let Some(text) = texts.read(f) {
                        found.push(super::found_line(f, &text, at));
                    }
                }
                i += 1;
                i == files.len()
            });
            // the declaration is listed too, like the plain search does (the hover card and the
            // usages dropdown leave it out)
            if let Some(text) = texts.read(&target.0) {
                found.push(super::found_line(&target.0, &text, target.1));
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
fn run(state: &mut State, kind: &str, task: Task) {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(state, &|| false)));
    if ran.is_err() {
        eprintln!("droid-lite: a code insight task ({kind}) failed; the worker goes on");
        // a pass cut short may have left the graph half taken
        state.graph = HiltGraph::default();
    }
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Each name the text spells (comments and strings too: more is safe).
fn words_of(text: &str) -> HashSet<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.starts_with(|c: char| c.is_alphabetic() || c == '_'))
        .collect()
}

/// A digest of the project's declarations named `name`: which files declare it, in which
/// container (not where in the file, so an edit above them keeps it).
fn digest(index: &crate::index::Index, name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    index
        .named(name)
        .map(|(p, _, d)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (p, &d.container).hash(&mut h);
            h.finish()
        })
        .fold(0, u64::wrapping_add)
}

#[allow(clippy::too_many_arguments)]
fn file_pass(
    s: &mut State,
    path: &Path,
    rev: u64,
    text: Arc<str>,
    mut open: HashMap<PathBuf, Arc<str>>,
    files: &[PathBuf],
    root: &Path,
    targets: &[((Option<String>, String), usize)],
    stale: &dyn Fn() -> bool,
) -> Option<FileXref> {
    open.insert(path.to_path_buf(), text.clone());
    let texts = Texts { open };
    let cache = Cache::default();
    let started = Instant::now();
    let hash = text_hash(&text);
    let valid = s
        .done
        .get(path)
        .filter(|k| k.hash == hash)
        .and_then(|k| still_valid(s, path, k, &texts));
    let (refs, usages) = match (valid, s.done.get_mut(path)) {
        (Some(generation), Some(k)) => {
            k.generation = generation;
            (k.refs.clone(), k.usages.clone())
        }
        _ => {
            let kept = names_and_usages(
                s, path, hash, &text, &texts, &cache, files, root, targets, stale,
            )?;
            let out = (kept.refs.clone(), kept.usages.clone());
            s.done.insert(path.to_path_buf(), kept);
            if s.done.len() > 24 {
                let drop: Vec<PathBuf> = s
                    .done
                    .keys()
                    .filter(|p| *p != path)
                    .take(4)
                    .cloned()
                    .collect();
                for p in drop {
                    s.done.remove(&p);
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
    files: &[PathBuf],
    root: &Path,
    targets: &[((Option<String>, String), usize)],
    stale: &dyn Fn() -> bool,
) -> Option<Kept> {
    let generation = s.index.read().generation;
    // what each name in the file means, a step at a time
    let mut refs = Vec::new();
    let mut from = 0;
    in_chunks(s, texts, cache, stale, |r| {
        let to = (from + STEP).min(text.len());
        refs.extend(r.refs_between(path, from, to));
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
    let mut usages: HashMap<(Option<String>, String), Vec<Hit>> = targets
        .iter()
        .map(|(k, _)| (k.clone(), Vec::new()))
        .collect();
    let mut deps: HashSet<PathBuf> = HashSet::new();
    let mut i = 0;
    in_chunks(s, texts, cache, stale, |r| {
        let Some(f) = files.get(i) else { return true };
        for (ti, at) in r.usages_in(&decls, f) {
            deps.insert(f.clone());
            if let Some(text) = texts.read(f) {
                let l = super::found_line(f, &text, at);
                let rel = f.strip_prefix(root).unwrap_or(f);
                usages.get_mut(&targets[ti].0).unwrap().push(Hit {
                    name: l.text,
                    detail: format!("{}:{}", rel.display(), l.line),
                    path: f.clone(),
                    offset: at,
                    score: 0,
                });
            }
        }
        i += 1;
        i == files.len()
    })?;
    for (_, _, r) in &refs {
        if let Res::Decls(v) = r {
            deps.extend(v.iter().map(|d| d.0.clone()));
        }
    }
    Some(Kept {
        hash,
        generation,
        deps,
        names,
        words,
        refs,
        usages,
    })
}

/// The index generation a kept result is good at: no file changed since that it read, that
/// now names one of its declarations, or that declares a name it spells anew. `None` when it
/// must be worked out again.
fn still_valid(s: &State, path: &Path, k: &Kept, texts: &Texts) -> Option<u64> {
    // under the lock only lookups; reading the changed files waits until it is let go
    let (now, changed) = {
        let index = s.index.read();
        let changed: Vec<Arc<Path>> = index
            .changed_since(k.generation)?
            .into_iter()
            .filter(|p| **p != *path)
            .collect();
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

/// The Hilt links of one file: the project's graph brought up to date (only changed files are
/// read again), then "injected in N places" on each binding, and on each injection site the
/// binding it gets. `None` when cancelled.
fn hilt_pass(
    s: &mut State,
    texts: &Texts,
    cache: &Cache,
    path: &Path,
    root: &Path,
    stale: &dyn Fn() -> bool,
) -> Option<Vec<HiltLink>> {
    let mut graph = std::mem::take(&mut s.graph);
    loop {
        if stale() {
            s.graph = graph;
            return None;
        }
        let t = Instant::now();
        let finished = s.with(texts, cache, |r| {
            graph.refresh(r, &|| t.elapsed() >= s.budget || stale())
        });
        std::thread::yield_now();
        if finished {
            break;
        }
    }
    // what the graph says, without the lock
    let mut wants = Vec::new();
    if let Some((_, part)) = graph.files.get(path) {
        for b in &part.providers {
            let sites: Vec<(DeclRef, usize)> = graph
                .injected_in(b)
                .iter()
                .map(|s| (s.owner.clone(), s.at))
                .collect();
            if !sites.is_empty() {
                wants.push(Want::Injected(b.decl.1, sites));
            }
        }
        for site in &part.sites {
            let providers: Vec<DeclRef> = graph
                .providers_of(site)
                .iter()
                .map(|b| b.decl.clone())
                .collect();
            if !providers.is_empty() {
                wants.push(Want::Site(site.owner.clone(), site.param, providers));
            }
        }
    }
    s.graph = graph;
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
                links.push(HiltLink {
                    at: *at,
                    injected: true,
                    title: format!("Injected in {n} place{}", if n == 1 { "" } else { "s" }),
                    hits: sites
                        .iter()
                        .map(|(owner, at)| {
                            let mut h = to_hit(r, owner, root);
                            h.offset = *at;
                            h
                        })
                        .collect(),
                });
            }
            Want::Site(owner, param, providers) => {
                let hits = providers.iter().map(|d| to_hit(r, d, root));
                let ty = param.and_then(|i| r.param_type(owner, i));
                match by_owner.iter_mut().find(|(o, _, _)| o == owner) {
                    Some((_, h, t)) => {
                        h.extend(hits);
                        t.extend(ty);
                    }
                    None => {
                        by_owner.push((owner.clone(), hits.collect(), ty.into_iter().collect()))
                    }
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

    fn pass(s: &mut State, path: &str, rev: u64, open: &HashMap<PathBuf, Arc<str>>) -> FileXref {
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
        let files = s.code_files(Path::new("/p"));
        file_pass(
            s,
            path,
            rev,
            text,
            open.clone(),
            &files,
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
        let mut s = State::new(shared.clone(), None);
        assert_eq!(
            meaning(&pass(&mut s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:2"
        );
        // a checkout in a terminal moved A; the project is opened again (a fresh index, the
        // tab's version count starts again)
        let (index, again) = build(&[(a, "package a\n\n// moved\n\nclass A\n"), (u, use_a)]);
        shared.set(index);
        shared.settle();
        open = again;
        assert_eq!(
            meaning(&pass(&mut s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:5"
        );
        // same version number, other text: worked out again
        open.insert(
            PathBuf::from(u),
            Arc::from("package a\n\n\n\nfun zz(x: A) = 1\n"),
        );
        assert_eq!(
            meaning(&pass(&mut s, u, 1, &open), &open, u, "A", 1),
            "/p/A.kt:5"
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
        let mut s = State::new(shared.clone(), None);
        let links = |x: &FileXref| -> Vec<String> {
            x.hilt
                .iter()
                .filter(|l| !l.injected)
                .map(|l| l.title.clone())
                .collect()
        };
        assert_eq!(links(&pass(&mut s, h, 1, &open)), ["Dependencies: String"]);
        change(&shared, &mut open, m, &module("b"));
        assert!(
            links(&pass(&mut s, h, 1, &open)).is_empty(),
            "@Named(\"b\") is not what it asks for"
        );
        change(&shared, &mut open, m, &module("a"));
        assert_eq!(links(&pass(&mut s, h, 1, &open)), ["Dependencies: String"]);
    }

    #[test]
    fn a_name_declared_later_is_found() {
        let u = "/p/U.kt";
        let (shared, mut open) = project(&[(u, "package a\nfun zz() = Foo()\n")]);
        let mut s = State::new(shared.clone(), None);
        assert_eq!(meaning(&pass(&mut s, u, 1, &open), &open, u, "Foo", 1), "-");
        // an unrelated change keeps the kept result
        change(&shared, &mut open, "/p/B.kt", "package a\nclass Bar\n");
        let kept = s.done[Path::new(u)].generation;
        pass(&mut s, u, 1, &open);
        assert!(
            s.done[Path::new(u)].generation > kept,
            "reused, now known good later"
        );
        change(&shared, &mut open, "/p/F.kt", "package a\nclass Foo\n");
        assert_eq!(
            meaning(&pass(&mut s, u, 1, &open), &open, u, "Foo", 1),
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
        let one = pass(&mut long, a, 1, &open);
        let mut short = State::new(shared, None);
        short.budget = Duration::ZERO;
        let other = pass(&mut short, a, 1, &open);
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
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_err(),
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
        gate_tx.send(()).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
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
        let mut state = State::new(index.clone(), None);
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
        let files = Arc::new(files);
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
                &mut state,
                path,
                1,
                text.clone(),
                HashMap::new(),
                &files,
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
                &mut state,
                path,
                1,
                text,
                HashMap::new(),
                &files,
                &root,
                &targets,
                &|| false,
            )
            .unwrap();
            eprintln!("  again: {:?}", t.elapsed());
        }
    }
}
