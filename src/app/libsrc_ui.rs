//! Go to declaration on a library or Android SDK call, like Android Studio: the exact declaration
//! (the overload whose parameter types take the arguments, looked for in the class and then its
//! supertypes) in the class's source, opened read-only. A missing source is downloaded first, on a
//! worker thread with a status line, and cached; when there is no source at all, a stub made from
//! the class file is shown instead. Reading and scanning a source also runs on a worker.

use super::{App, Tab};
use crate::index::Hit;
use crate::libsrc::{self, Fetch};
use crate::resolve::LibCall;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// A failed download is tried again after this long (a network blip is not for the session).
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// How many supertypes up a member is looked for.
const MAX_SUPER: usize = 3;

/// Downloads of sources and reads of a source on a worker.
#[derive(Default)]
pub(super) struct Fetches {
    running: Vec<(String, Receiver<Result<(), String>>)>,
    /// How each download ended, and when.
    pub ended: HashMap<String, (Result<(), String>, Instant)>,
    /// The download key, the call to go to once it ends, and the class file for a stub.
    waiting: Option<(String, LibCall, Option<ClassFile>)>,
    /// A source being read and scanned on a worker for the declaration a call means.
    reading: Option<Receiver<Read>>,
    /// The project's compile SDK, read once per project root.
    api: Option<(PathBuf, Option<u32>)>,
    /// How many downloads ran (tests check a cached source is not downloaded again).
    pub started: usize,
}

/// A class file inside a jar: for a stub when no source can be had.
#[derive(Clone, Debug)]
struct ClassFile {
    jar: PathBuf,
    entry: String,
    qualified: String,
}

/// A source read on a worker: the declarations of the call in it, or its supertypes to look in.
struct Read {
    path: PathBuf,
    call: LibCall,
    class: String,
    /// The declarations that fit, with their lines (for the picker).
    found: Vec<(usize, String)>,
    /// The class's supertypes (qualified), when the member is not in it.
    supers: Vec<String>,
    depth: usize,
    error: Option<String>,
}

impl App {
    /// Go to declaration on a library member call ([`crate::resolve::Res::Lib`]).
    pub(super) fn goto_lib_call(&mut self, call: LibCall) {
        self.lib_source(call, true, 0);
    }

    /// A source is still being read for a jump (tests wait for it).
    #[cfg(test)]
    pub(super) fn lib_reading(&self) -> bool {
        self.fetches.reading.is_some()
    }

    /// A new Go to declaration: a jump still waiting for a download or a read is dropped.
    pub(super) fn cancel_lib_jump(&mut self) {
        self.fetches.waiting = None;
        self.fetches.reading = None;
    }

    /// The source of a library member call: opened at its declaration (`open`), or only fetched
    /// when it is missing (Find usages, so the declaration is there when wanted). `depth`: how
    /// many supertypes up this is.
    pub(super) fn lib_source(&mut self, call: LibCall, open: bool, depth: usize) {
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
                let api = sdk
                    .as_ref()
                    .and_then(|s| libsrc::compile_sdk(&root, &scripts, s));
                self.fetches.api = Some((root.clone(), api));
                api
            }
        };
        for (q, guess) in self.lib_candidates(&call) {
            if libsrc::is_platform(&q) {
                let (Some(sdk), Some(api)) = (sdk.clone(), api) else {
                    continue;
                };
                let jar = sdk.join(format!("platforms/android-{api}/android.jar"));
                let entry = libsrc::class_entry(&q);
                // a guessed nested class must really be one (its class file is there)
                if guess && !zip_has(&jar, &entry) {
                    continue;
                }
                if let Some(path) = libsrc::sdk_source(&sdk, api, &q) {
                    if open {
                        self.read_source(path, call, depth);
                    }
                    return;
                }
                if !zip_has(&jar, &entry) {
                    continue; // not an SDK class: maybe a library one of the same name
                }
                let stub = ClassFile {
                    jar,
                    entry,
                    qualified: q.clone(),
                };
                self.start_fetch(Fetch::Sdk { sdk, api }, call, Some(stub), open);
                return;
            }
            // the class, or for a nested class its outer class (whose file holds it)
            let found = outers(&q).into_iter().find_map(|o| {
                let simple = o.rsplit('.').next().unwrap_or(&o).to_string();
                self.libs
                    .named(&simple)
                    .find(|s| s.kind.is_type() && s.qualified() == o)
                    .cloned()
                    .map(|s| (s, o))
            });
            let Some((s, outer)) = found else {
                continue;
            };
            if guess
                && s.archive.extension().is_some_and(|e| e == "jar")
                && !zip_has(&s.archive, &libsrc::class_entry(&q))
            {
                continue;
            }
            if let Some(src) = s.source_path() {
                if open {
                    self.read_source(src, call, depth);
                }
                return;
            }
            if let Some(jar) = libsrc::cached_sources_jar(&s.artifact).filter(|j| j.is_file())
                && let Some(e) = libsrc::jar_entry(&jar, &outer)
            {
                if open {
                    let src = PathBuf::from(format!("{}!/{e}", jar.display()));
                    self.read_source(src, call, depth);
                }
                return;
            }
            let stub = s
                .archive
                .extension()
                .is_some_and(|e| e == "jar")
                .then(|| ClassFile {
                    jar: s.archive.to_path_buf(),
                    entry: libsrc::class_entry(&q),
                    qualified: q.clone(),
                });
            let fetch = Fetch::Maven {
                coords: s.artifact.to_string(),
                repos: libsrc::repos(&root),
            };
            self.start_fetch(fetch, call, stub, open);
            return;
        }
        if open {
            self.status = format!(
                "`{}.{}`: no source of {} in the Android SDK or the project's libraries",
                call.class, call.member, call.class
            );
        }
    }

    /// The qualified names the receiver's class may have, each with whether it is a guess that
    /// must be checked. An import names it for sure: only that. Else a class nested in one the
    /// open file imports (`Locator.Spot` read as `Spot`), the `*` imports, `java.lang`, then
    /// library classes of that name.
    fn lib_candidates(&self, call: &LibCall) -> Vec<(String, bool)> {
        if let Some(q) = &call.qualified {
            return vec![(q.clone(), false)];
        }
        let imports: Vec<String> = self
            .tabs
            .get(self.active)
            .map(|t| {
                t.text
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("import "))
                    .map(|l| l.trim_end_matches(';').trim().to_string())
                    .filter(|l| !l.ends_with('*') && !l.contains(" as "))
                    .collect()
            })
            .unwrap_or_default();
        let mut out: Vec<(String, bool)> = imports
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
        let mut seen = std::collections::HashSet::new();
        out.retain(|(q, _)| seen.insert(q.clone()));
        out
    }

    /// Reads and scans `path` (a file, or `jar!/entry`) on a worker for the declaration `call`
    /// means ([`App::poll_fetches`] opens it).
    fn read_source(&mut self, path: PathBuf, call: LibCall, depth: usize) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read(path, call, depth));
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        self.fetches.reading = Some(rx);
    }

    /// Downloads a missing source on a worker (once per key at a time), then goes to `call`. A
    /// download that failed lately goes to the stub at once; one that worked but brought no
    /// source of the class goes to the stub too, never downloading again.
    fn start_fetch(&mut self, fetch: Fetch, call: LibCall, stub: Option<ClassFile>, open: bool) {
        let key = fetch.key();
        match self.fetches.ended.get(&key).cloned() {
            Some((Ok(()), _)) => {
                if open {
                    let why = format!("{} has no source of {}", fetch.label(), call.class);
                    self.open_stub(stub, &call, &why);
                }
                return;
            }
            Some((Err(e), at)) if at.elapsed() < RETRY_AFTER => {
                if open {
                    let why = format!("{} could not be downloaded ({e})", fetch.label());
                    self.open_stub(stub, &call, &why);
                }
                return;
            }
            _ => {}
        }
        if open {
            self.fetches.waiting = Some((key.clone(), call, stub));
        }
        let label = fetch.label();
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

    /// A finished download goes on to the declaration that waited for it; a finished read opens
    /// the declaration (or looks in the supertypes).
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
            self.fetches
                .ended
                .insert(key.clone(), (r.clone(), Instant::now()));
            if let Err(e) = &r {
                self.status = format!("Download failed: {e}");
            }
            match self.fetches.waiting.take() {
                Some((wk, call, _)) if wk == key && r.is_ok() => self.lib_source(call, true, 0),
                Some((wk, call, stub)) if wk == key => {
                    let why = format!(
                        "No source could be downloaded ({})",
                        r.err().unwrap_or_default()
                    );
                    self.open_stub(stub, &call, &why);
                }
                other => self.fetches.waiting = other,
            }
        }
        let Some(rx) = &self.fetches.reading else {
            return;
        };
        let Ok(read) = rx.try_recv() else {
            return;
        };
        self.fetches.reading = None;
        self.opened(read);
    }

    /// A source was read: one fitting declaration opens, several that fit as well go to the
    /// picker; none: the supertypes are looked in, then the status says the member is not there.
    fn opened(&mut self, r: Read) {
        if let Some(e) = r.error {
            self.status = e;
            return;
        }
        match r.found.len() {
            1 => self.open_file(&r.path, Some(r.found[0].0)),
            0 => {
                if r.depth < MAX_SUPER
                    && let Some(s) = r.supers.first()
                {
                    // `Builder.setSmallIcon` declared in a superclass: look there
                    let mut call = r.call.clone();
                    call.qualified = Some(s.clone());
                    call.class = s.rsplit('.').next().unwrap_or(s).to_string();
                    call.stars.clear();
                    self.lib_source(call, true, r.depth + 1);
                    return;
                }
                self.status = format!(
                    "`{}` is not a member of {} or its supertypes (an extension function?)",
                    r.call.member, r.class
                );
            }
            n => {
                let hits = r
                    .found
                    .iter()
                    .map(|(o, line)| Hit {
                        name: r.call.member.clone(),
                        detail: line.clone(),
                        path: r.path.clone(),
                        offset: *o,
                        score: 0,
                    })
                    .collect();
                let title = format!("`{}.{}`: {n} declarations fit", r.class, r.call.member);
                self.pick(title, hits);
            }
        }
    }

    /// The class without source: its public constructors and methods as Java declarations,
    /// read-only, at the one `call` means when it can tell.
    fn open_stub(&mut self, stub: Option<ClassFile>, call: &LibCall, why: &str) {
        let made = stub.as_ref().and_then(|s| {
            let bytes = zip_read(&s.jar, &s.entry)?;
            let note = format!("{why}: signatures from the class file, no source");
            Some((s, libsrc::stub(&bytes, &note)?))
        });
        let Some((s, text)) = made else {
            self.status = format!("{why}; no class file to show either");
            return;
        };
        let path = PathBuf::from(format!(
            "{}!/{}.java (no source)",
            s.jar.display(),
            s.qualified.replace('.', "/")
        ));
        let class = s
            .qualified
            .rsplit('.')
            .next()
            .unwrap_or(&s.qualified)
            .to_string();
        let at = libsrc::find_member(Path::new("Stub.java"), &text, &class, call)
            .first()
            .copied();
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
        self.status = why.to_string();
    }
}

/// On a worker: the declarations `call` means in the source at `path`, else the class's
/// supertypes to look in.
fn read(path: PathBuf, call: LibCall, depth: usize) -> Read {
    let class = call.class.clone();
    let mut r = Read {
        path: path.clone(),
        call,
        class: class.clone(),
        found: Vec::new(),
        supers: Vec::new(),
        depth,
        error: None,
    };
    let text = if crate::libs::is_archive_path(&path) {
        crate::libs::read_entry(&path)
    } else {
        std::fs::read_to_string(&path).ok()
    };
    let Some(text) = text else {
        r.error = Some(format!("Cannot read {}", path.display()));
        return r;
    };
    r.found = libsrc::find_member(&path, &text, &class, &r.call)
        .into_iter()
        .map(|o| (o, line_at(&text, o)))
        .collect();
    if r.found.is_empty() {
        r.supers = libsrc::supertypes(&text, &class)
            .into_iter()
            .filter(|s| s.rsplit('.').next() != Some("Object"))
            .collect();
    }
    r
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
