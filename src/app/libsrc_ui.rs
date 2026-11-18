//! Go to declaration on a library or Android SDK call, like Android Studio: the exact declaration
//! (the overload whose parameter types take the arguments, looked for in the class and then, breadth
//! first, in all its supertypes; an extension function by its receiver) in the source, opened
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
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// A failed download is tried again after this long (a network blip is not for the session).
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// How many supertypes up, and how many classes in all, a member is looked for.
const MAX_DEPTH: usize = 8;
const MAX_CLASSES: usize = 40;
/// How many library functions of the name are read to match an extension's receiver.
const MAX_EXTENSION_FILES: usize = 30;

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
    /// Find usages' lookup: only fetches a missing source.
    prefetch: Option<Receiver<Looked>>,
    /// The project's compile SDK, read once per project root.
    api: Option<(PathBuf, Option<u32>)>,
    /// How many downloads ran (tests check a cached source is not downloaded again).
    pub started: usize,
}

impl Fetches {
    /// Drops a jump still waiting for a download or a lookup (the user went elsewhere).
    pub(super) fn cancel(&mut self) {
        self.waiting = None;
        self.looking = None;
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
    /// Not found: why.
    NotFound(String),
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
    libs: Arc<Libraries>,
    /// Downloads that may not run (again): done but the class was not in them (None), or failed
    /// lately (the error), for the stub's note.
    blocked: HashMap<String, Option<String>>,
}

/// One lookup, sent to the worker.
struct Lookup {
    jump: Jump,
    /// Find usages: only find what to fetch, read nothing.
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
    pub(super) fn reset_lib_lookups(&mut self) {
        self.cancel_lib_jump();
        self.fetches.prefetch = None;
        self.fetches.api = None;
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
        let api = match &self.fetches.api {
            Some((r, api)) if *r == root => *api,
            _ => {
                let scripts: Vec<PathBuf> = p
                    .files
                    .iter()
                    .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
                    .cloned()
                    .collect();
                // read once per project; the scripts are small
                let api = sdk
                    .as_ref()
                    .and_then(|s| libsrc::compile_sdk(&root, &scripts, s));
                self.fetches.api = Some((root.clone(), api));
                api
            }
        };
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
    /// or the stub is shown); a finished lookup opens what it found.
    pub(super) fn poll_fetches(&mut self) {
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
            if let Err(e) = &r {
                self.status = format!("Download failed: {e}");
            }
            self.fetches.ended.insert(key.clone(), (r, Instant::now()));
            match self.fetches.waiting.take() {
                Some((wk, jump)) if wk == key => self.look(jump, false),
                other => self.fetches.waiting = other,
            }
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
            Some(Looked::NotFound(why)) => self.status = why,
            Some(Looked::Nothing) | None => {}
        }
    }

    /// One fitting declaration opens; several that fit as well go to the picker.
    fn open_found(&mut self, hits: Vec<(PathBuf, usize, String)>) {
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

/// A call as Find usages tells calls apart: its file, receiver class (simple, qualified) and
/// argument types.
type CallKey = (PathBuf, String, Option<String>, String);
/// A declaration: its file and offset.
type Decl = (PathBuf, usize);

/// Find usages of a library call, exactly: the candidate calls (same member, as many arguments,
/// argument types that can match) that lead to the same declaration as `target`. When the target's
/// declaration cannot be read (no source), the candidates on the same class are kept.
pub(super) fn exact_usages(
    base: LookupBase,
    target: LibCall,
    target_imports: Vec<String>,
    cands: Vec<(super::FoundLine, LibCall)>,
) -> Vec<super::FoundLine> {
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
    let want = decl(target.clone(), target_imports);
    let mut known: HashMap<CallKey, Option<Decl>> = HashMap::new();
    let mut imports: HashMap<PathBuf, Vec<String>> = HashMap::new();
    cands
        .into_iter()
        .filter(|(line, c)| {
            let Some(w) = &want else {
                return super::xref::same_lib_call(c, &target); // nothing to compare with
            };
            let im = imports
                .entry(line.path.clone())
                .or_insert_with(|| {
                    std::fs::read_to_string(&line.path)
                        .map(|t| imports_of(&t))
                        .unwrap_or_default()
                })
                .clone();
            let key = (
                line.path.clone(),
                c.class.clone(),
                c.qualified.clone(),
                format!("{:?}", c.args),
            );
            let got = known
                .entry(key)
                .or_insert_with(|| decl(c.clone(), im))
                .clone();
            match got {
                Some(g) => g == *w,
                None => super::xref::same_lib_call(c, &target),
            }
        })
        .map(|(line, _)| line)
        .collect()
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

impl Lookup {
    /// On a worker: the declarations the call means, breadth first through the class and its
    /// supertypes, else its extension functions; else the download that would bring a missing
    /// source, or the stub.
    fn run(mut self) -> Looked {
        if self.jump.call.builtin {
            // a Kotlin type: a standard library extension (its members have no source to read)
            let mut receivers: Vec<String> = vec![self.jump.call.class.clone()];
            receivers.extend(
                libsrc::kotlin_supers(&self.jump.call.class)
                    .iter()
                    .map(|s| s.to_string()),
            );
            if self.fetch_only {
                return Looked::Nothing;
            }
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
        let mut queue: VecDeque<(Vec<(String, bool)>, usize)> =
            starts.into_iter().map(|c| (c, 0)).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seen_names: Vec<String> = Vec::new();
        let mut exact: Vec<(PathBuf, usize, String)> = Vec::new();
        let mut exact_depth = usize::MAX;
        let mut loose: Vec<(PathBuf, usize, String)> = Vec::new();
        let mut missing: Option<(Fetch, Option<ClassFile>, String, usize)> = None;
        while let Some((cands, depth)) = queue.pop_front() {
            if depth > exact_depth || seen.len() >= MAX_CLASSES {
                break; // the nearest level that declares it exactly wins
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
            let class = q.rsplit('.').next().unwrap_or(&q).to_string();
            seen_names.push(class.clone());
            let path = match source {
                Source::Text(p) => p,
                Source::Missing(fetch, stub) => {
                    if missing.is_none() {
                        missing = Some((fetch, stub, class, depth));
                    }
                    continue; // its supertypes cannot be read either
                }
            };
            if self.fetch_only {
                continue; // the receiver's source is there: nothing to fetch for it
            }
            let Some(text) = read_source(&path) else {
                continue;
            };
            let mut c = call.clone();
            c.class = class.clone();
            let Members {
                offsets,
                exact: sure,
            } = libsrc::find_member(&path, &text, &class, &c);
            let hits = offsets
                .into_iter()
                .map(|o| (path.clone(), o, line_at(&text, o)));
            if sure {
                exact_depth = depth;
                exact.extend(hits);
                continue;
            }
            if loose.is_empty() {
                loose.extend(hits); // used only when no level fits exactly
            }
            if depth < MAX_DEPTH {
                for sup in libsrc::supertypes(&text, &class) {
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
        // a class without source as near as the match (or nearer) may declare it: fetch it first
        if let Some((fetch, stub, class, depth)) = missing
            && (exact.is_empty() || depth <= exact_depth)
        {
            match self.base.blocked.get(&fetch.key()) {
                None => return Looked::Fetch(fetch, self.jump.clone()),
                Some(_) if !exact.is_empty() => {}
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
        if self.fetch_only {
            return Looked::Nothing;
        }
        if !exact.is_empty() {
            return Looked::Found(exact);
        }
        // not a member: an extension function of the class or a supertype
        let mut receivers = vec![call.class.clone()];
        receivers.extend(seen_names.iter().cloned());
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
            Looked::NotFound(_) if !loose.is_empty() => Looked::Found(loose),
            other => other,
        }
    }

    /// The library's extension functions `fun Receiver.member` for one of `receivers`, read from
    /// the sources of the library functions of that name; ranked by the arguments.
    fn extensions(&self, receivers: &[String], why: Option<&str>) -> Looked {
        let call = &self.jump.call;
        let mut found = Vec::new();
        let mut binary: Vec<String> = Vec::new();
        for s in self
            .base
            .libs
            .named(&call.member)
            .filter(|s| !s.kind.is_type())
            .take(MAX_EXTENSION_FILES)
        {
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
        Looked::NotFound(msg)
    }

    /// A library argument (known by its name only) gets its supertypes from its source, so a
    /// `MainActivity`-like library class passed for a `Context` parameter is taken by it.
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
            return Looked::NotFound(format!("{why}; no class file to show either"));
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
    }
}
