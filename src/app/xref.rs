//! The one background worker for code insight: when a file is opened (or edited), what each of
//! its names means, where each of its declarations is used and its Hilt links are worked out
//! there, in short chunks under the index's read lock so the editor can keep updating the index.
//! One thread, started once; a newer request of the same kind cancels the older one; finished
//! results are kept per file and reused while neither the file nor anything it depends on changed.

use super::FoundLine;
use crate::index::{Hit, Shared};
use crate::resolve::{Cache, DeclRef, HiltGraph, Res, Resolver, Texts};
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
}

struct Kept {
    rev: u64,
    generation: u64,
    deps: HashSet<PathBuf>,
    names: Vec<String>,
    result: FileXref,
}

impl State {
    /// Runs `f` with a resolver under a short read lock.
    fn with<R>(&self, texts: &Texts, cache: &Cache, f: impl FnOnce(&Resolver) -> R) -> R {
        // the editor's queued changes go in first, under a short write lock
        self.index.flush();
        let t = Instant::now();
        let index = self.index.read();
        let r = Resolver::with(&index, texts, cache);
        let out = f(&r);
        drop(index);
        LONGEST_LOCK.fetch_max(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        out
    }
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
                let mut state = State {
                    index,
                    graph: HiltGraph::default(),
                    done: HashMap::new(),
                    ctx,
                };
                while let Ok(first) = rx.recv() {
                    // only the newest task of each kind runs
                    let mut tasks = vec![first];
                    while let Ok(t) = rx.try_recv() {
                        tasks.retain(|(k, _, _)| *k != t.0);
                        tasks.push(t);
                    }
                    for (_, _, task) in tasks {
                        task(&mut state, &|| false);
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

    /// Takes a finished file result.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.file_rx else {
            return false;
        };
        let mut got = false;
        while let Ok(r) = rx.try_recv() {
            self.file = Some(r);
            got = true;
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
        files: Arc<Vec<PathBuf>>,
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
            if let Some(r) = file_pass(s, &path, rev, text, open, &files, &root, &targets, stale) {
                let _ = tx.send(r);
                if let Some(c) = &s.ctx {
                    c.request_repaint();
                }
            }
        });
    }

    /// Finds the uses of `target` in the project on the worker; `reply` gets them.
    pub fn find_usages(
        &mut self,
        kind: &'static str,
        target: DeclRef,
        open: HashMap<PathBuf, Arc<str>>,
        files: Arc<Vec<PathBuf>>,
        reply: impl FnOnce(Vec<FoundLine>) + Send + 'static,
    ) {
        self.submit(kind, move |s, stale| {
            let texts = Texts { open };
            let cache = Cache::default();
            let mut found = Vec::new();
            let mut i = 0;
            while i < files.len() && !stale() {
                let t = Instant::now();
                s.with(&texts, &cache, |r| {
                    while i < files.len() && t.elapsed() < CHUNK {
                        for (_, at) in r.usages_in(std::slice::from_ref(&target), &files[i]) {
                            if let Some(text) = texts.read(&files[i]) {
                                found.push(super::found_line(&files[i], &text, at));
                            }
                        }
                        i += 1;
                    }
                });
                std::thread::yield_now();
            }
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
    let generation = s.index.read().generation;
    if let Some(k) = s.done.get(path)
        && k.rev == rev
        && still_valid(s, k, &texts)
    {
        let mut r = k.result.clone();
        r.rev = rev;
        return Some(r);
    }
    let cache = Cache::default();
    let started = Instant::now();
    // what each name in the file means, a few kilobytes at a time
    let mut refs = Vec::new();
    let mut from = 0;
    while from < text.len() {
        if stale() {
            return None;
        }
        let to = (from + 4096).min(text.len());
        s.with(&texts, &cache, |r| {
            refs.extend(r.refs_between(path, from, to))
        });
        from = to;
        std::thread::yield_now();
    }
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
    while i < files.len() {
        if stale() {
            return None;
        }
        let t = Instant::now();
        s.with(&texts, &cache, |r| {
            while i < files.len() && t.elapsed() < CHUNK {
                let f = &files[i];
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
            }
        });
        std::thread::yield_now();
    }
    for (_, _, r) in &refs {
        if let Res::Decls(v) = r {
            deps.extend(v.iter().map(|d| d.0.clone()));
        }
    }
    // Hilt: the project's graph (only changed files are read again), then this file's links
    let mut graph = std::mem::take(&mut s.graph);
    loop {
        if stale() {
            s.graph = graph;
            return None;
        }
        let t = Instant::now();
        let finished = s.with(&texts, &cache, |r| {
            let budget = || t.elapsed() > CHUNK || stale();
            graph.refresh(r, &budget) && !budget()
        });
        std::thread::yield_now();
        if finished {
            break;
        }
    }
    let hilt = s.with(&texts, &cache, |r| hilt_links(r, &graph, path, root));
    s.graph = graph;
    let result = FileXref {
        path: path.to_path_buf(),
        rev,
        refs,
        usages,
        hilt,
    };
    if std::env::var_os("DROID_LITE_XREF_LOG").is_some() {
        eprintln!("xref {} in {:?}", path.display(), started.elapsed());
    }
    s.done.insert(
        path.to_path_buf(),
        Kept {
            rev,
            generation,
            deps,
            names,
            result: result.clone(),
        },
    );
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
    Some(result)
}

/// A kept result is good while no file changed since that it read, or that now names one of
/// its declarations.
fn still_valid(s: &State, k: &Kept, texts: &Texts) -> bool {
    let index = s.index.read();
    let Some(changed) = index.changed_since(k.generation) else {
        return false;
    };
    changed.iter().all(|p| {
        !k.deps.contains(&**p)
            && texts
                .read(p)
                .is_none_or(|t| !k.names.iter().any(|n| t.contains(n.as_str())))
    })
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

/// The Hilt links of one file: "injected in N places" on each binding, and on each injection
/// site the binding it gets.
fn hilt_links(r: &Resolver, g: &HiltGraph, path: &Path, root: &Path) -> Vec<HiltLink> {
    let mut out = Vec::new();
    let Some((_, part)) = g.files.get(path) else {
        return out;
    };
    for b in &part.providers {
        let sites = g.injected_in(b);
        let n = sites.len();
        if n == 0 {
            continue;
        }
        out.push(HiltLink {
            at: b.decl.1,
            injected: true,
            title: format!("Injected in {n} place{}", if n == 1 { "" } else { "s" }),
            hits: sites
                .iter()
                .map(|s| {
                    let mut h = to_hit(r, &s.owner, root);
                    h.offset = s.at;
                    h
                })
                .collect(),
        });
    }
    // one link per declaration: a constructor's sites together, each to its providers
    let mut by_owner: Vec<(DeclRef, Vec<Hit>, Vec<String>)> = Vec::new();
    for site in &part.sites {
        let providers = g.providers_of(site);
        if providers.is_empty() {
            continue;
        }
        let hits: Vec<Hit> = providers.iter().map(|b| to_hit(r, &b.decl, root)).collect();
        let ty = site.param.and_then(|i| r.param_type(&site.owner, i));
        match by_owner.iter_mut().find(|(o, _, _)| *o == site.owner) {
            Some((_, h, t)) => {
                h.extend(hits);
                t.extend(ty);
            }
            None => by_owner.push((site.owner.clone(), hits, ty.into_iter().collect())),
        }
    }
    for (owner, mut hits, types) in by_owner {
        hits.dedup_by(|a, b| a.path == b.path && a.offset == b.offset);
        let title = match (&hits[..], types.is_empty()) {
            // a constructor or `@Provides` function: what it needs
            (_, false) => format!("Dependencies: {}", types.join(", ")),
            ([one], true) => format!("Provided by {}", one.name),
            (_, true) => format!("{} providers", hits.len()),
        };
        out.push(HiltLink {
            at: owner.1,
            injected: false,
            title,
            hits,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut state = State {
            index: index.clone(),
            graph: HiltGraph::default(),
            done: HashMap::new(),
            ctx: None,
        };
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
