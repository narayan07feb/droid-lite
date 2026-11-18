//! Go to declaration on a library or Android SDK call, like Android Studio: the exact declaration
//! (the overload whose parameter types take the arguments best, weighed across the class and,
//! breadth first, all its supertypes; an extension function by its receiver) in the source, opened
//! read-only. A missing source is downloaded first, on a worker thread with a status line, and
//! cached; when there is no source at all, a stub made from the class file is shown instead. The
//! whole lookup (jars, sources, the supertype walk, the stub) runs on a worker: the editor never
//! waits for it.

use super::{App, Tab};
use crate::index::Hit;
use crate::libs::Libraries;
use crate::libsrc::{self, Fetch, Members};
use crate::resolve::LibCall;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// A failed download is tried again after this long (a network blip is not for the session).
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// How many supertypes up, and how many classes in all, a member is looked for.
const MAX_DEPTH: usize = 8;
const MAX_CLASSES: usize = 40;
/// How many library functions of the name are read to match an extension's receiver.
const MAX_EXTENSION_FILES: usize = 30;

/// A walk step: candidate classes (name, nested guess) and the depth.
type Step = (Vec<(String, bool)>, usize);
/// The project's compile SDK by project root, shared with the lookup workers.
type ApiCache = Arc<std::sync::Mutex<Option<(PathBuf, Option<u32>)>>>;

/// Downloads of sources, and lookups on a worker.
#[derive(Default)]
pub(super) struct Fetches {
    running: Vec<(String, Receiver<Result<(), String>>)>,
    /// How each download ended, and when.
    pub ended: HashMap<String, (Result<(), String>, Instant)>,
    /// The download key and the jump to make once it ends.
    waiting: Option<(String, Jump)>,
    /// A jump's lookup on a worker.
    looking: Option<Receiver<Looked>>,
    /// The file the waiting or looking jump was made from: another active tab drops it.
    from: Option<PathBuf>,
    /// Find usages' lookup: only fetches a missing source.
    prefetch: Option<Receiver<Looked>>,
    /// The library call whose Find usages were approximate (a source was missing): searched
    /// again when a download ends.
    pub usages_again: Option<(LibCall, String)>,
    /// Find usages' searches: a newer one makes an older one stop.
    pub usages_gen: Arc<AtomicU64>,
    /// The project's compile SDK, read once per project root (on a worker).
    api: ApiCache,
    /// How many downloads ran (tests check a cached source is not downloaded again).
    pub started: usize,
}

impl Fetches {
    /// Drops a jump still waiting for a download or a lookup (the user went elsewhere).
    pub(super) fn cancel(&mut self) {
        self.waiting = None;
        self.looking = None;
        self.from = None;
    }
}

/// What a jump needs to look again after a download: the call and the file's imports.
#[derive(Clone, Debug)]
struct Jump {
    call: LibCall,
    imports: Vec<String>,
}

/// A class file inside a jar or `.aar`: for a stub when no source can be had.
#[derive(Clone, Debug)]
struct ClassFile {
    archive: PathBuf,
    entry: String,
    qualified: String,
}

/// What a lookup found.
enum Looked {
    /// The declarations that fit (one opens; several go to the picker): file, offset, line.
    Found(Vec<(PathBuf, usize, String)>),
    /// The source this needs is missing: this download brings it.
    Fetch(Fetch, Jump),
    /// No source can be had: a stub (its path, text and the member's offset) and why.
    Stub(PathBuf, String, Option<usize>, String),
    /// Not found: why, and the call (one with a fallback is looked up by name).
    NotFound(String, Option<LibCall>),
    /// Find usages: nothing to fetch.
    Nothing,
}

/// Where a class's source is.
enum Source {
    Text(PathBuf),
    Missing(Fetch, Option<ClassFile>),
}

/// What every lookup reads besides the call (shared by Find usages' checks).
#[derive(Clone)]
pub(super) struct LookupBase {
    root: PathBuf,
    sdk: Option<PathBuf>,
    api: Option<u32>,
    /// The project's compile SDK, shared: the first lookup of a project reads it from `scripts`.
    api_cache: ApiCache,
    scripts: Vec<PathBuf>,
    libs: Arc<Libraries>,
    /// Downloads that may not run (again): done but the class was not in them (None), or failed
    /// lately (the error), for the stub's note.
    blocked: HashMap<String, Option<String>>,
}

/// One lookup, sent to the worker.
struct Lookup {
    jump: Jump,
    /// Find usages: only find what to fetch, open nothing.
    fetch_only: bool,
    base: LookupBase,
}

impl App {
    /// Go to declaration on a library member call ([`crate::resolve::Res::Lib`]).
    pub(super) fn goto_lib_call(&mut self, call: LibCall) {
        let jump = Jump {
            call,
            imports: self.active_imports(),
        };
        self.fetches.from = self.tabs.get(self.active).map(|t| t.path.clone());
        self.look(jump, false);
    }

    /// The source of a library member call fetched when it is missing, without opening it (Find
    /// usages, so the declaration is there when wanted).
    pub(super) fn fetch_lib_source(&mut self, call: LibCall) {
        let jump = Jump {
            call,
            imports: self.active_imports(),
        };
        self.look(jump, true);
    }

    /// A jump's lookup is running (tests wait for it).
    #[cfg(test)]
    pub(super) fn lib_reading(&self) -> bool {
        self.fetches.looking.is_some()
    }

    /// The user went elsewhere (a new jump, a file, a tab): a jump still waiting for a download or
    /// a lookup is dropped.
    pub(super) fn cancel_lib_jump(&mut self) {
        self.fetches.cancel();
    }

    /// A project opened: nothing of the last one's lookups is kept but the downloads' results.
    /// The download a Find usages waits for ended (`key`): it is searched again, now exactly.
    pub(super) fn usages_after_download(&mut self, key: &str) {
        match self.fetches.usages_again.take() {
            Some((call, k)) if k == key => {
                if let Some(ctx) = self.ctx.clone() {
                    self.search_lib_usages(call, &ctx);
                }
            }
            other => self.fetches.usages_again = other,
        }
    }

    pub(super) fn reset_lib_lookups(&mut self) {
        self.cancel_lib_jump();
        self.fetches.prefetch = None;
        self.fetches.usages_again = None;
        self.fetches.api = Default::default();
        // another project: its sources folders may differ, so each download may run again
        self.fetches.ended.clear();
    }

    /// The imports of the open file (where a class known only by its simple name may be nested).
    fn active_imports(&self) -> Vec<String> {
        self.tabs
            .get(self.active)
            .map(|t| imports_of(&t.text))
            .unwrap_or_default()
    }

    /// What a lookup reads besides the call; None without a project.
    pub(super) fn lookup_base(&mut self) -> Option<LookupBase> {
        let p = self.project.as_ref()?;
        let root = p.root.clone();
        let sdk = libsrc::sdk_dir();
        // the compile SDK is read on the worker the first time (build scripts can be many)
        let api = self
            .fetches
            .api
            .lock()
            .ok()
            .and_then(|a| a.as_ref().filter(|(r, _)| *r == root).map(|(_, api)| *api))
            .flatten();
        let scripts: Vec<PathBuf> = p
            .files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
            .cloned()
            .collect();
        let blocked = self
            .fetches
            .ended
            .iter()
            .filter_map(|(k, (r, at))| match r {
                Ok(()) => Some((k.clone(), None)),
                Err(e) if at.elapsed() < RETRY_AFTER => Some((k.clone(), Some(e.clone()))),
                Err(_) => None,
            })
            .collect();
        Some(LookupBase {
            root,
            sdk,
            api,
            api_cache: self.fetches.api.clone(),
            scripts,
            libs: self.libs.clone(),
            blocked,
        })
    }

    /// Starts a lookup on a worker ([`App::poll_fetches`] takes its result).
    fn look(&mut self, jump: Jump, fetch_only: bool) {
        let Some(base) = self.lookup_base() else {
            if !fetch_only {
                self.status = "Open a project to go to library declarations".into();
            }
            return;
        };
        let lookup = Lookup {
            jump,
            fetch_only,
            base,
        };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(lookup.run());
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        if fetch_only {
            self.fetches.prefetch = Some(rx);
        } else {
            self.fetches.looking = Some(rx);
        }
    }

    /// Downloads a missing source on a worker (once per key at a time); the jump goes on when it
    /// ends ([`App::poll_fetches`]).
    fn start_fetch(&mut self, fetch: Fetch, jump: Jump, open: bool) {
        let key = fetch.key();
        let label = fetch.label();
        // it ended while the lookup ran (another jump started it): look again, which now sees how
        // it ended (the source, or the stub), never a second download
        if let Some((r, at)) = self.fetches.ended.get(&key)
            && (r.is_ok() || at.elapsed() < RETRY_AFTER)
        {
            if open {
                self.look(jump, false);
            }
            return;
        }
        if open {
            self.fetches.waiting = Some((key.clone(), jump));
        }
        self.status = if open {
            format!("Downloading {label}… (the editor keeps working)")
        } else {
            format!("Downloading {label} for the declaration… (the editor keeps working)")
        };
        if self.fetches.running.iter().any(|(k, _)| *k == key) {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(libsrc::fetch(&fetch));
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        self.fetches.running.push((key, rx));
        self.fetches.started += 1;
    }

    /// A finished download looks again for the jump that waited for it (then the source is there,
    /// or the stub is shown) and runs approximate Find usages again; a finished lookup opens what
    /// it found. A jump made from a file no longer active is dropped.
    pub(super) fn poll_fetches(&mut self) {
        let active = self.tabs.get(self.active).map(|t| t.path.clone());
        if self.fetches.from.is_some() && self.fetches.from != active {
            self.fetches.cancel(); // another tab: by a click, a key, a session restore…
        }
        let mut done = Vec::new();
        self.fetches.running.retain(|(k, rx)| match rx.try_recv() {
            Ok(r) => {
                done.push((k.clone(), r));
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                done.push((k.clone(), Err("the download stopped".into())));
                false
            }
        });
        for (key, r) in done {
            self.status = match &r {
                Ok(()) => format!("Downloaded {key} sources"),
                Err(e) => format!("Download failed: {e}"),
            };
            self.fetches.ended.insert(key.clone(), (r, Instant::now()));
            match self.fetches.waiting.take() {
                Some((wk, jump)) if wk == key => self.look(jump, false),
                other => self.fetches.waiting = other,
            }
            self.usages_after_download(&key);
        }
        if let Some(looked) = take(&mut self.fetches.prefetch, &mut self.status)
            && let Looked::Fetch(fetch, jump) = looked
        {
            self.start_fetch(fetch, jump, false);
        }
        match take(&mut self.fetches.looking, &mut self.status) {
            Some(Looked::Fetch(fetch, jump)) => self.start_fetch(fetch, jump, true),
            Some(Looked::Found(hits)) => self.open_found(hits),
            Some(Looked::Stub(path, text, at, why)) => self.open_stub(path, text, at, why),
            Some(Looked::NotFound(why, call)) => match call.filter(|c| c.fallback) {
                // not a member of the project class's library supertype: by name, as before
                Some(c) => {
                    self.status = why;
                    self.goto_named(c.member);
                }
                None => self.status = why,
            },
            Some(Looked::Nothing) | None => {}
        }
    }

    /// One fitting declaration opens; several that fit as well go to the picker.
    fn open_found(&mut self, hits: Vec<(PathBuf, usize, String)>) {
        self.fetches.from = None;
        if let [(path, at, _)] = hits.as_slice() {
            self.open_file(&path.clone(), Some(*at));
            return;
        }
        let n = hits.len();
        let hits = hits
            .into_iter()
            .map(|(path, offset, detail)| Hit {
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                detail,
                path,
                offset,
                score: 0,
            })
            .collect();
        self.pick(format!("{n} declarations fit"), hits);
    }

    /// The stub of a class without source, read-only, at the member when it can tell.
    fn open_stub(&mut self, path: PathBuf, text: String, at: Option<usize>, why: String) {
        self.fetches.from = None;
        if let Some(t) = self.tabs.get(self.active) {
            // Back returns to the call, as after any jump
            super::push_capped(&mut self.history, (t.path.clone(), t.cursor));
            self.forward.clear();
        }
        let i = match self.tabs.iter().position(|t| t.path == path) {
            Some(i) => i,
            None => {
                let mut tab = Tab::open(&path, text);
                tab.readonly = true;
                tab.reanalyse();
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        self.active = i;
        self.tabs[i].goto = at;
        self.status = why;
    }
}

/// A call as Find usages tells calls apart: its file (only when the class is not qualified, as
/// the file's imports then decide), receiver class (simple, qualified) and argument types.
type CallKey = (Option<PathBuf>, String, Option<String>, String);
/// A declaration: its file and offset.
type Decl = (PathBuf, usize);

/// The class file declares `member`: a method of that name, its getter or setter (a Kotlin
/// property), or a field.
fn declares(info: &crate::classfile::ClassInfo, member: &str) -> bool {
    let cap: String = member
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .chain(member.chars().skip(1))
        .collect();
    let names = [
        member.to_string(),
        format!("get{cap}"),
        format!("is{cap}"),
        format!("set{cap}"),
    ];
    info.methods.iter().any(|(n, _, _)| names.contains(n))
        || info.fields.iter().any(|f| f == member)
}

/// How a library Find usages came out.
pub(super) enum UsagesState {
    /// Each call checked against the declaration.
    Exact,
    /// The declaration's source is downloading (this key): calls on the same class for now.
    Downloading(String),
    /// The declaration has no source (a stub, not found, or several fit): calls on the same class.
    NoSource,
}

/// Find usages of a library call, exactly: the candidate calls (same member, as many arguments,
/// argument types that can match) that lead to the same declaration as `target`. When the target's
/// declaration cannot be read yet (its source is missing), the candidates on the same class are
/// kept and the result says it is approximate (true). `stale()`: a newer search started.
pub(super) fn exact_usages(
    base: LookupBase,
    target: LibCall,
    target_imports: Vec<String>,
    cands: Vec<(super::FoundLine, LibCall)>,
    stale: impl Fn() -> bool,
) -> Option<(Vec<super::FoundLine>, UsagesState)> {
    let decl = |call: LibCall, imports: Vec<String>| -> Option<Decl> {
        let l = Lookup {
            jump: Jump { call, imports },
            fetch_only: false,
            base: base.clone(),
        };
        match l.run() {
            Looked::Found(h) if h.len() == 1 => Some((h[0].0.clone(), h[0].1)),
            _ => None,
        }
    };
    let target_look = Lookup {
        jump: Jump {
            call: target.clone(),
            imports: target_imports,
        },
        fetch_only: false,
        base: base.clone(),
    }
    .run();
    let (want, state) = match target_look {
        Looked::Found(h) if h.len() == 1 => (Some((h[0].0.clone(), h[0].1)), UsagesState::Exact),
        Looked::Fetch(f, _) => (None, UsagesState::Downloading(f.key())),
        _ => (None, UsagesState::NoSource),
    };
    let mut known: HashMap<CallKey, Option<Decl>> = HashMap::new();
    let mut imports: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut out = Vec::new();
    for (line, c) in cands {
        if stale() {
            return None;
        }
        let keep = match &want {
            None => super::xref::same_lib_call(&c, &target), // nothing to compare with yet
            Some(w) => {
                let key = (
                    c.qualified.is_none().then(|| line.path.clone()),
                    c.class.clone(),
                    c.qualified.clone(),
                    format!("{:?}", c.args),
                );
                let got = match known.get(&key) {
                    Some(g) => g.clone(),
                    None => {
                        let im = imports
                            .entry(line.path.clone())
                            .or_insert_with(|| {
                                std::fs::read_to_string(&line.path)
                                    .map(|t| imports_of(&t))
                                    .unwrap_or_default()
                            })
                            .clone();
                        let g = decl(c.clone(), im);
                        known.insert(key, g.clone());
                        g
                    }
                };
                match got {
                    Some(g) => g == *w,
                    None => super::xref::same_lib_call(&c, &target),
                }
            }
        };
        if keep {
            out.push(line);
        }
    }
    Some((out, state))
}

/// A finished lookup from `slot`; a lookup that died (a bug in it) clears the slot and says so.
fn take(slot: &mut Option<Receiver<Looked>>, status: &mut String) -> Option<Looked> {
    let rx = slot.as_ref()?;
    match rx.try_recv() {
        Ok(l) => {
            *slot = None;
            Some(l)
        }
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            *slot = None;
            *status = "The library lookup stopped unexpectedly".into();
            None
        }
    }
}

/// `import a.b.C` lines of a file (not `*` or aliases).
fn imports_of(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("import "))
        .map(|l| l.trim_end_matches(';').trim().to_string())
        .filter(|l| !l.ends_with('*') && !l.contains(" as "))
        .collect()
}

/// The class part of a qualified name, nested classes kept (`a.b.Outer.Inner` gives
/// `Outer.Inner`): what [`libsrc::find_member`] looks in.
fn class_path(q: &str) -> String {
    let parts: Vec<&str> = q.split('.').collect();
    let k = parts
        .iter()
        .position(|s| s.starts_with(char::is_uppercase))
        .unwrap_or(parts.len().saturating_sub(1));
    parts[k..].join(".")
}

/// One class's declarations of the member, scored for the walk.
struct Level {
    depth: usize,
    score: usize,
    hits: Vec<(PathBuf, usize, String)>,
}

impl Lookup {
    /// On a worker: the declarations the call means, weighed across the class and its supertypes
    /// (breadth first: the best fit wins, the nearest of equal ones), else its extension functions;
    /// else the download that would bring a missing source, or the stub. Find usages
    /// (`fetch_only`) walks the same way and only answers what to fetch.
    fn run(mut self) -> Looked {
        if self.base.api.is_none()
            && let Some(sdk) = &self.base.sdk
        {
            let known = self.base.api_cache.lock().ok().and_then(|a| {
                a.as_ref()
                    .filter(|(r, _)| *r == self.base.root)
                    .map(|(_, api)| *api)
            });
            self.base.api = match known {
                Some(api) => api,
                None => {
                    let api = libsrc::compile_sdk(&self.base.root, &self.base.scripts, sdk);
                    if let Ok(mut a) = self.base.api_cache.lock() {
                        *a = Some((self.base.root.clone(), api));
                    }
                    api
                }
            };
        }
        if self.jump.call.builtin {
            // a Kotlin type: a standard library extension (its members have no source to read)
            if self.fetch_only {
                return Looked::Nothing;
            }
            let mut receivers: Vec<String> = vec![self.jump.call.class.clone()];
            receivers.extend(
                libsrc::kotlin_supers(&self.jump.call.class)
                    .iter()
                    .map(|s| s.to_string()),
            );
            return self.extensions(&receivers, None);
        }
        self.widen_args();
        let call = self.jump.call.clone();
        // the receiver's class: an import names it for sure; else each candidate is a class to
        // look in (with whether it is a nested-class guess, kept only when its class file is there)
        let starts: Vec<Vec<(String, bool)>> = match &call.qualified {
            Some(q) => vec![vec![(q.clone(), false)]],
            None => self.candidates().into_iter().map(|c| vec![c]).collect(),
        };
        let mut queue: VecDeque<Step> = starts.into_iter().map(|c| (c, 0)).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seen_names: Vec<String> = Vec::new();
        let mut levels: Vec<Level> = Vec::new();
        let mut loose: Vec<(PathBuf, usize, String)> = Vec::new();
        let mut missing: Option<(Fetch, Option<ClassFile>, String, usize)> = None;
        while let Some((cands, depth)) = queue.pop_front() {
            if seen.len() >= MAX_CLASSES {
                break;
            }
            let Some((q, source)) = cands
                .iter()
                .find_map(|(q, g)| Some((q.clone(), self.locate(q, *g)?)))
            else {
                continue;
            };
            if !seen.insert(q.clone()) {
                continue; // a cycle, or a class reached twice
            }
            let class = class_path(&q);
            let simple = q.rsplit('.').next().unwrap_or(&q).to_string();
            seen_names.push(simple.clone());
            let path = match source {
                Source::Text(p) => p,
                Source::Missing(fetch, stub) => {
                    // its class file says whether it declares the member and what its
                    // supertypes are: only a class that declares it is worth a download (a
                    // guessed class is here only when its class file is)
                    let info = stub
                        .as_ref()
                        .and_then(|s| libsrc::class_bytes(&s.archive, &s.entry))
                        .and_then(|b| crate::classfile::parse(&b));
                    let declares = match &info {
                        Some(i) => declares(i, &call.member),
                        // no class file to tell: a sure receiver may still declare it; a name
                        // that may be the project's or the standard library's is not fetched
                        None => !call.fallback,
                    };
                    if missing.is_none() && declares {
                        missing = Some((fetch, stub, simple, depth));
                    }
                    if let Some(i) = info
                        && depth < MAX_DEPTH
                    {
                        for s in i.supers {
                            queue.push_back((vec![(s, false)], depth + 1));
                        }
                    }
                    continue;
                }
            };
            let Some(text) = read_source(&path) else {
                continue;
            };
            let mut c = call.clone();
            c.class = simple.clone();
            let Members {
                offsets,
                exact: sure,
                score,
            } = libsrc::find_member(&path, &text, &class, &c);
            let hits: Vec<(PathBuf, usize, String)> = offsets
                .into_iter()
                .map(|o| (path.clone(), o, line_at(&text, o)))
                .collect();
            if sure && !hits.is_empty() {
                levels.push(Level { depth, score, hits });
            } else if loose.is_empty() {
                loose = hits; // used only when no level fits exactly
            }
            // supertypes may declare a better overload (javac weighs them all): walk on
            if depth < MAX_DEPTH {
                for sup in libsrc::supertypes(&text, &simple) {
                    let sup: Vec<(String, bool)> = sup
                        .into_iter()
                        .filter(|s| s.rsplit('.').next() != Some("Object"))
                        .map(|s| (s, false))
                        .collect();
                    if !sup.is_empty() {
                        queue.push_back((sup, depth + 1));
                    }
                }
            }
        }
        // the best fit, the nearest of equal ones (an override is nearer than what it overrides)
        let best = levels
            .iter()
            .map(|l| (l.score, std::cmp::Reverse(l.depth)))
            .max();
        let exact: Vec<(PathBuf, usize, String)> = match best {
            Some((s, std::cmp::Reverse(d))) => levels
                .into_iter()
                .filter(|l| l.score == s && l.depth == d)
                .flat_map(|l| l.hits)
                .collect(),
            None => Vec::new(),
        };
        let exact_depth = best.map_or(usize::MAX, |(_, std::cmp::Reverse(d))| d);
        // a class without source as near as the match (or nearer) may declare it, and a farther
        // one may fit better when the match is not a perfect fit: fetch it first
        let known_args = call
            .args
            .as_ref()
            .map_or(0, |a| a.iter().filter(|x| !x.is_empty()).count());
        let perfect = best.is_some_and(|(s, _)| s >= 100 * known_args);
        if let Some((fetch, stub, class, depth)) = missing
            && (exact.is_empty() || depth <= exact_depth || !perfect)
        {
            let state = self.base.blocked.get(&fetch.key()).cloned().or_else(|| {
                // its result is on disk already (an earlier session): running it again brings
                // nothing; its class is just not in it
                libsrc::already_there(&fetch).then_some(None)
            });
            match state {
                None => return Looked::Fetch(fetch, self.jump.clone()),
                Some(_) if !exact.is_empty() => {}
                Some(_) if self.fetch_only => return Looked::Nothing,
                Some(_) if !loose.is_empty() => return Looked::Found(loose),
                Some(None) => {
                    let why = format!("{} has no source of {class}", fetch.label());
                    return self.stub(stub, &why, &class);
                }
                Some(Some(e)) => {
                    let why = format!("{} could not be downloaded ({e})", fetch.label());
                    return self.stub(stub, &why, &class);
                }
            }
        }
        if !exact.is_empty() {
            return if self.fetch_only {
                Looked::Nothing
            } else {
                Looked::Found(exact)
            };
        }
        // not a member: an extension function of the class, a supertype or a Kotlin twin
        let mut receivers = vec![call.class.clone()];
        receivers.extend(seen_names.iter().cloned());
        receivers.extend(
            libsrc::kotlin_supers(&call.class)
                .iter()
                .map(|s| s.to_string()),
        );
        let why = if seen.is_empty() {
            format!(
                "`{}.{}`: no source of {} in the Android SDK or the project's libraries",
                call.class, call.member, call.class
            )
        } else {
            format!(
                "`{}` is not a member of {} or its supertypes, nor a library extension of them",
                call.member, call.class
            )
        };
        match self.extensions(&receivers, Some(&why)) {
            Looked::NotFound(..) if !loose.is_empty() && !self.fetch_only => Looked::Found(loose),
            Looked::Fetch(f, j) => Looked::Fetch(f, j),
            _ if self.fetch_only => Looked::Nothing,
            other => other,
        }
    }

    /// The library's extension functions `fun Receiver.member` for one of `receivers`, read from
    /// the sources of the library functions of that name (the ones the file imports first);
    /// ranked by the arguments. A function whose library's sources are missing is downloaded.
    fn extensions(&self, receivers: &[String], why: Option<&str>) -> Looked {
        let call = &self.jump.call;
        let mut found = Vec::new();
        let mut binary: Vec<String> = Vec::new();
        let mut fetch: Option<Fetch> = None;
        // the functions the file imports by name first, then the others
        let mut syms: Vec<&crate::libs::LibSym> = self
            .base
            .libs
            .named(&call.member)
            .filter(|s| !s.kind.is_type())
            .collect();
        syms.sort_by_key(|s| !self.jump.imports.contains(&s.qualified()));
        for s in syms.into_iter().take(MAX_EXTENSION_FILES) {
            // its source: in the Gradle cache's sources jar, else in a downloaded one (a
            // function's file is found by what it declares: `Ext.kt` holds `ExtKt`'s functions)
            let paths: Vec<PathBuf> = match s.source_path() {
                Some(p) => vec![p],
                None => libsrc::cached_sources_jar(&s.artifact)
                    .filter(|j| j.is_file())
                    .map(|jar| {
                        libsrc::fun_entries(&jar, &s.package, &call.member)
                            .into_iter()
                            .map(|e| PathBuf::from(format!("{}!/{e}", jar.display())))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            if paths.is_empty() {
                let f = Fetch::Maven {
                    coords: s.artifact.to_string(),
                    repos: libsrc::repos(&self.base.root),
                };
                if fetch.is_none()
                    && !self.base.blocked.contains_key(&f.key())
                    && !libsrc::already_there(&f)
                {
                    fetch = Some(f);
                }
                binary.push(format!("{} ({})", s.qualified(), s.artifact));
                continue;
            }
            for path in paths {
                if found.iter().any(|(p, _, _)| *p == path) {
                    continue; // one file holds several functions of the name
                }
                let Some(text) = read_source(&path) else {
                    continue;
                };
                let m = libsrc::find_extension(&path, &text, receivers, call);
                found.extend(
                    m.offsets
                        .into_iter()
                        .map(|o| (path.clone(), o, line_at(&text, o))),
                );
            }
        }
        if !found.is_empty() {
            return Looked::Found(found);
        }
        if let Some(f) = fetch {
            return Looked::Fetch(f, self.jump.clone());
        }
        let mut msg = why.map(String::from).unwrap_or_else(|| {
            format!(
                "`{}` on Kotlin's {}: a standard library function whose source is not here",
                call.member, call.class
            )
        });
        if !binary.is_empty() {
            msg.push_str(&format!(
                "; library functions of that name without source: {}",
                binary.join(", ")
            ));
        }
        Looked::NotFound(msg, Some(self.jump.call.clone()))
    }

    /// A library argument (known by its name only) gets its supertypes from its source, so a
    /// library class passed for a `Context` parameter is taken by it.
    fn widen_args(&mut self) {
        let Some(args) = self.jump.call.args.clone() else {
            return;
        };
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let simple_lib = a.len() == 1
                && a[0].starts_with(char::is_uppercase)
                && libsrc::kotlin_supers(&a[0]).is_empty()
                && !matches!(
                    a[0].as_str(),
                    "String" | "Boolean" | "Char" | "Unit" | "Any"
                );
            if !simple_lib {
                out.push(a);
                continue;
            }
            let mut names = a.clone();
            let mut queue: VecDeque<(Vec<String>, usize)> = VecDeque::new();
            let first: Vec<String> = self
                .jump
                .imports
                .iter()
                .filter(|i| i.rsplit('.').next() == Some(a[0].as_str()))
                .cloned()
                .chain([format!("java.lang.{}", a[0])])
                .collect();
            queue.push_back((first, 0));
            let mut seen = HashSet::new();
            while let Some((cands, depth)) = queue.pop_front() {
                if depth > 4 || seen.len() > 12 {
                    break;
                }
                let Some((q, Source::Text(path))) = cands
                    .iter()
                    .find_map(|q| Some((q.clone(), self.locate(q, false)?)))
                else {
                    continue;
                };
                if !seen.insert(q.clone()) {
                    continue;
                }
                let class = q.rsplit('.').next().unwrap_or(&q).to_string();
                let Some(text) = read_source(&path) else {
                    continue;
                };
                for sup in libsrc::supertypes(&text, &class) {
                    if let Some(n) = sup.first().and_then(|s| s.rsplit('.').next())
                        && !names.iter().any(|x| x == n)
                    {
                        names.push(n.to_string());
                    }
                    queue.push_back((sup, depth + 1));
                }
            }
            out.push(names);
        }
        self.jump.call.args = Some(out);
    }

    /// The classes a receiver known only by its simple name may be, with whether each is a guess
    /// that must be checked: nested in a class the file imports (`Locator.Spot` read as `Spot`),
    /// the `*` imports, `java.lang`, then library classes of that name.
    fn candidates(&self) -> Vec<(String, bool)> {
        let call = &self.jump.call;
        let mut out: Vec<(String, bool)> = self
            .jump
            .imports
            .iter()
            .filter(|i| {
                i.rsplit('.')
                    .next()
                    .is_some_and(|s| s.starts_with(char::is_uppercase))
            })
            .map(|i| (format!("{i}.{}", call.class), true))
            .collect();
        out.extend(
            call.stars
                .iter()
                .map(|s| (format!("{s}.{}", call.class), false)),
        );
        out.push((format!("java.lang.{}", call.class), false));
        out.extend(
            self.base
                .libs
                .named(&call.class)
                .filter(|s| s.kind.is_type())
                .map(|s| (s.qualified(), false)),
        );
        let mut seen = HashSet::new();
        out.retain(|(q, _)| seen.insert(q.clone()));
        out
    }

    /// Where the source of class `q` is, or the download it needs; None when `q` is no such class
    /// (a guess whose class file is not there, a name no SDK or library has).
    fn locate(&self, q: &str, guess: bool) -> Option<Source> {
        if libsrc::is_platform(q)
            && let (Some(sdk), Some(api)) = (self.base.sdk.clone(), self.base.api)
        {
            let jar = sdk.join(format!("platforms/android-{api}/android.jar"));
            let entry = libsrc::class_entry(q);
            // without the platform installed, the package says it is an SDK class
            let in_jar = || !jar.is_file() || libsrc::has_class(&jar, &entry);
            if !(guess && !in_jar()) {
                if let Some(path) = libsrc::sdk_source(&sdk, api, q) {
                    return Some(Source::Text(path));
                }
                if in_jar() {
                    let stub = jar.is_file().then(|| ClassFile {
                        archive: jar,
                        entry,
                        qualified: q.to_string(),
                    });
                    return Some(Source::Missing(Fetch::Sdk { sdk, api }, stub));
                }
            }
            // not an SDK class (javax.inject, android.support…): maybe a library one
        }
        // the class, or for a nested class its outer class (whose file holds it)
        let (s, outer) = outers(q).into_iter().find_map(|o| {
            let simple = o.rsplit('.').next().unwrap_or(&o).to_string();
            self.base
                .libs
                .named(&simple)
                .find(|s| s.kind.is_type() && s.qualified() == o)
                .cloned()
                .map(|s| (s, o))
        })?;
        let entry = libsrc::class_entry(q);
        if guess && !libsrc::has_class(&s.archive, &entry) {
            return None;
        }
        if let Some(src) = s.source_path() {
            return Some(Source::Text(src));
        }
        if let Some(cached) = libsrc::cached_sources_jar(&s.artifact).filter(|j| j.is_file())
            && let Some(e) = libsrc::jar_entry(&cached, &outer)
        {
            return Some(Source::Text(PathBuf::from(format!(
                "{}!/{e}",
                cached.display()
            ))));
        }
        let stub = ClassFile {
            archive: s.archive.to_path_buf(),
            entry,
            qualified: q.to_string(),
        };
        let fetch = Fetch::Maven {
            coords: s.artifact.to_string(),
            repos: libsrc::repos(&self.base.root),
        };
        Some(Source::Missing(fetch, Some(stub)))
    }

    /// The class without source as a stub of its public and protected members, from its class
    /// file (in a jar or an `.aar`), at the member the call means when it can tell.
    fn stub(&self, stub: Option<ClassFile>, why: &str, class: &str) -> Looked {
        let made = stub.as_ref().and_then(|s| {
            let bytes = libsrc::class_bytes(&s.archive, &s.entry)?;
            let note = format!("{why}: signatures from the class file, no source");
            Some((s, libsrc::stub(&bytes, &note)?))
        });
        let Some((s, text)) = made else {
            return Looked::NotFound(
                format!("{why}; no class file to show either"),
                Some(self.jump.call.clone()),
            );
        };
        let path = PathBuf::from(format!(
            "{}!/{}.java (no source)",
            s.archive.display(),
            s.qualified.replace('.', "/")
        ));
        let mut c = self.jump.call.clone();
        c.class = class.to_string();
        let at = libsrc::find_member(Path::new("Stub.java"), &text, class, &c)
            .offsets
            .first()
            .copied();
        Looked::Stub(path, text, at, why.to_string())
    }
}

/// A source file's text (a file, or `jar!/entry`), up to 16 MB.
fn read_source(path: &Path) -> Option<String> {
    if crate::libs::is_archive_path(path) {
        return crate::libs::read_entry(path).filter(|t| t.len() <= MAX_SOURCE);
    }
    if std::fs::metadata(path).ok()?.len() > MAX_SOURCE as u64 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// A source bigger than this is not read.
const MAX_SOURCE: usize = 16 * 1024 * 1024;

/// `a.b.Outer.Inner` and then `a.b.Outer`: the class and the classes it is nested in.
fn outers(q: &str) -> Vec<String> {
    let parts: Vec<&str> = q.split('.').collect();
    (1..=parts.len())
        .rev()
        .map(|n| parts[..n].join("."))
        .take_while(|s| {
            s.rsplit('.')
                .next()
                .is_some_and(|x| x.starts_with(char::is_uppercase))
        })
        .collect()
}

fn line_at(text: &str, at: usize) -> String {
    let s = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let e = text[at..].find('\n').map_or(text.len(), |i| at + i);
    text[s..e].trim().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_nested_class_and_the_classes_around_it() {
        assert_eq!(
            super::outers("androidx.core.app.NotificationCompat.Builder"),
            [
                "androidx.core.app.NotificationCompat.Builder",
                "androidx.core.app.NotificationCompat"
            ]
        );
        assert_eq!(super::outers("a.B"), ["a.B"]);
        assert_eq!(super::class_path("a.b.Outer.Builder"), "Outer.Builder");
    }
}
