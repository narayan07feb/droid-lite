//! `droid-lite serve`: the engine without the window, for editor plugins (VS Code, Cursor).
//!
//! One JSON request per line on stdin, one reply per line on stdout:
//! `{"id":1,"method":"initialize","params":{"root":"/path"}}` →
//! `{"id":1,"result":{...}}` or `{"id":1,"error":"..."}`.
//! Positions are 0-based lines and columns in UTF-16 code units (as VS Code counts them).
//!
//! Methods: `ping`, `initialize {root}`, `update {path, text}`, `remove {path}` (a deleted file or
//! folder), `lenses {path, text?}`, `implementations {name, member?}`, `stability {path, text?}`;
//! device tools without a project: `adb`, `devices`, `screenshot {serial}`,
//! `record_start {serial}`, `record_stop`. Device tools run on their own thread and answer when
//! done, so a device that hangs (each adb call is stopped after 20 to 30 s) holds up nothing else.
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
    devices: Option<(Sender<DeviceCall>, std::thread::JoinHandle<()>)>,
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
fn save_cards(
    dir: &Path,
    file: &Path,
    cards: Vec<(String, Result<egui::ColorImage, String>)>,
) -> Vec<Value> {
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
            let png = pic.and_then(|img| {
                let [w, h] = img.size;
                let raw: Vec<u8> = img
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_srgba_unmultiplied())
                    .collect();
                let path = dir.join(format!("{key}-{stamp}-{i}.png"));
                image::RgbaImage::from_raw(w as u32, h as u32, raw)
                    .ok_or("bad picture size")?
                    .save(&path)
                    .map_err(|e| e.to_string())?;
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
/// macOS); a deleted file keeps its canonical folder.
fn canon(p: &str) -> std::path::PathBuf {
    let p = Path::new(p);
    std::fs::canonicalize(p).unwrap_or_else(|_| {
        match (
            p.parent().and_then(|d| std::fs::canonicalize(d).ok()),
            p.file_name(),
        ) {
            (Some(d), Some(n)) => d.join(n),
            _ => p.to_path_buf(),
        }
    })
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
            Ok(json!(r.finish(&capture::out_dir())?))
        }
        _ => Err(format!("unknown method {method}")),
    }
}

/// The device thread: runs device calls one after another and answers each when it is done. It
/// ends when its sender is dropped; a recording still running then stops (see [`Server::shutdown`]).
fn device_thread(out: Out) -> (Sender<DeviceCall>, std::thread::JoinHandle<()>) {
    let (tx, rx) = channel::<DeviceCall>();
    let t = std::thread::spawn(move || {
        let mut recording = None;
        for (id, method, params) in rx {
            let r = guarded(|| device_call(&mut recording, &method, &params));
            out.emit(answer(id, r));
        }
    });
    (tx, t)
}

/// Every source file under `dir` (a folder created, moved or renamed), leaving out generated and
/// tool folders like `Project::open` does.
fn source_files(dir: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(dir)
        .hidden(true)
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some("build" | ".gradle" | ".idea" | ".git" | "node_modules" | ".cxx")
            )
        })
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| crate::project::is_code(p))
        .collect()
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

/// Generated or tool folders the index leaves out (Project::open skips them too): Hilt_* and
/// *_Factory classes there would count as implementations.
fn ignored(root: Option<&Path>, path: &Path) -> bool {
    let rel = root.and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path);
    rel.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("build" | ".gradle" | ".idea" | ".git")
        )
    })
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
                let dirs: Vec<_> = p.modules.iter().map(|m| m.dir.clone()).collect();
                self.reports = compose::load_reports(&dirs);
                self.project = Some(project_ref(p.root.clone(), p.files.clone()));
                self.files = p.files.iter().cloned().collect();
                self.files_changed = false;
                self.open = true;
                Ok(json!({
                    "files": p.files.len(),
                    "symbols": self.index.symbol_count(),
                    "modules": p.modules.len(),
                }))
            }
            "update" => {
                let path = &canon(s("path")?);
                if ignored(self.project.as_ref().map(|p| p.root.as_path()), path) {
                    return Ok(Value::Null);
                }
                // a folder created, moved or renamed: every source file in it
                if path.is_dir() {
                    for f in source_files(path) {
                        let Ok(src) = std::fs::read_to_string(&f) else {
                            continue;
                        };
                        self.index.update(&f, &src);
                        self.texts.remove(&f);
                        self.files_changed |= self.files.insert(f);
                    }
                    return Ok(Value::Null);
                }
                let src = match params["text"].as_str() {
                    Some(t) => t.to_string(),
                    None => std::fs::read_to_string(path).map_err(|e| e.to_string())?,
                };
                self.index.update(path, &src);
                if params["text"].is_string() {
                    self.texts.insert(path.clone(), src);
                } else {
                    self.texts.remove(path);
                }
                self.files_changed |= self.files.insert(path.clone());
                Ok(Value::Null)
            }
            // a deleted file, or everything under a deleted folder (in the project: never all of
            // it by an empty path)
            "remove" => {
                let raw = s("path")?;
                let path = &canon(raw);
                let root = self.project.as_ref().map(|p| p.root.as_path());
                if raw.trim().is_empty() || root.is_some_and(|r| !path.starts_with(r)) {
                    return Err(format!("{raw:?} is not in the project"));
                }
                self.index.remove(path);
                self.index.remove_under(path);
                self.texts.retain(|p, _| !p.starts_with(path));
                let before = self.files.len();
                self.files.retain(|p| !p.starts_with(path));
                self.files_changed |= self.files.len() != before;
                Ok(Value::Null)
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
                for d in &syms.decls {
                    let (line, _) = lines.position(&text, d.offset);
                    let kmp = self.index.counterparts(path, &syms, d);
                    if !kmp.is_empty() {
                        let expect = crate::index::kmp_role(&syms, d) == Some("expect");
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
                            if let Some(h) = self.index.overridden(c, &d.name) {
                                let title = format!("↑ Overrides {}", h.name);
                                out.push(placer.lens(line, title, &[h]));
                            }
                        } else {
                            let impls = self.index.member_implementations(c, &d.name);
                            if !impls.is_empty() {
                                let n = plural(impls.len(), "class", "classes");
                                let title = format!("↓ Implemented in {n}");
                                out.push(placer.lens(line, title, &impls));
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

    /// Starts drawing a file's previews. `Ok(Some)` answers now (no previews); `Ok(None)` means the
    /// answer comes later from the preview thread.
    pub fn previews(&mut self, id: &Value, params: &Value) -> Result<Option<Value>, String> {
        if !self.open {
            return Err("call initialize first".into());
        }
        let path = canon(params["path"].as_str().ok_or("missing \"path\"")?);
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let syms = crate::kotlin::scan(&text);
        let previews: Vec<String> = syms
            .decls
            .iter()
            .filter(|d| d.is_preview() && d.container.is_none())
            .map(|d| d.name.clone())
            .collect();
        if previews.is_empty() {
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
            let (tx, events) = crate::preview::spawn(&egui::Context::default());
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
    /// removed since the last one (else the same list, not a copy).
    fn project_ref(&mut self) -> Option<crate::preview::ProjectRef> {
        if std::mem::take(&mut self.files_changed)
            && let Some(p) = &mut self.project
        {
            *p = project_ref(p.root.clone(), self.files.iter().cloned().collect());
        }
        self.project.clone()
    }

    /// At the end of input: the device thread ends first (dropping a running recording stops
    /// screenrecord and deletes its file on the device), waited for at most `limit`; then this
    /// engine's pictures go.
    pub fn shutdown(&mut self, limit: Duration) {
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
            let out = self.out.clone();
            let (devices, _) = self.devices.get_or_insert_with(|| device_thread(out));
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
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(v) = server.request(&line) {
            emit(&v);
        }
    }
    server.shutdown(Duration::from_secs(15));
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
        for p in ["", " ", "/", "/tmp/elsewhere"] {
            let r = call(&mut s, "remove", json!({ "path": p }));
            assert!(r["error"].is_string(), "{p:?}: {r}");
        }
        let r = call(&mut s, "implementations", json!({ "name": "Api" }));
        assert_eq!(r["result"][0]["name"], "RealApi", "{r}");
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
}
