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
//! request answers an older one it replaces with an error.

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
    waiting: Waiting,
    /// The number of the last preview request.
    seq: u64,
    /// The editor's latest text of each file `update` sent: the index's offsets point into it.
    texts: HashMap<PathBuf, String>,
    devices: Option<Sender<DeviceCall>>,
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
        Event::Status(text, progress) => {
            let mut v = json!({ "event": "preview_status", "text": text });
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

/// Sends the preview worker's progress and answers as they come, until the client ends.
fn relay(events: Receiver<crate::preview::Event>, waiting: Waiting, out: Out) {
    let dir = crate::preview::dir().join("vscode");
    for event in events {
        if let Some(v) = guarded(|| Ok(on_event(event, &waiting, &dir))).unwrap_or(None) {
            out.emit(v);
        }
    }
}

/// 0-based line and UTF-16 column of byte `offset` in `text`; an offset past the end or inside a
/// character counts from the character's start (never a panic).
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = &text[..at];
    let line = before.matches('\n').count();
    let col = before
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count();
    (line, col)
}

/// A hit with its position, counted in the text the index read: the editor's text `update` sent,
/// else the file on disk.
fn hit_json(h: &Hit, texts: &HashMap<PathBuf, String>) -> Value {
    let disk;
    let text = match texts.get(&h.path) {
        Some(t) => t,
        None => {
            disk = std::fs::read_to_string(&h.path).unwrap_or_default();
            &disk
        }
    };
    let (line, col) = position(text, h.offset);
    json!({ "name": h.name, "detail": h.detail, "path": h.path, "line": line, "col": col })
}

fn lens(line: usize, title: String, targets: &[Hit], texts: &HashMap<PathBuf, String>) -> Value {
    let targets: Vec<Value> = targets.iter().map(|h| hit_json(h, texts)).collect();
    json!({ "line": line, "title": title, "targets": targets })
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

/// The device thread: runs device calls one after another and answers each when it is done.
fn device_thread(out: Out) -> Sender<DeviceCall> {
    let (tx, rx) = channel::<DeviceCall>();
    std::thread::spawn(move || {
        let mut recording = None;
        for (id, method, params) in rx {
            let r = guarded(|| device_call(&mut recording, &method, &params));
            out.emit(answer(id, r));
        }
    });
    tx
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
                self.project = Some(crate::preview::ProjectRef {
                    root: p.root.clone(),
                    scripts: p
                        .files
                        .iter()
                        .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
                        .cloned()
                        .collect(),
                    files: Arc::new(p.files.clone()),
                });
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
                Ok(Value::Null)
            }
            // a deleted file, or everything under a deleted folder
            "remove" => {
                let path = &canon(s("path")?);
                self.index.remove(path);
                self.index.remove_under(path);
                self.texts.retain(|p, _| !p.starts_with(path));
                Ok(Value::Null)
            }
            // Droid Lite's gutter links for one file: expect/actual, supertypes, implementations,
            // overrides
            "lenses" => {
                let path = &canon(s("path")?);
                let text = text(path)?;
                let syms = crate::kotlin::scan(&text);
                let mut out = Vec::new();
                for d in &syms.decls {
                    let (line, _) = position(&text, d.offset);
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
                        out.push(lens(line, title, &kmp, texts));
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
                            out.push(lens(line, title, &supers, texts));
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
                            out.push(lens(line, format!("↓ {verb} {n}"), &impls, texts));
                        }
                    } else if let Some(c) = &d.container
                        && matches!(d.kind, Kind::Fun | Kind::Property)
                    {
                        if d.has_modifier("override") {
                            if let Some(h) = self.index.overridden(c, &d.name) {
                                let title = format!("↑ Overrides {}", h.name);
                                out.push(lens(line, title, &[h], texts));
                            }
                        } else {
                            let impls = self.index.member_implementations(c, &d.name);
                            if !impls.is_empty() {
                                let n = plural(impls.len(), "class", "classes");
                                let title = format!("↓ Implemented in {n}");
                                out.push(lens(line, title, &impls, texts));
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
                Ok(Value::Array(
                    hits.iter().map(|h| hit_json(h, texts)).collect(),
                ))
            }
            // each composable of a file with its parameters' stability (report, else a guess)
            "stability" => {
                let path = &canon(s("path")?);
                let text = text(path)?;
                let syms = crate::kotlin::scan(&text);
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
                        let (line, col) = position(&text, d.offset);
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
                                    let (l, c) = position(&text, r.start);
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
        let (waiting, out) = (self.waiting.clone(), self.out.clone());
        let client = self.previews.get_or_insert_with(|| {
            let (tx, events) = crate::preview::spawn(&egui::Context::default());
            std::thread::spawn(move || relay(events, waiting, out));
            tx
        });
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
        let _ = client.send(crate::preview::Request {
            file: path,
            class,
            previews,
            seq: self.seq,
            project: self.project.clone(),
        });
        Ok(None)
    }

    /// Answers one request line now (`Some`), or later through the output (`None`: previews and
    /// device tools).
    pub fn request(&mut self, line: &str) -> Option<Value> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(json!({ "id": null, "error": format!("bad JSON: {e}") })),
        };
        let (id, method, params) = (
            &req["id"],
            req["method"].as_str().unwrap_or(""),
            &req["params"],
        );
        if is_device_call(method) {
            let out = self.out.clone();
            let devices = self.devices.get_or_insert_with(|| device_thread(out));
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
        let t = std::thread::spawn(move || relay(events, Default::default(), Out(Some(out))));
        tx.send(crate::preview::Event::Status(
            "Drawing".into(),
            Some((1, 2)),
        ))
        .unwrap();
        let v = lines
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!((&v["text"], &v["done"]), (&json!("Drawing"), &json!(1)));
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
}
