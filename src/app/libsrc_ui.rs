//! Go to declaration on a library or Android SDK call, like Android Studio: the exact declaration
//! (the overload whose parameter types take the arguments, looked for in the class and then, breadth
//! first, in all its supertypes) in the class's source, opened read-only. A missing source is
//! downloaded first, on a worker thread with a status line, and cached; when there is no source at
//! all, a stub made from the class file is shown instead. The whole lookup (jars, sources, the
//! supertype walk, the stub) runs on a worker: the editor never waits for it.

use super::{App, Tab};
use crate::index::Hit;
use crate::libs::Libraries;
use crate::libsrc::{self, Fetch};
use crate::resolve::LibCall;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// A failed download is tried again after this long (a network blip is not for the session).
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// How many supertypes up, and how many classes in all, a member is looked for.
const MAX_DEPTH: usize = 8;
const MAX_CLASSES: usize = 40;

/// Downloads of sources, and lookups on a worker.
#[derive(Default)]
pub(super) struct Fetches {
    running: Vec<(String, Receiver<Result<(), String>>)>,
    /// How each download ended, and when.
    pub ended: HashMap<String, (Result<(), String>, Instant)>,
    /// The download key and the jump to make once it ends.
    waiting: Option<(String, Jump)>,
    /// A lookup on a worker, and whether it opens what it finds.
    looking: Option<(Receiver<Looked>, bool)>,
    /// The project's compile SDK, read once per project root.
    api: Option<(PathBuf, Option<u32>)>,
    /// How many downloads ran (tests check a cached source is not downloaded again).
    pub started: usize,
}

/// What a jump needs to look again after a download: the call and the file's imports.
#[derive(Clone, Debug)]
struct Jump {
    call: LibCall,
    imports: Vec<String>,
}

/// A class file inside a jar: for a stub when no source can be had.
#[derive(Clone, Debug)]
struct ClassFile {
    jar: PathBuf,
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
    /// The member is not in the class or its supertypes, or the class is not found.
    NotFound(String, Jump),
}

/// Where a class's source is.
enum Source {
    Text(PathBuf),
    Missing(Fetch, Option<ClassFile>),
}

/// Everything a lookup reads, sent to the worker.
struct Lookup {
    jump: Jump,
    root: PathBuf,
    sdk: Option<PathBuf>,
    api: Option<u32>,
    libs: Arc<Libraries>,
    /// Downloads that may not run (again): done but the class was not in them (None), or failed
    /// lately (the error), for the stub's note.
    blocked: HashMap<String, Option<String>>,
}

impl App {
    /// Go to declaration on a library member call ([`crate::resolve::Res::Lib`]).
    pub(super) fn goto_lib_call(&mut self, call: LibCall) {
        let jump = Jump {
            call,
            imports: self.active_imports(),
        };
        self.look(jump, true);
    }

    /// The source of a library member call fetched when it is missing, without opening it (Find
    /// usages, so the declaration is there when wanted).
    pub(super) fn fetch_lib_source(&mut self, call: LibCall) {
        let jump = Jump {
            call,
            imports: self.active_imports(),
        };
        self.look(jump, false);
    }

    /// A lookup is running (tests wait for it).
    #[cfg(test)]
    pub(super) fn lib_reading(&self) -> bool {
        self.fetches.looking.is_some()
    }

    /// A new jump or file: a jump still waiting for a download or a lookup is dropped.
    pub(super) fn cancel_lib_jump(&mut self) {
        self.fetches.waiting = None;
        if self.fetches.looking.as_ref().is_some_and(|(_, open)| *open) {
            self.fetches.looking = None;
        }
    }

    /// A project opened: nothing of the last one's lookups is kept but the downloads' results.
    pub(super) fn reset_lib_lookups(&mut self) {
        self.fetches.waiting = None;
        self.fetches.looking = None;
        self.fetches.api = None;
    }

    /// The imports of the open file (where a class known only by its simple name may be nested).
    fn active_imports(&self) -> Vec<String> {
        self.tabs
            .get(self.active)
            .map(|t| {
                t.text
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("import "))
                    .map(|l| l.trim_end_matches(';').trim().to_string())
                    .filter(|l| !l.ends_with('*') && !l.contains(" as "))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Starts a lookup on a worker ([`App::poll_fetches`] takes its result).
    fn look(&mut self, jump: Jump, open: bool) {
        let Some(p) = &self.project else { return };
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
        let lookup = Lookup {
            jump,
            root,
            sdk,
            api,
            libs: self.libs.clone(),
            blocked,
        };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(lookup.run());
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        self.fetches.looking = Some((rx, open));
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
                self.look(jump, true);
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
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => {
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
                Some((wk, jump)) if wk == key => self.look(jump, true),
                other => self.fetches.waiting = other,
            }
        }
        let Some((rx, open)) = &self.fetches.looking else {
            return;
        };
        let open = *open;
        let Ok(looked) = rx.try_recv() else {
            return;
        };
        self.fetches.looking = None;
        match looked {
            Looked::Fetch(fetch, jump) => self.start_fetch(fetch, jump, open),
            _ if !open => {}
            Looked::Found(hits) => self.open_found(hits),
            Looked::Stub(path, text, at, why) => self.open_stub(path, text, at, why),
            Looked::NotFound(why, jump) => self.library_functions(why, &jump.call),
        }
    }

    /// One fitting declaration opens; several that fit as well go to the picker.
    fn open_found(&mut self, hits: Vec<(PathBuf, usize, String)>) {
        if let [(path, at, _)] = hits.as_slice() {
            self.open_file(&path.clone(), Some(*at));
            return;
        }
        let name = hits.first().map(|(_, _, l)| l.clone()).unwrap_or_default();
        let n = hits.len();
        let hits = hits
            .into_iter()
            .map(|(path, offset, detail)| Hit {
                name: name.clone(),
                detail,
                path,
                offset,
                score: 0,
            })
            .collect();
        self.pick(format!("{n} declarations fit"), hits);
    }

    /// Not a member of the class or its supertypes: a library's top-level function of that name
    /// (an extension such as `Modifier.padding`), never a project declaration that only shares
    /// the name.
    fn library_functions(&mut self, why: String, call: &LibCall) {
        let found: Vec<Hit> = self
            .libs
            .named(&call.member)
            .filter(|s| !s.kind.is_type())
            .map(|s| super::libs_ui::lib_hit(s, 0))
            .collect();
        match found.len() {
            0 => self.status = why,
            1 if super::libs_ui::is_binary_archive(&found[0].path) => {
                self.status = format!("{why}; `{}`: {} (no sources)", call.member, found[0].detail)
            }
            1 => self.open_file(&found[0].path.clone(), None),
            n => self.pick(format!("`{}` in {n} libraries", call.member), found),
        }
    }

    /// The stub of a class without source, read-only, at the member when it can tell.
    fn open_stub(&mut self, path: PathBuf, text: String, at: Option<usize>, why: String) {
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

impl Lookup {
    /// On a worker: the declarations the call means, breadth first through the class and its
    /// supertypes; else the download that would bring a missing source, or the stub.
    fn run(self) -> Looked {
        let call = &self.jump.call;
        // the receiver's class: an import names it for sure; else each candidate is a class to
        // look in (with whether it is a nested-class guess, kept only when its class file is there)
        let starts: Vec<Vec<(String, bool)>> = match &call.qualified {
            Some(q) => vec![vec![(q.clone(), false)]],
            None => self.candidates().into_iter().map(|c| vec![c]).collect(),
        };
        let mut queue: VecDeque<(Vec<(String, bool)>, usize)> =
            starts.into_iter().map(|c| (c, 0)).collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut found: Vec<(PathBuf, usize, String)> = Vec::new();
        let mut found_depth = usize::MAX;
        let mut missing: Option<(Fetch, Option<ClassFile>, String)> = None;
        while let Some((cands, depth)) = queue.pop_front() {
            if depth > found_depth || seen.len() >= MAX_CLASSES {
                break; // the nearest class that declares it wins
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
            let path = match source {
                Source::Text(p) => p,
                Source::Missing(fetch, stub) => {
                    if missing.is_none() {
                        missing = Some((fetch, stub, class));
                    }
                    continue; // its supertypes cannot be read either
                }
            };
            let text = if crate::libs::is_archive_path(&path) {
                crate::libs::read_entry(&path)
            } else {
                std::fs::read_to_string(&path).ok()
            };
            let Some(text) = text else { continue };
            let mut c = call.clone();
            c.class = class.clone();
            let here = libsrc::find_member(&path, &text, &class, &c);
            if !here.is_empty() {
                found_depth = depth;
                found.extend(
                    here.into_iter()
                        .map(|o| (path.clone(), o, line_at(&text, o))),
                );
                continue;
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
        if !found.is_empty() {
            return Looked::Found(found);
        }
        if let Some((fetch, stub, class)) = missing {
            return match self.blocked.get(&fetch.key()) {
                Some(None) => {
                    let why = format!("{} has no source of {class}", fetch.label());
                    self.stub(stub, &why, &class)
                }
                Some(Some(e)) => {
                    let why = format!("{} could not be downloaded ({e})", fetch.label());
                    self.stub(stub, &why, &class)
                }
                None => Looked::Fetch(fetch, self.jump.clone()),
            };
        }
        let why = if seen.is_empty() {
            format!(
                "`{}.{}`: no source of {} in the Android SDK or the project's libraries",
                call.class, call.member, call.class
            )
        } else {
            format!(
                "`{}` is not a member of {} or its supertypes (an extension function?)",
                call.member, call.class
            )
        };
        Looked::NotFound(why, self.jump.clone())
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
            self.libs
                .named(&call.class)
                .filter(|s| s.kind.is_type())
                .map(|s| (s.qualified(), false)),
        );
        let mut seen = HashSet::new();
        out.retain(|(q, _)| seen.insert(q.clone()));
        out
    }

    /// Where the source of class `q` is, or the download it needs; None when `q` is no such
    /// class (a guess whose class file is not there, a name no SDK or library has).
    fn locate(&self, q: &str, guess: bool) -> Option<Source> {
        if libsrc::is_platform(q) {
            let (sdk, api) = (self.sdk.clone()?, self.api?);
            let jar = sdk.join(format!("platforms/android-{api}/android.jar"));
            let entry = libsrc::class_entry(q);
            let in_jar = || zip_has(&jar, &entry);
            if guess && !in_jar() {
                return None;
            }
            if let Some(path) = libsrc::sdk_source(&sdk, api, q) {
                return Some(Source::Text(path));
            }
            if !in_jar() {
                return None; // not an SDK class: maybe a library one of the same name
            }
            let stub = ClassFile {
                jar,
                entry,
                qualified: q.to_string(),
            };
            return Some(Source::Missing(Fetch::Sdk { sdk, api }, Some(stub)));
        }
        // the class, or for a nested class its outer class (whose file holds it)
        let (s, outer) = outers(q).into_iter().find_map(|o| {
            let simple = o.rsplit('.').next().unwrap_or(&o).to_string();
            self.libs
                .named(&simple)
                .find(|s| s.kind.is_type() && s.qualified() == o)
                .cloned()
                .map(|s| (s, o))
        })?;
        let jar = s.archive.extension().is_some_and(|e| e == "jar");
        if guess && jar && !zip_has(&s.archive, &libsrc::class_entry(q)) {
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
        let stub = jar.then(|| ClassFile {
            jar: s.archive.to_path_buf(),
            entry: libsrc::class_entry(q),
            qualified: q.to_string(),
        });
        let fetch = Fetch::Maven {
            coords: s.artifact.to_string(),
            repos: libsrc::repos(&self.root),
        };
        Some(Source::Missing(fetch, stub))
    }

    /// The class without source as a stub of its public and protected members, from its class
    /// file, at the member the call means when it can tell.
    fn stub(&self, stub: Option<ClassFile>, why: &str, class: &str) -> Looked {
        let made = stub.as_ref().and_then(|s| {
            let bytes = zip_read(&s.jar, &s.entry)?;
            let note = format!("{why}: signatures from the class file, no source");
            Some((s, libsrc::stub(&bytes, &note)?))
        });
        let Some((s, text)) = made else {
            return Looked::NotFound(
                format!("{why}; no class file to show either"),
                self.jump.clone(),
            );
        };
        let path = PathBuf::from(format!(
            "{}!/{}.java (no source)",
            s.jar.display(),
            s.qualified.replace('.', "/")
        ));
        let mut c = self.jump.call.clone();
        c.class = class.to_string();
        let at = libsrc::find_member(Path::new("Stub.java"), &text, class, &c)
            .first()
            .copied();
        Looked::Stub(path, text, at, why.to_string())
    }
}

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

fn zip_read(jar: &Path, entry: &str) -> Option<Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(jar).ok()?).ok()?;
    let mut e = z.by_name(entry).ok()?;
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut v).ok()?;
    Some(v)
}

fn zip_has(jar: &Path, entry: &str) -> bool {
    std::fs::File::open(jar)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
        .is_some_and(|mut z| z.by_name(entry).is_ok())
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
