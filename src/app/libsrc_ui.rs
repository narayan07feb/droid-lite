//! Go to declaration on a library or Android SDK call, like Android Studio: the exact declaration
//! (the overload whose parameter types take the arguments) in the class's source, opened
//! read-only. A missing source is downloaded first, on a worker thread with a status line, and
//! cached; when there is no source at all, a stub made from the class file is shown instead.

use super::{App, Tab};
use crate::index::Hit;
use crate::libsrc::{self, Fetch};
use crate::resolve::LibCall;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

/// Downloads of sources: the running ones, how the finished ones ended, and the declaration
/// waiting for one.
#[derive(Default)]
pub(super) struct Fetches {
    running: Vec<(String, Receiver<Result<(), String>>)>,
    pub ended: HashMap<String, Result<(), String>>,
    /// The download key, the call to go to once it ends, and the class file for a stub.
    waiting: Option<(String, LibCall, Option<ClassFile>)>,
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

impl App {
    /// Go to declaration on a library member call ([`crate::resolve::Res::Lib`]).
    pub(super) fn goto_lib_call(&mut self, call: LibCall) {
        self.lib_source(call, true);
    }

    /// The source of a library member call: opened at its declaration (`open`), or only fetched
    /// when it is missing (Find usages, so the declaration is there when wanted).
    pub(super) fn lib_source(&mut self, call: LibCall, open: bool) {
        let Some(p) = &self.project else { return };
        let root = p.root.clone();
        let scripts: Vec<PathBuf> = p
            .files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
            .cloned()
            .collect();
        let sdk = libsrc::sdk_dir();
        let api = sdk
            .as_ref()
            .and_then(|s| libsrc::compile_sdk(&root, &scripts, s));
        for q in self.lib_candidates(&call) {
            if libsrc::is_platform(&q) {
                let (Some(sdk), Some(api)) = (sdk.clone(), api) else {
                    continue;
                };
                if let Some(path) = libsrc::sdk_source(&sdk, api, &q) {
                    if open {
                        self.open_lib_member(&path, &q, &call);
                    }
                    return;
                }
                let jar = sdk.join(format!("platforms/android-{api}/android.jar"));
                let entry = format!("{}.class", q.replace('.', "/"));
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
            // the index keeps one symbol per class (the first artifact that has it)
            let sym = self
                .libs
                .named(&call.class)
                .find(|s| s.kind.is_type() && s.qualified() == q)
                .cloned();
            if let Some(s) = sym {
                if let Some(src) = s.source_path() {
                    if open {
                        self.open_lib_member(&src, &q, &call);
                    }
                    return;
                }
                if let Some(jar) = libsrc::cached_sources_jar(&s.artifact).filter(|j| j.is_file())
                    && let Some(e) = libsrc::jar_entry(&jar, &q)
                {
                    if open {
                        let src = PathBuf::from(format!("{}!/{e}", jar.display()));
                        self.open_lib_member(&src, &q, &call);
                    }
                    return;
                }
                let stub = s
                    .archive
                    .extension()
                    .is_some_and(|e| e == "jar")
                    .then(|| ClassFile {
                        jar: s.archive.to_path_buf(),
                        entry: format!("{}.class", q.replace('.', "/")),
                        qualified: q.clone(),
                    });
                let fetch = Fetch::Maven {
                    coords: s.artifact.to_string(),
                    repos: libsrc::repos(&root),
                };
                self.start_fetch(fetch, call, stub, open);
                return;
            }
        }
        if open {
            self.status = format!(
                "No declaration found for `{}.{}` in the Android SDK or the project's libraries",
                call.class, call.member
            );
        }
    }

    /// The qualified names the receiver's class may have: its import, the `*` imports, `java.lang`,
    /// then library classes of that name.
    fn lib_candidates(&self, call: &LibCall) -> Vec<String> {
        let mut out: Vec<String> = call.qualified.iter().cloned().collect();
        out.extend(call.stars.iter().map(|s| format!("{s}.{}", call.class)));
        out.push(format!("java.lang.{}", call.class));
        out.extend(
            self.libs
                .named(&call.class)
                .filter(|s| s.kind.is_type())
                .map(|s| s.qualified()),
        );
        let mut seen = std::collections::HashSet::new();
        out.retain(|q| seen.insert(q.clone()));
        out
    }

    /// Opens `path` (a file, or `jar!/entry`) at the declaration `call` means in class `qualified`:
    /// one opens, several that really fit go to the picker, none opens the class.
    fn open_lib_member(&mut self, path: &Path, qualified: &str, call: &LibCall) {
        let text = if crate::libs::is_archive_path(path) {
            crate::libs::read_entry(path)
        } else {
            std::fs::read_to_string(path).ok()
        };
        let Some(text) = text else {
            self.status = format!("Cannot read {}", path.display());
            return;
        };
        let class = qualified.rsplit('.').next().unwrap_or(qualified);
        let found = libsrc::find_member(path, &text, class, call);
        match found.len() {
            0 => {
                let at = class_offset(&text, class);
                self.open_file(path, at);
                self.status = format!("`{}` not found in {class}: showing the class", call.member);
            }
            1 => self.open_file(path, Some(found[0])),
            n => {
                let hits = found
                    .iter()
                    .map(|&o| Hit {
                        name: call.member.clone(),
                        detail: line_at(&text, o),
                        path: path.to_path_buf(),
                        offset: o,
                        score: 0,
                    })
                    .collect();
                self.pick(
                    format!("`{class}.{}`: {n} declarations fit", call.member),
                    hits,
                );
            }
        }
    }

    /// Downloads a missing source on a worker (once per key at a time), then goes to `call`; a
    /// download that failed before goes to the stub at once.
    fn start_fetch(&mut self, fetch: Fetch, call: LibCall, stub: Option<ClassFile>, open: bool) {
        let key = fetch.key();
        if let Some(Err(e)) = self.fetches.ended.get(&key).cloned() {
            if open {
                self.open_stub(
                    stub,
                    &call,
                    &format!("{} could not be downloaded ({e})", fetch.label()),
                );
            }
            return;
        }
        if open {
            self.fetches.waiting = Some((key.clone(), call, stub));
        }
        if self.fetches.running.iter().any(|(k, _)| *k == key) {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let label = fetch.label();
        std::thread::spawn(move || {
            let _ = tx.send(libsrc::fetch(&fetch));
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        self.fetches.running.push((key, rx));
        self.fetches.started += 1;
        if open {
            self.status = format!("Downloading {label}… (the editor keeps working)");
        }
    }

    /// A finished download: say how it went, and go to the declaration that waited for it.
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
            self.fetches.ended.insert(key.clone(), r.clone());
            let Some((wk, call, stub)) = self.fetches.waiting.take() else {
                continue;
            };
            if wk != key {
                self.fetches.waiting = Some((wk, call, stub));
                continue;
            }
            match r {
                Ok(()) => self.goto_lib_call(call),
                Err(e) => {
                    self.open_stub(stub, &call, &format!("No source could be downloaded ({e})"))
                }
            }
        }
    }

    /// The class without source: its public methods as Java declarations, read-only, at the one
    /// `call` means when it can tell.
    fn open_stub(&mut self, stub: Option<ClassFile>, call: &LibCall, why: &str) {
        let made = stub.as_ref().and_then(|s| {
            let bytes = zip_read(&s.jar, &s.entry)?;
            let text = libsrc::stub(
                &bytes,
                &format!("{why}: signatures from the class file, no source"),
            )?;
            Some((s, text))
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

/// Where `class Name` / `interface Name` / `object Name` is spelled.
fn class_offset(text: &str, class: &str) -> Option<usize> {
    ["class ", "interface ", "object "].iter().find_map(|k| {
        text.match_indices(&format!("{k}{class}"))
            .map(|(i, _)| i + k.len())
            .find(|&e| {
                !text[e + class.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
            })
    })
}

fn line_at(text: &str, at: usize) -> String {
    let s = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let e = text[at..].find('\n').map_or(text.len(), |i| at + i);
    text[s..e].trim().to_string()
}
