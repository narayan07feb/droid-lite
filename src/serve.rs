//! `droid-lite serve`: the engine without the window, for editor plugins (VS Code, Cursor).
//!
//! One JSON request per line on stdin, one reply per line on stdout:
//! `{"id":1,"method":"initialize","params":{"root":"/path"}}` →
//! `{"id":1,"result":{...}}` or `{"id":1,"error":"..."}`.
//! Positions are 0-based lines and columns in UTF-16 code units (as VS Code counts them).
//!
//! Methods: `ping`, `initialize {root}`, `update {path, text}`, `remove {path}` or
//! `remove {paths: [...]}` (deleted files or folders), `lenses {path, text?}`, `implementations {name, member?}`, `stability {path, text?}`;
//! device tools without a project: `adb`, `devices`, `screenshot {serial}`,
//! `record_start {serial}`, `record_stop`. Device tools run on their own thread and answer when
//! done, so a device that hangs (each adb call is stopped after 20 s) holds up nothing else.
//! `record_stop` copies the video before it answers: up to about 16 minutes for a long recording
//! over a slow Wi-Fi adb (see [`capture::Recording::finish`]).
//!
//! `previews {path}` answers when the pictures are drawn (seconds), while other requests go on;
//! meanwhile it sends `{"event":"preview_status","file","text","done"?,"total"?}` lines. A newer
//! request answers an older one it replaces with an error. The pictures are PNGs in this engine's
//! own folder (`previews/vscode/<pid>/`), deleted when it ends.

use crate::capture::{self, Recording};
use crate::compose::{self, Stability, Verdict};
use crate::index::{Hit, Index};
use crate::kotlin::Kind;
use crate::project::Project;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Preview requests waiting for their pictures: file → (request number, JSON id).
type Waiting = Arc<Mutex<HashMap<PathBuf, (u64, Value)>>>;

/// A device tool call for the device thread: JSON id, method, params.
type DeviceCall = (Value, String, Value);

#[derive(Default)]
pub struct Server {
    index: Index,
    /// Compose compiler / stability-analyzer reports by function name.
    reports: HashMap<String, Stability>,
    open: bool,
    project: Option<crate::preview::ProjectRef>,
    previews: Option<Sender<crate::preview::Request>>,
    /// This engine's picture folder, once previews started.
    pngs: Option<PathBuf>,
    waiting: Waiting,
    /// The number of the last preview request.
    seq: u64,
    /// The editor's latest text of each file `update` sent: the index's offsets point into it.
    texts: HashMap<PathBuf, String>,
    /// The project's files: those of `initialize`, plus files `update` added, minus those `remove`
    /// dropped; the previews' file list is made again from it when it changed.
    files: std::collections::BTreeSet<PathBuf>,
    files_changed: bool,
    /// Files `update` sent since the last preview request (text or disk): the preview reads their
    /// packages and declared names again.
    edited: std::collections::BTreeSet<PathBuf>,
    devices: Option<(Sender<DeviceCall>, std::thread::JoinHandle<()>)>,
    /// Set at shutdown: a recording's copy under way stops (no adb pull outlives the engine).
    stopping: Arc<std::sync::atomic::AtomicBool>,
    out: Out,
}

/// Where answers that come later go: stdout, or a channel in tests.
#[derive(Clone, Default)]
struct Out(Option<Sender<Value>>);

impl Out {
    fn emit(&self, v: Value) {
        match &self.0 {
            Some(tx) => {
                let _ = tx.send(v);
            }
            None => emit(&v),
        }
    }
}

/// One JSON line on stdout; the preview thread writes too, so each line is written whole.
fn emit(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// Runs `f`; a panic (a bug) becomes an error answer instead of ending the engine.
fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let why = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Err(format!("internal error: {why}"))
    })
}

fn answer(id: Value, r: Result<Value, String>) -> Value {
    match r {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(e) => json!({ "id": id, "error": e }),
    }
}

/// Saves the pictures as PNGs in `dir` (new names each time, so an editor never shows a cached
/// one) and deletes this file's older ones and anyone's older than the preview cache keeps.
fn save_cards(dir: &Path, file: &Path, cards: crate::preview_cache::Cards) -> Vec<Value> {
    let _ = std::fs::create_dir_all(dir);
    let key = format!("{:016x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        file.hash(&mut h);
        h.finish()
    });
    let ttl = std::time::Duration::from_secs(crate::preview_cache::TTL_SECS);
    if let Ok(old) = std::fs::read_dir(dir) {
        for e in old.flatten() {
            let expired = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= ttl));
            if e.file_name().to_string_lossy().starts_with(&key) || expired {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    cards
        .into_iter()
        .enumerate()
        .map(|(i, (name, pic))| {
            // the worker's PNG bytes as they are, never decoded and encoded again
            let png = pic.and_then(|p| {
                let path = dir.join(format!("{key}-{stamp}-{i}.png"));
                std::fs::write(&path, &p.png).map_err(|e| e.to_string())?;
                Ok(path)
            });
            match png {
                Ok(p) => json!({ "name": name, "png": p }),
                Err(e) => json!({ "name": name, "error": e }),
            }
        })
        .collect()
}

/// The line to send for one preview event, if any: an answer only to the request it belongs to
/// (an older request's pictures never answer a newer one).
fn on_event(event: crate::preview::Event, waiting: &Waiting, dir: &Path) -> Option<Value> {
    use crate::preview::Event;
    let take = |f: &Path, seq: u64| {
        let mut w = waiting.lock().ok()?;
        if w.get(f)?.0 != seq {
            return None; // an older request's answer: that one was answered when it was replaced
        }
        w.remove(f).map(|(_, id)| id)
    };
    match event {
        Event::Status(file, text, progress) => {
            let mut v = json!({ "event": "preview_status", "file": file, "text": text });
            if let Some((done, total)) = progress {
                v["done"] = json!(done);
                v["total"] = json!(total);
            }
            Some(v)
        }
        Event::Done { file, seq, cards } => {
            let id = take(&file, seq)?;
            Some(
                json!({ "id": id, "result": { "file": file, "cards": save_cards(dir, &file, cards) } }),
            )
        }
        Event::Failed {
            file,
            seq,
            error: e,
        }
        | Event::Skipped {
            file,
            seq,
            reason: e,
        } => Some(json!({ "id": take(&file, seq)?, "error": e })),
    }
}

/// Sends the preview worker's progress and answers as they come, until the client ends. The
/// pictures go to `dir`.
fn relay(events: Receiver<crate::preview::Event>, waiting: Waiting, out: Out, dir: PathBuf) {
    for event in events {
        if let Some(v) = guarded(|| Ok(on_event(event, &waiting, &dir))).unwrap_or(None) {
            out.emit(v);
        }
    }
    // the preview thread ended (a bug): no answer comes for any request still waiting, whatever
    // its file
    let left: Vec<Value> = waiting
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, (_, id))| id)
        .collect();
    for id in left {
        out.emit(
            json!({ "id": id, "error": "the preview thread stopped; open the preview again" }),
        );
    }
}

/// This engine's picture folder in `root` (`previews/vscode`): one per engine, so another window's
/// engine never deletes pictures this one's panel still shows.
fn png_dir(root: &Path) -> PathBuf {
    root.join(std::process::id().to_string())
}

/// Deletes the picture folders in `root` of engines that have ended (one killed before it could
/// clean up), and loose pictures of older builds past the preview cache's time.
fn sweep_png_dirs(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let ttl = Duration::from_secs(crate::preview_cache::TTL_SECS);
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match name.parse::<i32>() {
            Ok(pid) if e.path().is_dir() => {
                // EPERM: alive, only not ours to signal
                let alive = unsafe { libc::kill(pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                if !alive {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
            _ => {
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= ttl));
                if old && name.ends_with(".png") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Line starts of a text, so placing an offset is a binary search: lenses place thousands of
/// declarations in one file (counting from the start each time took 2.9 s for a 1 MB file).
struct Lines(Vec<usize>);

impl Lines {
    fn new(text: &str) -> Lines {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Lines(starts)
    }

    /// 0-based line and UTF-16 column of byte `offset` in `text` (the text these lines were
    /// counted in); an offset past the end or inside a character counts from the character's
    /// start (never a panic).
    fn position(&self, text: &str, offset: usize) -> (usize, usize) {
        let mut at = offset.min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        let line = self.0.partition_point(|&s| s <= at) - 1;
        (line, text[self.0[line]..at].encode_utf16().count())
    }
}

/// 0-based line and UTF-16 column of byte `offset` in `text`.
#[cfg(test)]
fn position(text: &str, offset: usize) -> (usize, usize) {
    Lines::new(text).position(text, offset)
}

/// Places hits in the text the index read: the editor's text `update` sent, else the file on
/// disk. Each file is read and its lines counted once per request.
struct Placer<'a> {
    texts: &'a HashMap<PathBuf, String>,
    files: HashMap<PathBuf, (Option<String>, Lines)>,
    disk_reads: usize,
}

impl<'a> Placer<'a> {
    fn new(texts: &'a HashMap<PathBuf, String>) -> Placer<'a> {
        Placer {
            texts,
            files: HashMap::new(),
            disk_reads: 0,
        }
    }

    fn hit_json(&mut self, h: &Hit) -> Value {
        let texts = self.texts;
        let (disk, lines) = self.files.entry(h.path.clone()).or_insert_with(|| {
            let disk = match texts.get(&h.path) {
                Some(_) => None,
                None => {
                    self.disk_reads += 1;
                    Some(std::fs::read_to_string(&h.path).unwrap_or_default())
                }
            };
            let lines = Lines::new(disk.as_deref().unwrap_or_else(|| &texts[&h.path]));
            (disk, lines)
        });
        let text = disk.as_deref().unwrap_or_else(|| &texts[&h.path]);
        let (line, col) = lines.position(text, h.offset);
        json!({ "name": h.name, "detail": h.detail, "path": h.path, "line": line, "col": col })
    }

    fn lens(&mut self, line: usize, title: String, targets: &[Hit]) -> Value {
        let targets: Vec<Value> = targets.iter().map(|h| self.hit_json(h)).collect();
        json!({ "line": line, "title": title, "targets": targets })
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The path as the index keeps it (the project root is opened canonical, e.g. /private/var on
/// macOS); a deleted file or folder keeps its nearest canonical folder.
fn canon(p: &str) -> std::path::PathBuf {
    let p = Path::new(p);
    for a in p.ancestors() {
        if let Ok(c) = std::fs::canonicalize(a) {
            return match p.strip_prefix(a) {
                Ok(rest) if !rest.as_os_str().is_empty() => c.join(rest),
                _ => c,
            };
        }
    }
    p.to_path_buf()
}

/// Whether a canonical path is in the project: under its root, and no `..` left (a missing folder
/// kept it) that could lead out again.
fn in_project(root: Option<&Path>, path: &Path) -> bool {
    root.is_none_or(|r| path.starts_with(r))
        && !path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
}

fn adb() -> Result<std::path::PathBuf, String> {
    crate::logcat::adb().ok_or_else(|| "adb not found: set ANDROID_HOME or DROID_LITE_ADB".into())
}

/// Tools that may wait on a device: they run on the device thread.
fn is_device_call(method: &str) -> bool {
    matches!(
        method,
        "devices" | "screenshot" | "record_start" | "record_stop"
    )
}

/// One device tool call; the thread that runs them owns the recording.
fn device_call(
    recording: &mut Option<Recording>,
    method: &str,
    params: &Value,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Value, String> {
    let s = |k: &str| params[k].as_str().ok_or_else(|| format!("missing \"{k}\""));
    match method {
        "devices" => Ok(Value::Array(
            crate::logcat::devices(&adb()?)
                .into_iter()
                .map(|d| json!({ "serial": d.serial, "label": d.label, "online": d.online }))
                .collect(),
        )),
        "screenshot" => Ok(json!(capture::screenshot(
            &adb()?,
            s("serial")?,
            &capture::out_dir()
        )?)),
        "record_start" => {
            if recording.is_some() {
                return Err("a recording is already running".into());
            }
            *recording = Some(Recording::start(&adb()?, s("serial")?)?);
            Ok(Value::Null)
        }
        // waits while the device finishes and sends the video
        "record_stop" => {
            let r = recording.take().ok_or("no recording is running")?;
            Ok(json!(r.finish(&capture::out_dir(), stop)?))
        }
        _ => Err(format!("unknown method {method}")),
    }
}

/// The device thread: runs device calls one after another and answers each when it is done. A
/// `devices` call with a newer one queued behind it (they pile up while `record_stop` copies a long
/// video) does not run: the newest one's list answers it too. It ends when its sender is dropped;
/// a recording still running then stops, and a copy under way stops when `stop` is set (see
/// [`Server::shutdown`]).
fn device_thread(
    out: Out,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> (Sender<DeviceCall>, std::thread::JoinHandle<()>) {
    let (tx, rx) = channel::<DeviceCall>();
    let t = std::thread::spawn(move || {
        let mut recording = None;
        let mut queued = std::collections::VecDeque::new();
        let mut stale = Vec::new();
        loop {
            if queued.is_empty() {
                match rx.recv() {
                    Ok(c) => queued.push_back(c),
                    Err(_) => break,
                }
            }
            queued.extend(rx.try_iter());
            let Some((id, method, params)) = queued.pop_front() else {
                break;
            };
            if method == "devices" && queued.iter().any(|(_, m, _)| m == "devices") {
                stale.push(id);
                continue;
            }
            let r = guarded(|| device_call(&mut recording, &method, &params, &stop));
            if method == "devices" {
                for id in stale.drain(..) {
                    out.emit(answer(id, r.clone()));
                }
            }
            out.emit(answer(id, r));
        }
    });
    (tx, t)
}

/// The `"id"` of a request line that is not valid JSON (a lone surrogate in a text), found by a
/// scan of its top level, so the error answer still reaches the request that waits for it.
fn lenient_id(line: &str) -> Value {
    let b = line.as_bytes();
    let (mut depth, mut in_str, mut i) = (0i32, false, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'"' if in_str => in_str = false,
            b'"' => {
                if depth == 1
                    && let Some(rest) = line[i..].strip_prefix("\"id\"")
                    && let Some(v) = rest.trim_start().strip_prefix(':')
                {
                    let v = v.trim_start();
                    let n = v
                        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
                        .unwrap_or(v.len());
                    return v[..n].parse::<i64>().map_or(Value::Null, |n| json!(n));
                }
                in_str = true;
            }
            b'{' | b'[' if !in_str => depth += 1,
            b'}' | b']' if !in_str => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    Value::Null
}

/// Generated, tool and hidden folders the index leaves out, as `Project::open` does (Hilt_* and
/// *_Factory classes under build/ would count as implementations), the path itself included.
fn ignored(root: Option<&Path>, path: &Path) -> bool {
    let rel = root.and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path);
    crate::project::skipped(rel)
}

fn verdict(v: Verdict) -> &'static str {
    match v {
        Verdict::Stable => "stable",
        Verdict::Unstable => "unstable",
        Verdict::Runtime => "runtime",
        Verdict::Unknown => "unknown",
    }
}

impl Server {
    pub fn handle(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        let s = |k: &str| params[k].as_str().ok_or_else(|| format!("missing \"{k}\""));
        // the editor's unsaved text when sent, else the latest `update`, else the file on disk
        let texts = &self.texts;
        let text = |path: &Path| match params["text"].as_str() {
            Some(t) => Ok(t.to_string()),
            None => match texts.get(path) {
                Some(t) => Ok(t.clone()),
                None => std::fs::read_to_string(path).map_err(|e| e.to_string()),
            },
        };
        if !matches!(method, "ping" | "initialize" | "adb") && !self.open {
            return Err("call initialize first".into());
        }
        match method {
            "ping" => Ok(json!({ "version": env!("CARGO_PKG_VERSION") })),
            "initialize" => {
                let root = Path::new(s("root")?);
                if !root.is_dir() {
                    return Err(format!("{} is not a folder", root.display()));
                }
                let p = Project::open(root);
                self.index = Index::build(&p.files);
                self.texts.clear(); // texts of the old index: the editor sends its own again
                let dirs: Vec<_> = p.modules.iter().map(|m| m.dir.clone()).collect();
                self.reports = compose::load_reports(&dirs);
                self.project = Some(project_ref(p.root.clone(), p.files.clone()));
                self.files = p.files.iter().cloned().collect();
                self.files_changed = false;
                self.edited.clear();
                self.open = true;
                Ok(json!({
                    "files": p.files.len(),
                    "symbols": self.index.symbol_count(),
                    "modules": p.modules.len(),
                }))
            }
            "update" => {
                let raw = s("path")?;
                let path = &canon(raw);
                let root = self.project.as_ref().map(|p| p.root.as_path());
                if !in_project(root, path) {
                    return Err(format!("{raw:?} is not in the project"));
                }
                // what the walk of `initialize` leaves out: tool folders, `.gitignore`
                if ignored(root, path) || root.is_some_and(|r| crate::project::git_ignored(r, path))
                {
                    return Ok(Value::Null);
                }
                // a folder created, moved, renamed or replaced: what is in it now, and nothing of
                // what was (a replaced folder keeps no deleted file); the editor's unsaved text of
                // a file still there stays what the index reads
                if path.is_dir() {
                    let mut typed: HashMap<PathBuf, String> = self
                        .texts
                        .iter()
                        .filter(|(p, _)| p.starts_with(path))
                        .map(|(p, t)| (p.clone(), t.clone()))
                        .collect();
                    self.forget(path);
                    let root = self.project.as_ref().map(|p| p.root.clone());
                    for f in crate::project::walk(path) {
                        if !crate::project::is_code(&f) {
                            if root
                                .as_deref()
                                .is_some_and(|r| crate::project::accepts_in(r, &f))
                            {
                                self.edited.insert(f.clone());
                                self.files_changed |= self.files.insert(f);
                            }
                            continue;
                        }
                        let src = match typed.remove(&f) {
                            Some(text) => {
                                self.texts.insert(f.clone(), text.clone());
                                text
                            }
                            None => match std::fs::read_to_string(&f) {
                                Ok(src) => src,
                                Err(_) => continue,
                            },
                        };
                        self.index.update(&f, &src);
                        self.edited.insert(f.clone());
                        self.files_changed |= self.files.insert(f);
                    }
                    return Ok(Value::Null);
                }
                // a file the file list does not take (an image, a log): nothing to do
                if root.is_some_and(|r| !crate::project::accepts_in(r, path)) {
                    return Ok(Value::Null);
                }
                // a build or resource file: only the file list (the index reads Kotlin and Java)
                if !crate::project::is_code(path) {
                    if path.is_file() {
                        self.edited.insert(path.clone());
                        self.files_changed |= self.files.insert(path.clone());
                    } else {
                        self.forget(path);
                    }
                    return Ok(Value::Null);
                }
                let src = match params["text"].as_str() {
                    Some(t) => t.to_string(),
                    None => match std::fs::read_to_string(path) {
                        Ok(t) => t,
                        // deleted meanwhile: as `remove`
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.forget(path);
                            return Ok(Value::Null);
                        }
                        Err(e) => return Err(e.to_string()),
                    },
                };
                self.index.update(path, &src);
                if params["text"].is_string() {
                    self.texts.insert(path.clone(), src);
                } else {
                    self.texts.remove(path);
                }
                self.edited.insert(path.clone());
                self.files_changed |= self.files.insert(path.clone());
                Ok(Value::Null)
            }
            // a deleted file, or everything under a deleted folder (in the project: never all of
            // it by an empty path); `paths` removes many in one request
            "remove" => {
                let raws: Vec<&str> = match params["paths"].as_array() {
                    Some(a) => a
                        .iter()
                        .map(|p| p.as_str().ok_or("\"paths\" holds a non-text"))
                        .collect::<Result<_, _>>()?,
                    None => vec![s("path")?],
                };
                let root = self.project.as_ref().map(|p| p.root.clone());
                let mut outside = Vec::new();
                for raw in raws {
                    let path = &canon(raw);
                    // never the project folder itself: that would wipe the whole index
                    if raw.trim().is_empty()
                        || !in_project(root.as_deref(), path)
                        || root.as_deref() == Some(path.as_path())
                    {
                        outside.push(format!("{raw:?}"));
                    } else if !ignored(root.as_deref(), path) {
                        self.forget(path);
                    }
                }
                match outside.len() {
                    0 => Ok(Value::Null),
                    _ => Err(format!("{} is not in the project", outside.join(", "))),
                }
            }
            // Droid Lite's gutter links for one file: expect/actual, supertypes, implementations,
            // overrides
            "lenses" => {
                let path = &canon(s("path")?);
                let text = text(path)?;
                let syms = crate::kotlin::scan(&text);
                let lines = Lines::new(&text);
                let mut placer = Placer::new(texts);
                let mut out = Vec::new();
                // looked up once per file or class, not per declaration: a file may have 20k
                let expect_types = crate::index::expect_types(&syms);
                let mut overrides: HashMap<&str, HashMap<String, Hit>> = HashMap::new();
                let mut member_impls: HashMap<&str, HashMap<String, Vec<Hit>>> = HashMap::new();
                for d in &syms.decls {
                    let (line, _) = lines.position(&text, d.offset);
                    let role = crate::index::kmp_role_in(d, &expect_types);
                    let kmp = role
                        .map_or_else(Vec::new, |r| self.index.counterparts_as(path, &syms, d, r));
                    if !kmp.is_empty() {
                        let expect = role == Some("expect");
                        let mut sets: Vec<&str> = kmp
                            .iter()
                            .filter_map(|h| h.detail.split(" · ").next())
                            .collect();
                        sets.dedup();
                        let verb = if expect { "Actual" } else { "Expected" };
                        let title = format!("{verb} in {}", sets.join(", "));
                        out.push(placer.lens(line, title, &kmp));
                    }
                    if d.kind.is_type() {
                        let supers: Vec<Hit> = d
                            .supers
                            .iter()
                            .flat_map(|s| self.index.definitions(s, path))
                            .filter(|h| {
                                let k = |d: &crate::kotlin::Decl| d.offset == h.offset;
                                self.index
                                    .named(&h.name)
                                    .any(|(p, _, d)| *p == h.path && k(d) && d.kind.is_type())
                            })
                            .collect();
                        if !supers.is_empty() {
                            let title = format!("↑ {}", d.supers.join(", "));
                            out.push(placer.lens(line, title, &supers));
                        }
                        let impls = self.index.implementations(&d.name);
                        if !impls.is_empty() {
                            let (verb, what) =
                                match (d.kind == Kind::Interface, d.has_modifier("fun")) {
                                    (_, true) => ("Implemented in", ("place", "places")),
                                    (true, _) => ("Implemented in", ("class", "classes")),
                                    _ => ("Subclassed by", ("class", "classes")),
                                };
                            let n = plural(impls.len(), what.0, what.1);
                            out.push(placer.lens(line, format!("↓ {verb} {n}"), &impls));
                        }
                    } else if let Some(c) = &d.container
                        && matches!(d.kind, Kind::Fun | Kind::Property)
                    {
                        if d.has_modifier("override") {
                            let all = overrides
                                .entry(c)
                                .or_insert_with(|| self.index.overridden_in(c));
                            if let Some(h) = all.get(&d.name) {
                                let title = format!("↑ Overrides {}", h.name);
                                out.push(placer.lens(line, title, std::slice::from_ref(h)));
                            }
                        } else {
                            let all = member_impls
                                .entry(c)
                                .or_insert_with(|| self.index.member_implementations_in(c));
                            if let Some(impls) = all.get(&d.name) {
                                let n = plural(impls.len(), "class", "classes");
                                let title = format!("↓ Implemented in {n}");
                                out.push(placer.lens(line, title, impls));
                            }
                        }
                    }
                }
                Ok(Value::Array(out))
            }
            // classes implementing `name`; with `member`, that member's overrides in them
            "implementations" => {
                let name = s("name")?;
                let hits = match params["member"].as_str() {
                    Some(m) => self.index.member_implementations(name, m),
                    None => self.index.implementations(name),
                };
                let mut placer = Placer::new(texts);
                Ok(Value::Array(
                    hits.iter().map(|h| placer.hit_json(h)).collect(),
                ))
            }
            // each composable of a file with its parameters' stability (report, else a guess)
            "stability" => {
                let path = &canon(s("path")?);
                let text = text(path)?;
                let syms = crate::kotlin::scan(&text);
                let lines = Lines::new(&text);
                let out: Vec<Value> = syms
                    .decls
                    .iter()
                    .filter(|d| d.is_composable() && !d.is_preview() && !d.params.is_empty())
                    .map(|d| {
                        let st = self
                            .reports
                            .get(&d.name)
                            .cloned()
                            .unwrap_or_else(|| compose::guess(d, &self.index));
                        let (line, col) = lines.position(&text, d.offset);
                        // where each parameter's name is, for a badge on its line
                        let at: HashMap<String, std::ops::Range<usize>> =
                            compose::param_names(&text, d.offset).into_iter().collect();
                        json!({
                            "function": d.name,
                            "line": line,
                            "col": col,
                            "skippable": st.skippable,
                            "restartable": st.restartable,
                            "source": st.source,
                            "params": st.params.iter().map(|p| {
                                let mut v = json!({
                                    "name": p.name,
                                    "verdict": verdict(p.verdict),
                                    "reason": p.reason,
                                });
                                if let Some(r) = at.get(&p.name) {
                                    let (l, c) = lines.position(&text, r.start);
                                    let name = text.get(r.clone()).unwrap_or("");
                                    v["line"] = json!(l);
                                    v["col"] = json!(c);
                                    v["end"] = json!(c + name.encode_utf16().count());
                                }
                                v
                            }).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                Ok(Value::Array(out))
            }
            "adb" => Ok(json!(adb()?)),
            _ => Err(format!("unknown method {method}")),
        }
    }

    /// Drops a deleted file, or everything under a deleted folder: its index entries, texts and
    /// place in the file list. A known file costs a few lookups; a folder one range of the sorted
    /// lists, never a scan of every file (an editor may send thousands of removes at once).
    fn forget(&mut self, path: &Path) {
        if self.files.remove(path) || self.index.files.contains_key(path) {
            self.index.remove(path);
            self.texts.remove(path);
            self.files_changed = true;
            return;
        }
        self.index.remove_under(path);
        self.texts.retain(|p, _| !p.starts_with(path));
        let under: Vec<PathBuf> = self
            .files
            .range::<Path, _>((std::ops::Bound::Included(path), std::ops::Bound::Unbounded))
            .take_while(|p| p.starts_with(path))
            .cloned()
            .collect();
        self.files_changed |= !under.is_empty();
        for p in under {
            self.files.remove(&p);
        }
    }

    /// Starts drawing a file's previews. `Ok(Some)` answers now (no previews); `Ok(None)` means the
    /// answer comes later from the preview thread.
    pub fn previews(&mut self, id: &Value, params: &Value) -> Result<Option<Value>, String> {
        if !self.open {
            return Err("call initialize first".into());
        }
        let raw = params["path"].as_str().ok_or("missing \"path\"")?;
        let path = canon(raw);
        // as `update`: a file outside the project is never read nor drawn
        let root = self.project.as_ref().map(|p| p.root.as_path());
        if !in_project(root, &path) {
            return Err(format!("{raw:?} is not in the project"));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let syms = crate::kotlin::scan(&text);
        let previews: Vec<String> = syms
            .decls
            .iter()
            .filter(|d| d.is_preview() && d.container.is_none())
            .map(|d| d.name.clone())
            .collect();
        if previews.is_empty() {
            // an older request of this file still drawing: answered now, its pictures never sent
            let older = self
                .waiting
                .lock()
                .map_err(|e| e.to_string())?
                .remove(&path);
            if let Some((_, older)) = older {
                self.out
                    .emit(json!({ "id": older, "error": "replaced by a newer request" }));
            }
            return Ok(Some(json!({ "file": path, "cards": [] })));
        }
        let class = crate::device::preview_name(&syms.package, &path, None, "x")
            .trim_end_matches(".x")
            .to_string();
        let project = self.project_ref();
        let (waiting, out) = (self.waiting.clone(), self.out.clone());
        if self.previews.is_none() {
            let root = crate::preview::dir().join("vscode");
            sweep_png_dirs(&root);
            let dir = png_dir(&root);
            self.pngs = Some(dir.clone());
            let (tx, events) = crate::preview::spawn(&egui::Context::default(), false);
            std::thread::spawn(move || relay(events, waiting, out, dir));
            self.previews = Some(tx);
        }
        self.seq += 1;
        let older = self
            .waiting
            .lock()
            .map_err(|e| e.to_string())?
            .insert(path.clone(), (self.seq, id.clone()));
        if let Some((_, older)) = older {
            self.out
                .emit(json!({ "id": older, "error": "replaced by a newer request" }));
        }
        let request = crate::preview::Request {
            file: path.clone(),
            class,
            previews,
            seq: self.seq,
            project,
        };
        if let Some(client) = &self.previews
            && client.send(request).is_err()
        {
            // the preview thread ended (a bug): a new one starts at the next request
            self.previews = None;
            if let Ok(mut w) = self.waiting.lock() {
                w.remove(&path);
            }
            return Err("the preview thread stopped; open the preview again".into());
        }
        Ok(None)
    }

    /// The project for a preview request, its file list made again when files were added or
    /// removed since the last one (else the same list, not a copy), with the files edited since.
    fn project_ref(&mut self) -> Option<crate::preview::ProjectRef> {
        if std::mem::take(&mut self.files_changed)
            && let Some(p) = &mut self.project
        {
            *p = project_ref(p.root.clone(), self.files.iter().cloned().collect());
        }
        let mut p = self.project.clone()?;
        p.changed = std::mem::take(&mut self.edited).into_iter().collect();
        Some(p)
    }

    /// At the end of input: the device thread ends first (dropping a running recording stops
    /// screenrecord and deletes its file on the device; a video being copied stops copying and
    /// stays on the device), waited for at most `limit`; then this engine's pictures go.
    pub fn shutdown(&mut self, limit: Duration) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some((tx, t)) = self.devices.take() {
            drop(tx);
            let until = Instant::now() + limit;
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
        if let Some(d) = self.pngs.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Answers one request line now (`Some`), or later through the output (`None`: previews and
    /// device tools).
    pub fn request(&mut self, line: &str) -> Option<Value> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(json!({ "id": lenient_id(line), "error": format!("bad JSON: {e}") }));
            }
        };
        let (id, method, params) = (
            &req["id"],
            req["method"].as_str().unwrap_or(""),
            &req["params"],
        );
        if is_device_call(method) {
            let (out, stop) = (self.out.clone(), self.stopping.clone());
            let (devices, _) = self.devices.get_or_insert_with(|| device_thread(out, stop));
            let _ = devices.send((id.clone(), method.to_string(), params.clone()));
            return None;
        }
        if method == "previews" {
            return match guarded(|| self.previews(id, params)) {
                Ok(Some(result)) => Some(answer(id.clone(), Ok(result))),
                Ok(None) => None,
                Err(e) => Some(answer(id.clone(), Err(e))),
            };
        }
        Some(answer(id.clone(), guarded(|| self.handle(method, params))))
    }

    /// Answers one request line that answers at once.
    #[cfg(test)]
    pub fn reply(&mut self, line: &str) -> Value {
        self.request(line).unwrap_or(Value::Null)
    }
}

/// Reads requests from stdin until it closes.
pub fn run() {
    let mut server = Server::default();
    serve_input(&mut server, std::io::stdin().lock(), &emit);
    server.shutdown(Duration::from_secs(15));
}

/// Answers each request line of `input` until it ends. A line that is not UTF-8 is answered with
/// an error (to its id when one can be read) and the next line goes on: it never ends the engine.
fn serve_input(server: &mut Server, mut input: impl BufRead, out: &dyn Fn(&Value)) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match input.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            let lossy = String::from_utf8_lossy(&buf);
            out(
                &json!({ "id": lenient_id(&lossy), "error": "bad request: the line is not UTF-8 text" }),
            );
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(v) = server.request(line.trim_end_matches(['\n', '\r'])) {
            out(&v);
        }
    }
}

/// The project as a preview request carries it: its build scripts and its files.
fn project_ref(root: PathBuf, files: Vec<PathBuf>) -> crate::preview::ProjectRef {
    crate::preview::ProjectRef {
        root,
        scripts: files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
            .cloned()
            .collect(),
        files: Arc::new(files),
        changed: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("droid-lite-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("app/src/main/java/demo");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(d.join("settings.gradle.kts"), "include(\":app\")\n").unwrap();
        std::fs::write(
            src.join("Api.kt"),
            "package demo\n\ninterface Api {\n    fun load()\n}\n",
        )
        .unwrap();
        std::fs::write(
            src.join("RealApi.kt"),
            "package demo\n\ninternal class RealApi @Inject constructor(\n    private val x: Int,\n) : Api {\n    override fun load() {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            src.join("Card.kt"),
            "package demo\n\n@Composable\nfun Card(names: MutableList<String>, title: String) {}\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn answers_over_json_lines() {
        let root = project("main");
        let mut s = Server::default();
        assert_eq!(
            s.previews(&json!(1), &json!({ "path": "x" })),
            Err("call initialize first".into())
        );
        // device tools need no project; they answer from their own thread
        let (tx, rx) = channel();
        s.out = Out(Some(tx));
        assert_eq!(s.request(r#"{"id":0,"method":"record_stop"}"#), None);
        let later = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(
            later,
            json!({ "id": 0, "error": "no recording is running" })
        );
        assert_eq!(
            s.reply(r#"{"id":1,"method":"implementations","params":{"name":"Api"}}"#)["error"],
            "call initialize first"
        );
        let init = s.reply(&format!(
            r#"{{"id":2,"method":"initialize","params":{{"root":{:?}}}}}"#,
            root.to_str().unwrap()
        ));
        assert_eq!(init["id"], 2);
        // Api.kt, RealApi.kt, Card.kt and settings.gradle.kts
        assert_eq!(init["result"]["files"], 4, "{init}");
        let imp = s.reply(r#"{"id":3,"method":"implementations","params":{"name":"Api"}}"#);
        assert_eq!(imp["result"][0]["name"], "RealApi", "{imp}");
        assert_eq!(imp["result"][0]["line"], 2);
        let m = s.reply(
            r#"{"id":4,"method":"implementations","params":{"name":"Api","member":"load"}}"#,
        );
        assert_eq!(m["result"][0]["line"], 5, "{m}");
        let card = root.join("app/src/main/java/demo/Card.kt");
        let st = s.reply(&format!(
            r#"{{"id":5,"method":"stability","params":{{"path":{:?}}}}}"#,
            card.to_str().unwrap()
        ));
        let f = &st["result"][0];
        assert_eq!(
            (f["function"].as_str(), f["line"].as_u64()),
            (Some("Card"), Some(3)),
            "{st}"
        );
        assert_eq!(f["params"][0]["verdict"], "unstable", "MutableList: {st}");
        // `fun Card(names: …` on line 3: `names` starts at column 9
        assert_eq!(
            (
                &f["params"][0]["line"],
                &f["params"][0]["col"],
                &f["params"][0]["end"]
            ),
            (&json!(3), &json!(9), &json!(14)),
            "{st}"
        );
        assert_eq!(f["params"][1]["verdict"], "stable", "{st}");
        assert!(
            s.reply("not json")["error"]
                .as_str()
                .unwrap()
                .starts_with("bad JSON")
        );
        assert_eq!(
            s.reply(r#"{"id":6,"method":"nope"}"#)["error"],
            "unknown method nope"
        );
        let lenses = |s: &mut Server, file: &str| {
            let p = root.join("app/src/main/java/demo").join(file);
            let r = s.reply(&format!(
                r#"{{"id":7,"method":"lenses","params":{{"path":{:?}}}}}"#,
                p.to_str().unwrap()
            ));
            r["result"]
                .as_array()
                .unwrap_or_else(|| panic!("{r}"))
                .iter()
                .map(|l| {
                    (
                        l["line"].as_u64().unwrap(),
                        l["title"].as_str().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lenses(&mut s, "Api.kt"),
            [
                (2, "↓ Implemented in 1 class".to_string()),
                (3, "↓ Implemented in 1 class".to_string())
            ]
        );
        assert_eq!(
            lenses(&mut s, "RealApi.kt"),
            [
                (2, "↑ Api".to_string()),
                (5, "↑ Overrides Api.load".to_string())
            ]
        );
        // the editor's path may go through a symlink (/var vs /private/var on macOS): the
        // same file, not a second copy in the index
        let real = root.join("app/src/main/java/demo/RealApi.kt");
        let r = s.reply(&format!(
            r#"{{"id":9,"method":"update","params":{{"path":{:?}}}}}"#,
            real.to_str().unwrap()
        ));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(lenses(&mut s, "Api.kt")[0].1, "↓ Implemented in 1 class");
        // a file without @Preview answers at once, with no cards
        let api = root.join("app/src/main/java/demo/Api.kt");
        let r = s.previews(&json!(10), &json!({ "path": api }));
        assert_eq!(r.unwrap().unwrap()["cards"], json!([]));
        // unsaved editor text: a second class implementing Api
        let other = root.join("app/src/main/java/demo/Other.kt");
        let r = s.reply(&format!(
            r#"{{"id":8,"method":"update","params":{{"path":{:?},"text":"package demo\nclass Other : Api {{ override fun load() {{}} }}\n"}}}}"#,
            other.to_str().unwrap()
        ));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(lenses(&mut s, "Api.kt")[0].1, "↓ Implemented in 2 classes");
        let _ = std::fs::remove_dir_all(root);
    }

    fn init(s: &mut Server, root: &Path) {
        let r = s.reply(&format!(
            r#"{{"id":1,"method":"initialize","params":{{"root":{:?}}}}}"#,
            root.to_str().unwrap()
        ));
        assert!(r["error"].is_null(), "{r}");
    }

    fn call(s: &mut Server, method: &str, params: Value) -> Value {
        s.reply(&json!({ "id": 1, "method": method, "params": params }).to_string())
    }

    #[test]
    fn update_of_a_replaced_folder_forgets_the_files_it_no_longer_has() {
        let root = std::fs::canonicalize(project("replaced")).unwrap();
        let feature = root.join("app/src/main/java/demo/feature");
        std::fs::create_dir_all(feature.join("res")).unwrap();
        std::fs::write(feature.join("Old.kt"), "package demo\nclass Old : Api\n").unwrap();
        std::fs::write(feature.join("res/old.xml"), "<resources/>\n").unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        // the folder replaced (a branch switch): Old.kt and old.xml gone, New.kt and new.xml there
        std::fs::remove_dir_all(&feature).unwrap();
        std::fs::create_dir_all(feature.join("res")).unwrap();
        std::fs::write(feature.join("New.kt"), "package demo\nclass New : Api\n").unwrap();
        std::fs::write(feature.join("res/new.xml"), "<resources/>\n").unwrap();
        let r = call(&mut s, "update", json!({ "path": feature }));
        assert!(r["error"].is_null(), "{r}");
        let imp = call(&mut s, "implementations", json!({ "name": "Api" }));
        let mut names: Vec<&str> = imp["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["New", "RealApi"], "{imp}");
        assert!(!s.files.contains(&feature.join("Old.kt")));
        assert!(!s.files.contains(&feature.join("res/old.xml")));
        assert!(s.files.contains(&feature.join("New.kt")));
        assert!(
            s.files.contains(&feature.join("res/new.xml")),
            "a resource file too"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_hit_in_unsaved_text_is_placed_in_that_text_not_the_file_on_disk() {
        let root = project("unsaved");
        // on disk: two-byte characters where the editor's text has the class
        let b = root.join("app/src/main/java/demo/B.kt");
        std::fs::write(&b, "// éééééééééééééééééééé\n").unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let text = "//a\nclass B : Api { override fun load() {} }\n";
        let r = call(&mut s, "update", json!({ "path": b, "text": text }));
        assert!(r["error"].is_null(), "{r}");
        let imp = call(&mut s, "implementations", json!({ "name": "Api" }));
        let hit = imp["result"]
            .as_array()
            .unwrap_or_else(|| panic!("{imp}"))
            .iter()
            .find(|h| h["name"] == "B")
            .cloned()
            .unwrap_or_else(|| panic!("{imp}"));
        assert_eq!(hit["line"], 1, "{hit}");
        // the member's override is placed in the same text
        let m = call(
            &mut s,
            "implementations",
            json!({ "name": "Api", "member": "load" }),
        );
        assert!(
            m["result"]
                .as_array()
                .is_some_and(|a| a.iter().any(|h| h["line"] == 1)),
            "{m}"
        );
        // once the file is removed its text is gone too
        let r = call(&mut s, "remove", json!({ "path": b }));
        assert!(r["error"].is_null(), "{r}");
        assert!(s.texts.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_offset_inside_a_character_or_past_the_end_never_panics() {
        assert_eq!(position("é", 1), (0, 0));
        assert_eq!(position("ab\ncd", 99), (1, 2));
    }

    #[test]
    fn a_panic_answers_an_error_and_the_engine_goes_on() {
        let r: Result<(), String> = guarded(|| panic!("boom"));
        assert_eq!(r, Err("internal error: boom".into()));
    }

    #[test]
    fn columns_are_utf16_code_units_like_vs_code() {
        // 😀 is one character, two UTF-16 units, four bytes
        assert_eq!(position("😀x", 4), (0, 2));
        let root = project("utf16");
        let mut s = Server::default();
        init(&mut s, &root);
        let card = root.join("app/src/main/java/demo/Card.kt");
        let text = "package demo\n\n@Composable\nfun Card(t: String = \"😀\", names: MutableList<String>) {}\n";
        let st = call(&mut s, "stability", json!({ "path": card, "text": text }));
        let p = &st["result"][0]["params"][1];
        assert_eq!(p["name"], "names", "{st}");
        // `fun Card(t: String = "😀", ` is 27 UTF-16 units (26 characters)
        assert_eq!(
            (&p["line"], &p["col"], &p["end"]),
            (&json!(3), &json!(27), &json!(32)),
            "{st}"
        );
        // a name with a letter outside the first plane (𝑥: one character, two UTF-16 units)
        let text = "package demo\n\n@Composable\nfun Card(𝑥s: MutableList<String>) {}\n";
        let st = call(&mut s, "stability", json!({ "path": card, "text": text }));
        let p = &st["result"][0]["params"][0];
        assert_eq!((&p["col"], &p["end"]), (&json!(9), &json!(12)), "{st}");
    }

    #[test]
    fn an_older_requests_pictures_never_answer_a_newer_request() {
        use crate::preview::Event;
        let dir = std::env::temp_dir().join(format!("dl-serve-ev-{}", std::process::id()));
        let waiting: Waiting = Default::default();
        let f = PathBuf::from("/p/Card.kt");
        // id 10 at V1 (seq 1) was replaced by id 11 at V2 (seq 2)
        waiting.lock().unwrap().insert(f.clone(), (2, json!(11)));
        let done = |seq| Event::Done {
            file: f.clone(),
            seq,
            cards: vec![],
        };
        assert_eq!(on_event(done(1), &waiting, &dir), None, "V1's cards");
        let v = on_event(done(2), &waiting, &dir).unwrap();
        assert_eq!(v["id"], 11);
        assert!(waiting.lock().unwrap().is_empty());
        // a request the preview thread skipped is answered with the reason
        waiting.lock().unwrap().insert(f.clone(), (3, json!(12)));
        let skipped = Event::Skipped {
            file: f.clone(),
            seq: 3,
            reason: "replaced by a newer request".into(),
        };
        assert_eq!(
            on_event(skipped, &waiting, &dir),
            Some(json!({ "id": 12, "error": "replaced by a newer request" }))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_relay_blocks_on_events_and_ends_with_the_client() {
        let (tx, events) = channel();
        let (out, lines) = channel();
        let dir = std::env::temp_dir().join(format!("dl-serve-relay-{}", std::process::id()));
        let t = std::thread::spawn(move || relay(events, Default::default(), Out(Some(out)), dir));
        tx.send(crate::preview::Event::Status(
            PathBuf::from("/p/Card.kt"),
            "Drawing".into(),
            Some((1, 2)),
        ))
        .unwrap();
        let v = lines
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!((&v["text"], &v["done"]), (&json!("Drawing"), &json!(1)));
        assert_eq!(
            v["file"], "/p/Card.kt",
            "the status names its request's file: {v}"
        );
        drop(tx);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !t.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            t.is_finished(),
            "the relay ends when the preview client does"
        );
    }

    #[test]
    fn a_device_that_hangs_holds_up_no_other_request() {
        let _lock = crate::app::test_lock();
        let dir = std::env::temp_dir().join(format!("dl-serve-adb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let adb = dir.join("adb");
        std::fs::write(&adb, "#!/bin/sh\nsleep 2\n").unwrap();
        std::fs::set_permissions(&adb, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let old = std::env::var_os("DROID_LITE_ADB");
        // SAFETY: the test lock is held, so no other test reads the environment meanwhile.
        unsafe { std::env::set_var("DROID_LITE_ADB", &adb) };
        let root = project("device");
        let mut s = Server::default();
        let (tx, rx) = channel();
        s.out = Out(Some(tx));
        init(&mut s, &root);
        let t = std::time::Instant::now();
        assert_eq!(s.request(r#"{"id":5,"method":"devices"}"#), None);
        let imp = call(&mut s, "implementations", json!({ "name": "Api" }));
        assert_eq!(imp["result"][0]["name"], "RealApi");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            t.elapsed()
        );
        let later = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(later, json!({ "id": 5, "result": [] }));
        match old {
            Some(v) => unsafe { std::env::set_var("DROID_LITE_ADB", v) },
            None => unsafe { std::env::remove_var("DROID_LITE_ADB") },
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn build_and_tool_folders_stay_out_and_remove_drops_a_folder() {
        let root = std::fs::canonicalize(project("ignored")).unwrap();
        for f in ["RealApi.kt", "Card.kt"] {
            std::fs::remove_file(root.join("app/src/main/java/demo").join(f)).unwrap();
        }
        let mut s = Server::default();
        init(&mut s, &root);
        let count = |s: &mut Server| {
            let r = call(s, "implementations", json!({ "name": "Api" }));
            r["result"].as_array().map_or(0, Vec::len)
        };
        let gen_dir = root.join("app/build/generated/hilt");
        for (i, dir) in [
            &gen_dir,
            &root.join(".gradle"),
            &root.join(".idea"),
            &root.join(".git"),
        ]
        .into_iter()
        .enumerate()
        {
            let f = dir.join(format!("Hilt_{i}.kt"));
            let text = format!("package demo\nclass Hilt_{i} : Api\n");
            let r = call(&mut s, "update", json!({ "path": f, "text": text }));
            assert!(r["error"].is_null(), "{r}");
        }
        assert_eq!(
            count(&mut s),
            0,
            "generated classes are not implementations"
        );
        // a folder: everything under it goes, with the editor's texts
        let feature = root.join("app/src/main/java/demo/feature");
        std::fs::create_dir_all(&feature).unwrap();
        for n in ["One", "Two"] {
            let text = format!("package demo\nclass {n} : Api\n");
            let f = feature.join(format!("{n}.kt"));
            std::fs::write(&f, &text).unwrap();
            call(&mut s, "update", json!({ "path": f, "text": text }));
        }
        assert_eq!(count(&mut s), 2);
        std::fs::remove_dir_all(&feature).unwrap();
        let r = call(&mut s, "remove", json!({ "path": feature }));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(count(&mut s), 0);
        assert!(s.texts.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saved_pictures_older_than_the_cache_keeps_are_deleted() {
        let dir = std::env::temp_dir().join(format!("dl-serve-png-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (old, fresh) = (
            dir.join("0123456789abcdef-1-0.png"),
            dir.join("fedcba9876543210-2-0.png"),
        );
        for p in [&old, &fresh] {
            std::fs::write(p, "png").unwrap();
        }
        let ttl = std::time::Duration::from_secs(crate::preview_cache::TTL_SECS + 60);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - ttl).unwrap();
        save_cards(&dir, Path::new("/p/Other.kt"), vec![]);
        assert!(!old.exists(), "older than the TTL");
        assert!(fresh.exists(), "another file's recent picture stays");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_previews_file_list_follows_files_added_and_removed() {
        let root = std::fs::canonicalize(project("files")).unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let first = s.project_ref().unwrap().files;
        assert!(
            Arc::ptr_eq(&first, &s.project_ref().unwrap().files),
            "unchanged: the same list, not a copy"
        );
        let new = root.join("app/src/main/java/demo/screen/New.kt");
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, "package demo.screen\nclass New\n").unwrap();
        let r = call(&mut s, "update", json!({ "path": new }));
        assert!(r["error"].is_null(), "{r}");
        assert!(s.project_ref().unwrap().files.contains(&new));
        let r = call(&mut s, "remove", json!({ "path": new.parent().unwrap() }));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(s.project_ref().unwrap().files, first);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn files_updated_since_the_last_preview_go_with_the_next_one_once() {
        let root = std::fs::canonicalize(project("edited")).unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let api = root.join("app/src/main/java/demo/Api.kt");
        let card = root.join("app/src/main/java/demo/Card.kt");
        assert!(s.project_ref().unwrap().changed.is_empty());
        // unsaved text and a save on disk both count; the file list stays the same
        let r = call(
            &mut s,
            "update",
            json!({ "path": api, "text": "package demo\ndata class Item(val n: Int)\n" }),
        );
        assert!(r["error"].is_null(), "{r}");
        let r = call(&mut s, "update", json!({ "path": card }));
        assert!(r["error"].is_null(), "{r}");
        let p = s.project_ref().unwrap();
        assert_eq!(p.changed, [api, card]);
        assert!(s.project_ref().unwrap().changed.is_empty(), "sent once");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn update_follows_gitignore_and_takes_only_code_into_the_index() {
        let root = std::fs::canonicalize(project("gitignore")).unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n*.gen.kt\n").unwrap();
        std::fs::write(root.join(".git/info/exclude"), "excluded.kt\n").unwrap();
        let demo = root.join("app/src/main/java/demo");
        std::fs::write(demo.join(".gitignore"), "local/\n!Kept.gen.kt\n").unwrap();
        let files = [
            demo.join("generated/G.kt"),
            demo.join("Model.gen.kt"),
            demo.join("excluded.kt"),
            demo.join("local/L.kt"),
            demo.join("Kept.gen.kt"),
            demo.join("Ok.kt"),
        ];
        for f in &files {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            let n = f
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split('.')
                .next()
                .unwrap()
                .to_string();
            std::fs::write(f, format!("package demo\nclass {n} : Base\n")).unwrap();
        }
        let mut s = Server::default();
        init(&mut s, &root);
        let subclasses = |s: &mut Server| {
            let r = call(s, "implementations", json!({ "name": "Base" }));
            let mut v: Vec<String> = r["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["name"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(subclasses(&mut s), ["Kept", "Ok"], "Project::open");
        for f in &files {
            call(&mut s, "remove", json!({ "path": f }));
        }
        for f in files
            .iter()
            .chain([&demo.join("generated"), &demo.join("local")])
        {
            let r = call(&mut s, "update", json!({ "path": f }));
            assert!(r["error"].is_null(), "{f:?}: {r}");
            let r = call(
                &mut s,
                "update",
                json!({ "path": f, "text": "package demo\nclass Typed : Base\n" }),
            );
            assert!(r["error"].is_null(), "{f:?}: {r}");
        }
        assert_eq!(
            subclasses(&mut s),
            ["Typed", "Typed"],
            "only Kept.gen.kt and Ok.kt took the typed text"
        );
        assert_eq!(s.texts.len(), 2, "{:?}", s.texts.keys());
        // not code: a build file joins the file list (the previews read its namespace), never the index
        let (notes, gradle, png) = (
            root.join("notes.md"),
            root.join("app/build.gradle"),
            root.join("app/shot.png"),
        );
        for (f, t) in [
            (&notes, "class Notes : Base"),
            (&gradle, "android { namespace 'demo' }"),
            (&png, "x"),
        ] {
            std::fs::write(f, t).unwrap();
            let r = call(
                &mut s,
                "update",
                json!({ "path": f, "text": "class Fake : Base" }),
            );
            assert!(r["error"].is_null(), "{r}");
        }
        assert_eq!(subclasses(&mut s), ["Typed", "Typed"]);
        assert!(s.files.contains(&gradle) && s.files.contains(&notes) && !s.files.contains(&png));
        assert_eq!(s.texts.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_line_that_is_not_json_is_answered_with_its_id() {
        let mut s = Server::default();
        // a lone surrogate: not valid JSON
        let r = s.reply(r#"{"params":{"id":5,"text":"\ud800"},"id":42,"method":"update"}"#);
        assert_eq!(r["id"], 42, "{r}");
        assert!(r["error"].as_str().unwrap().starts_with("bad JSON"), "{r}");
        assert_eq!(
            lenient_id(r#"{"method":"id", "id" : 7 ,"x":"\ud800"}"#),
            json!(7)
        );
        assert_eq!(lenient_id(r#"{"text":"\"id\":3"}"#), Value::Null);
        assert_eq!(lenient_id("not json"), Value::Null);
    }

    #[test]
    fn at_the_end_of_input_a_running_recording_is_stopped_and_its_file_deleted() {
        let _lock = crate::app::test_lock();
        let dir = std::env::temp_dir().join(format!("dl-serve-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (adb, log) = (dir.join("adb"), dir.join("adb.log"));
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncase \"$*\" in *screenrecord*) echo 4242; exec sleep 30;; esac\n",
            log.display()
        );
        std::fs::write(&adb, script).unwrap();
        std::fs::set_permissions(&adb, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let old = std::env::var_os("DROID_LITE_ADB");
        // SAFETY: the test lock is held, so no other test reads the environment meanwhile.
        unsafe { std::env::set_var("DROID_LITE_ADB", &adb) };
        let mut s = Server::default();
        let (tx, rx) = channel();
        s.out = Out(Some(tx));
        let start = r#"{"id":1,"method":"record_start","params":{"serial":"emu"}}"#;
        assert_eq!(s.request(start), None);
        let started = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(started, json!({ "id": 1, "result": null }));
        s.shutdown(Duration::from_secs(15));
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("shell rm -f /sdcard/droid-lite-"),
            "the device's file is deleted before the engine exits: {calls}"
        );
        match old {
            Some(v) => unsafe { std::env::set_var("DROID_LITE_ADB", v) },
            None => unsafe { std::env::remove_var("DROID_LITE_ADB") },
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A fake adb for the device thread's tests: every call goes to `adb.log`; `pull` writes part of
    /// the video and hangs, `screencap` takes a second.
    fn slow_adb(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let adb = dir.join("adb");
        let d = dir.display();
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {d}/adb.log\ncase \"$*\" in\n\
             *screenrecord*) echo 4242; sleep 0.2;;\n\
             *stat*) echo 1000;;\n\
             *pull*) head -c 500 /dev/zero > \"$5\"; echo $$ > {d}/pull.pid; exec sleep 30;;\n\
             *screencap*) sleep 1; printf png;;\n\
             *devices*) printf 'List of devices attached\\nemu device model:Pixel\\n';;\n\
             esac\n"
        );
        std::fs::write(&adb, script).unwrap();
        std::fs::set_permissions(&adb, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        adb
    }

    fn with_env<T>(vars: &[(&str, &Path)], f: impl FnOnce() -> T) -> T {
        let old: Vec<_> = vars.iter().map(|(k, _)| std::env::var_os(k)).collect();
        // SAFETY: the caller holds the test lock, so no other test reads the environment meanwhile.
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        let r = f();
        for ((k, _), v) in vars.iter().zip(old) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        r
    }

    #[test]
    fn at_the_end_of_input_a_video_being_copied_stops_copying_and_stays_on_the_device() {
        let _lock = crate::app::test_lock();
        let dir = std::env::temp_dir().join(format!("dl-serve-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let adb = slow_adb(&dir);
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        with_env(
            &[
                ("DROID_LITE_ADB", &adb),
                ("DROID_LITE_CAPTURE_DIR", &out_dir),
            ],
            || {
                let mut s = Server::default();
                let (tx, rx) = channel();
                s.out = Out(Some(tx));
                assert_eq!(
                    s.request(r#"{"id":1,"method":"record_start","params":{"serial":"emu"}}"#),
                    None
                );
                assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap()["id"], 1);
                assert_eq!(s.request(r#"{"id":2,"method":"record_stop"}"#), None);
                let until = Instant::now() + Duration::from_secs(10);
                while !dir.join("pull.pid").exists() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                }
                std::thread::sleep(Duration::from_millis(100));
                let pull: i32 = std::fs::read_to_string(dir.join("pull.pid"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert_eq!(
                    std::fs::read_dir(&out_dir).unwrap().count(),
                    1,
                    "the .part being copied"
                );
                let t = Instant::now();
                s.shutdown(Duration::from_secs(15));
                assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
                let answer = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(answer["id"], 2);
                let e = answer["error"].as_str().unwrap();
                assert!(e.contains("Droid Lite is closing. The video is still on the device: /sdcard/droid-lite-"), "{e}");
                assert_eq!(
                    std::fs::read_dir(&out_dir).unwrap().count(),
                    0,
                    "no .part left"
                );
                assert_ne!(
                    unsafe { libc::kill(pull, 0) },
                    0,
                    "no adb pull outlives the engine"
                );
                assert!(
                    !std::fs::read_to_string(dir.join("adb.log"))
                        .unwrap()
                        .contains("rm -f"),
                    "the device keeps its copy"
                );
            },
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_newest_of_queued_devices_calls_runs_and_answers_them_all() {
        let _lock = crate::app::test_lock();
        let dir = std::env::temp_dir().join(format!("dl-serve-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let adb = slow_adb(&dir);
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        with_env(
            &[
                ("DROID_LITE_ADB", &adb),
                ("DROID_LITE_CAPTURE_DIR", &out_dir),
            ],
            || {
                let mut s = Server::default();
                let (tx, rx) = channel();
                s.out = Out(Some(tx));
                // a long call first, then three `devices` behind it
                assert_eq!(
                    s.request(r#"{"id":1,"method":"screenshot","params":{"serial":"emu"}}"#),
                    None
                );
                for id in 2..=4 {
                    assert_eq!(
                        s.request(&format!(r#"{{"id":{id},"method":"devices"}}"#)),
                        None
                    );
                }
                let mut answers: Vec<Value> = (0..4)
                    .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
                    .collect();
                answers.sort_by_key(|a| a["id"].as_i64());
                for a in &answers[1..] {
                    assert_eq!(a["result"][0]["serial"], "emu", "{a}");
                }
                let log = std::fs::read_to_string(dir.join("adb.log")).unwrap();
                assert_eq!(log.matches("devices").count(), 1, "{log}");
                s.shutdown(Duration::from_secs(5));
            },
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_update_keeps_the_editors_unsaved_text_of_files_still_there() {
        let root = std::fs::canonicalize(project("folder-texts")).unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let names = |s: &mut Server| {
            let r = call(s, "implementations", json!({ "name": "Api" }));
            let mut v: Vec<String> = r["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["name"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        let dir = root.join("app/src/main/java/demo/feature");
        std::fs::create_dir_all(&dir).unwrap();
        let (kept, gone) = (dir.join("Kept.kt"), dir.join("Gone.kt"));
        for f in [&kept, &gone] {
            std::fs::write(f, "package demo\nclass OnDisk : Api\n").unwrap();
            let text = "package demo\nclass Typed : Api\n";
            call(&mut s, "update", json!({ "path": f, "text": text }));
        }
        // a branch switch: Gone.kt is deleted, then the editor reports the folder
        std::fs::remove_file(&gone).unwrap();
        let r = call(&mut s, "update", json!({ "path": dir }));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(
            names(&mut s),
            ["RealApi", "Typed"],
            "Kept.kt as typed, Gone.kt gone"
        );
        assert_eq!(
            s.texts.keys().collect::<Vec<_>>(),
            [&kept],
            "the deleted file's text goes"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_folder_moved_into_the_project_is_indexed_again() {
        let root = std::fs::canonicalize(project("folder")).unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let count = |s: &mut Server| {
            let r = call(s, "implementations", json!({ "name": "Api" }));
            r["result"].as_array().map_or(0, Vec::len)
        };
        assert_eq!(count(&mut s), 1);
        // a renamed folder: the editor reports the new folder, nothing for its files
        let moved = root.join("app/src/main/java/demo/moved");
        std::fs::create_dir_all(moved.join("inner")).unwrap();
        std::fs::create_dir_all(moved.join("build")).unwrap();
        for (f, n) in [
            ("One.kt", "One"),
            ("inner/Two.kt", "Two"),
            ("build/Gen.kt", "Gen"),
        ] {
            let text = format!("package demo\nclass {n} : Api\n");
            std::fs::write(moved.join(f), text).unwrap();
        }
        let r = call(&mut s, "update", json!({ "path": moved }));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(
            count(&mut s),
            3,
            "RealApi, One and Two; not the build folder's"
        );
        assert!(
            s.project_ref()
                .unwrap()
                .files
                .contains(&moved.join("inner/Two.kt"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_preview_thread_that_ended_answers_an_error_and_starts_again_next_time() {
        let root = project("dead");
        let mut s = Server::default();
        init(&mut s, &root);
        let shot = root.join("app/src/main/java/demo/Shot.kt");
        std::fs::write(
            &shot,
            "package demo\n\n@Preview\n@Composable\nfun ShotPreview() {}\n",
        )
        .unwrap();
        let (tx, rx) = channel();
        drop(rx); // the preview thread panicked and ended
        s.previews = Some(tx);
        let r = s.previews(&json!(3), &json!({ "path": shot }));
        assert!(r.is_err(), "{r:?}");
        assert!(
            s.previews.is_none(),
            "a new preview thread at the next request"
        );
        assert!(s.waiting.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lenses_of_a_large_file_take_no_quadratic_time() {
        let root = project("large");
        let mut s = Server::default();
        init(&mut s, &root);
        // 1 MB, 24 000 declarations
        let text: String = (0..24_000)
            .map(|i| format!("fun f{i:05}() {{}} // ..........................\n"))
            .collect();
        assert!(text.len() > 1_000_000);
        let big = root.join("app/src/main/java/demo/Big.kt");
        let t = Instant::now();
        let r = call(&mut s, "lenses", json!({ "path": big, "text": text }));
        assert!(r["result"].is_array(), "{r}");
        assert!(
            t.elapsed() < Duration::from_millis(1000),
            "{:?}",
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn each_target_file_is_read_once_per_request() {
        let dir = std::env::temp_dir().join(format!("dl-serve-placer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("Impl.kt");
        std::fs::write(&f, "package demo\n\nclass A : Api\nclass B : Api\n").unwrap();
        let hit = |offset| Hit {
            name: "A".into(),
            detail: String::new(),
            path: f.clone(),
            offset,
            score: 0,
        };
        let texts = HashMap::new();
        let mut placer = Placer::new(&texts);
        assert_eq!(placer.hit_json(&hit(14))["line"], 2);
        assert_eq!(placer.hit_json(&hit(28))["line"], 3);
        assert_eq!(placer.disk_reads, 1);
        // the editor's unsaved text when there is one, never the disk
        let texts = HashMap::from([(f.clone(), "\n\n\n\n\nclass A : Api\n".to_string())]);
        let mut placer = Placer::new(&texts);
        assert_eq!(placer.hit_json(&hit(5))["line"], 5);
        assert_eq!(placer.disk_reads, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_never_wipes_the_index_by_an_empty_or_outside_path() {
        let root = project("remove");
        let mut s = Server::default();
        init(&mut s, &root);
        let root_s = root.to_str().unwrap().to_string();
        let slash = format!("{root_s}/");
        for p in ["", " ", "/", "/tmp/elsewhere", &root_s, &slash] {
            let r = call(&mut s, "remove", json!({ "path": p }));
            assert!(r["error"].is_string(), "{p:?}: {r}");
        }
        let r = call(&mut s, "remove", json!({ "paths": [root_s] }));
        assert!(r["error"].is_string(), "{r}");
        let r = call(&mut s, "implementations", json!({ "name": "Api" }));
        assert_eq!(r["result"][0]["name"], "RealApi", "{r}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn update_never_indexes_a_file_or_folder_outside_the_project() {
        let root = project("outside");
        let other = project("outside-other"); // a folder with an Api implementation of its own
        let mut s = Server::default();
        init(&mut s, &root);
        let real = other.join("app/src/main/java/demo/RealApi.kt");
        let text = "package demo\nclass Sneaky : Api\n";
        let up = root
            .join("app/missing/../../..")
            .join(other.file_name().unwrap());
        for (path, text) in [
            (other.join("app/src/main/java/demo"), None),
            (real.clone(), None),
            (real.clone(), Some(text)),
            (PathBuf::from("/"), None),
            (up.join("app/src/main/java/demo/X.kt"), Some(text)),
        ] {
            let mut params = json!({ "path": path });
            if let Some(t) = text {
                params["text"] = json!(t);
            }
            let r = call(&mut s, "update", params);
            assert!(r["error"].is_string(), "{path:?}: {r}");
        }
        let r = call(&mut s, "implementations", json!({ "name": "Api" }));
        assert_eq!(r["result"].as_array().map(Vec::len), Some(1), "{r}");
        assert!(s.texts.is_empty());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn previews_never_read_a_file_outside_the_project() {
        let root = project("prevout");
        let other = project("prevout-other");
        let mut s = Server::default();
        init(&mut s, &root);
        let f = other.join("app/src/main/java/demo/RealApi.kt");
        let up = root
            .join("app/missing/../../..")
            .join(other.file_name().unwrap());
        for path in [f.clone(), up.join("app/src/main/java/demo/RealApi.kt")] {
            let r = call(&mut s, "previews", json!({ "path": path }));
            assert!(
                r["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("not in the project")),
                "{path:?}: {r}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn a_file_gone_before_its_update_is_dropped_like_remove() {
        let root = std::fs::canonicalize(project("gone")).unwrap();
        let mut s = Server::default();
        init(&mut s, &root);
        let f = root.join("app/src/main/java/demo/Other.kt");
        let text = "package demo\nclass Other : Api\n";
        std::fs::write(&f, text).unwrap();
        call(&mut s, "update", json!({ "path": f, "text": text }));
        assert_eq!(s.texts.len(), 1);
        std::fs::remove_file(&f).unwrap();
        let r = call(&mut s, "update", json!({ "path": f }));
        assert!(r["error"].is_null(), "{r}");
        let r = call(&mut s, "implementations", json!({ "name": "Api" }));
        assert_eq!(r["result"].as_array().map(Vec::len), Some(1), "{r}");
        assert!(s.texts.is_empty() && !s.files.contains(&f));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_again_drops_the_editors_texts() {
        let root = project("reinit");
        let mut s = Server::default();
        init(&mut s, &root);
        let f = root.join("app/src/main/java/demo/Api.kt");
        call(
            &mut s,
            "update",
            json!({ "path": f, "text": "package demo\n" }),
        );
        assert_eq!(s.texts.len(), 1);
        init(&mut s, &root);
        assert!(s.texts.is_empty(), "they do not match the index made again");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_left_with_no_previews_answers_its_older_request_replaced() {
        let root = project("nopreview");
        let mut s = Server::default();
        let (tx, rx) = channel();
        s.out = Out(Some(tx));
        init(&mut s, &root);
        let card = canon(
            root.join("app/src/main/java/demo/Card.kt")
                .to_str()
                .unwrap(),
        );
        // request 7 is still drawing the file's last @Preview, which was then deleted
        s.waiting
            .lock()
            .unwrap()
            .insert(card.clone(), (1, json!(7)));
        let r = s.previews(&json!(8), &json!({ "path": card })).unwrap();
        assert_eq!(r.unwrap()["cards"], json!([]));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            json!({ "id": 7, "error": "replaced by a newer request" })
        );
        assert!(s.waiting.lock().unwrap().is_empty());
        // its pictures come later and are never sent
        let done = crate::preview::Event::Done {
            file: card,
            seq: 1,
            cards: vec![],
        };
        assert_eq!(on_event(done, &s.waiting, &root), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lenses_of_a_large_class_hierarchy_take_no_quadratic_time() {
        let root = project("hierarchy");
        let mut s = Server::default();
        init(&mut s, &root);
        // 60 000 declarations: an interface, a class overriding each member, an expect class
        let n = 20_000;
        let mut text = String::from("package demo\ninterface Big {\n");
        text.extend((0..n).map(|i| format!("    fun f{i}()\n")));
        text.push_str("}\nclass Impl : Big {\n");
        text.extend((0..n).map(|i| format!("    override fun f{i}() {{}}\n")));
        text.push_str("}\nexpect class E {\n");
        text.extend((0..n).map(|i| format!("    fun e{i}()\n")));
        text.push_str("}\n");
        let big = root.join("app/src/main/java/demo/Big.kt");
        call(&mut s, "update", json!({ "path": big, "text": text }));
        let t = Instant::now();
        let r = call(&mut s, "lenses", json!({ "path": big, "text": text }));
        let took = t.elapsed();
        let lenses = r["result"].as_array().unwrap_or_else(|| panic!("{r}"));
        let titles = |line: usize| -> Vec<&str> {
            lenses
                .iter()
                .filter(|l| l["line"] == line)
                .filter_map(|l| l["title"].as_str())
                .collect()
        };
        assert_eq!(titles(2), ["↓ Implemented in 1 class"], "Big.f0");
        assert_eq!(titles(n + 4), ["↑ Overrides Big.f0"], "Impl.f0");
        assert!(took < Duration::from_millis(1000), "{took:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn each_engine_has_its_own_picture_folder_and_ended_engines_folders_go() {
        let root = std::env::temp_dir().join(format!("dl-serve-pngs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let dead = root.join(ended.id().to_string());
        ended.wait().unwrap();
        let mine = png_dir(&root);
        let (old, fresh) = (root.join("0123-1-0.png"), root.join("0123-2-0.png"));
        for d in [&dead, &mine] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("a.png"), "png").unwrap();
        }
        for p in [&old, &fresh] {
            std::fs::write(p, "png").unwrap();
        }
        let ttl = Duration::from_secs(crate::preview_cache::TTL_SECS + 60);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - ttl).unwrap();
        sweep_png_dirs(&root);
        assert!(!dead.exists(), "an ended engine's pictures");
        assert!(
            !old.exists(),
            "an older build's picture past the cache's time"
        );
        assert!(mine.join("a.png").exists() && fresh.exists());
        // and this engine's own folder goes when it ends
        let mut s = Server {
            pngs: Some(mine.clone()),
            ..Default::default()
        };
        s.shutdown(Duration::from_secs(1));
        assert!(!mine.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A project of `n` files under /p (not on disk) in the index and the file list.
    fn big(n: usize) -> (Server, Vec<PathBuf>) {
        let root = PathBuf::from("/p");
        let mut s = Server {
            open: true,
            project: Some(project_ref(root.clone(), vec![])),
            ..Default::default()
        };
        let files: Vec<PathBuf> = (0..n)
            .map(|i| root.join(format!("m{}/src/main/java/a/F{i}.kt", i % 40)))
            .collect();
        for (i, f) in files.iter().enumerate() {
            s.index
                .update(f, &format!("package a\nclass X{i}\nfun y{i}() {{}}\n"));
            s.files.insert(f.clone());
        }
        (s, files)
    }

    #[test]
    fn removes_scan_nothing_of_the_whole_project() {
        let (mut s, files) = big(12_000);
        s.texts.insert(files[1].clone(), "x".into());
        let t = Instant::now();
        for f in &files[..3000] {
            let r = call(&mut s, "remove", json!({ "path": f }));
            assert!(r["error"].is_null(), "{r}");
        }
        let one_by_one = t.elapsed();
        eprintln!("3000 removes on 12000 files: {one_by_one:?}");
        assert!(!s.texts.contains_key(&files[1]));
        assert_eq!((s.files.len(), s.index.files.len()), (9000, 9000));
        assert!(one_by_one < Duration::from_millis(500), "{one_by_one:?}");
        // many in one request; a folder; a path outside the project is named, the rest still go
        let t = Instant::now();
        let batch: Vec<&PathBuf> = files[3000..6000].iter().collect();
        let r = call(&mut s, "remove", json!({ "paths": batch }));
        assert!(r["error"].is_null(), "{r}");
        eprintln!("3000 removes in one request: {:?}", t.elapsed());
        assert_eq!((s.files.len(), s.index.files.len()), (6000, 6000));
        let r = call(
            &mut s,
            "remove",
            json!({ "paths": ["/elsewhere/A.kt", "/p/m7"] }),
        );
        assert_eq!(
            r["error"], "\"/elsewhere/A.kt\" is not in the project",
            "{r}"
        );
        assert_eq!(
            (s.files.len(), s.index.files.len()),
            (5850, 5850),
            "folder m7 went"
        );
        assert!(s.files.iter().all(|f| !f.starts_with("/p/m7")));
    }

    #[test]
    fn update_skips_what_project_open_skips_the_walked_folder_itself_included() {
        let root = std::fs::canonicalize(project("skips")).unwrap();
        let demo = root.join("app/src/main/java/demo");
        let files = [
            demo.join("node_modules/x/N.kt"),
            demo.join("feat/node_modules/Bad.kt"),
            demo.join("feat/.cache/Hidden.kt"),
            demo.join("feat/Ok.kt"),
            root.join("app/.cxx/C.kt"),
            root.join(".hidden/H.kt"),
        ];
        for f in &files {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            let n = f.file_stem().unwrap().to_str().unwrap();
            std::fs::write(f, format!("package demo\nclass {n} : Base\n")).unwrap();
        }
        let mut s = Server::default();
        init(&mut s, &root);
        let subclasses = |s: &mut Server| {
            let r = call(s, "implementations", json!({ "name": "Base" }));
            let mut v: Vec<String> = r["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["name"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(subclasses(&mut s), ["Ok"], "Project::open");
        for f in &files {
            call(&mut s, "remove", json!({ "path": f }));
        }
        assert_eq!(subclasses(&mut s), Vec::<String>::new());
        for p in [
            demo.join("node_modules"),
            demo.join("feat"),
            root.join("app/.cxx"),
            root.join(".hidden/H.kt"),
            root.join(".hidden"),
        ] {
            let r = call(&mut s, "update", json!({ "path": p }));
            assert!(r["error"].is_null(), "{r}");
        }
        assert_eq!(subclasses(&mut s), ["Ok"], "update of a folder or file");
        assert!(
            !s.files
                .iter()
                .any(|f| f.to_string_lossy().contains("node_modules"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_line_that_is_not_utf8_is_answered_and_the_engine_goes_on() {
        let input: &[u8] = b"{\"id\":1,\"method\":\"ping\"}\n{\"id\":2,\"method\":\"update\",\"params\":{\"text\":\"\xff\"}}\n\n{\"id\":3,\"method\":\"ping\"}";
        let got = std::cell::RefCell::new(Vec::new());
        serve_input(&mut Server::default(), input, &|v| {
            got.borrow_mut().push(v.clone())
        });
        let got = got.into_inner();
        let ids: Vec<&Value> = got.iter().map(|v| &v["id"]).collect();
        assert_eq!(ids, [&json!(1), &json!(2), &json!(3)], "{got:?}");
        assert!(
            got[1]["error"].as_str().unwrap().contains("not UTF-8"),
            "{got:?}"
        );
        assert_eq!(got[2]["result"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn when_the_preview_thread_ends_every_waiting_request_is_answered() {
        let (tx, events) = channel::<crate::preview::Event>();
        let (out, lines) = channel();
        let waiting: Waiting = Default::default();
        waiting
            .lock()
            .unwrap()
            .insert(PathBuf::from("/p/A.kt"), (1, json!(10)));
        waiting
            .lock()
            .unwrap()
            .insert(PathBuf::from("/p/B.kt"), (2, json!(11)));
        let w = waiting.clone();
        let t = std::thread::spawn(move || {
            relay(events, w, Out(Some(out)), PathBuf::from("/nonexistent"))
        });
        drop(tx);
        t.join().unwrap();
        let mut ids: Vec<i64> = lines
            .try_iter()
            .map(|v| {
                assert!(
                    v["error"]
                        .as_str()
                        .unwrap()
                        .contains("preview thread stopped"),
                    "{v}"
                );
                v["id"].as_i64().unwrap()
            })
            .collect();
        ids.sort();
        assert_eq!(ids, [10, 11]);
        assert!(waiting.lock().unwrap().is_empty());
    }

    #[test]
    fn saved_pictures_are_the_workers_png_bytes_not_encoded_again() {
        let dir = std::env::temp_dir().join(format!("dl-serve-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let png = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png"))
            .unwrap();
        let pic = crate::preview_cache::Picture::from_png(png.clone(), false).unwrap();
        assert!(pic.image.is_none(), "the engine decodes nothing");
        let saved = save_cards(&dir, Path::new("/p/Card.kt"), vec![("P".into(), Ok(pic))]);
        let path = saved[0]["png"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), png);
        let _ = std::fs::remove_dir_all(dir);
    }
}
