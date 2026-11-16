//! `droid-lite serve`: the engine without the window, for editor plugins (VS Code, Cursor).
//!
//! One JSON request per line on stdin, one reply per line on stdout:
//! `{"id":1,"method":"initialize","params":{"root":"/path"}}` →
//! `{"id":1,"result":{...}}` or `{"id":1,"error":"..."}`.
//! Positions are 0-based lines and columns in characters.
//!
//! Methods: `ping`, `initialize {root}`, `update {path, text}`, `lenses {path, text?}`,
//! `implementations {name, member?}`, `stability {path, text?}`; device tools without a project:
//! `adb`, `devices`, `screenshot {serial}`, `record_start {serial}`, `record_stop`.
//!
//! `previews {path}` answers when the pictures are drawn (seconds), while other requests go on;
//! meanwhile it sends `{"event":"preview_status","file","text","done"?,"total"?}` lines. A newer
//! request for the same file answers the older one with an error.

use crate::capture::{self, Recording};
use crate::compose::{self, Stability, Verdict};
use crate::index::{Hit, Index};
use crate::kotlin::Kind;
use crate::project::Project;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Server {
    index: Index,
    /// Compose compiler / stability-analyzer reports by function name.
    reports: HashMap<String, Stability>,
    open: bool,
    recording: Option<Recording>,
    project: Option<crate::preview::ProjectRef>,
    previews: Option<Arc<Mutex<crate::preview::Client>>>,
    /// Preview requests waiting for their pictures: file → request id.
    waiting: Arc<Mutex<HashMap<PathBuf, Value>>>,
}

/// One JSON line on stdout; the preview thread writes too, so each line is written whole.
fn emit(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// Saves the pictures as PNGs (new names each time, so an editor never shows a cached one) and
/// deletes this file's older ones.
fn save_cards(file: &Path, cards: Vec<(String, Result<egui::ColorImage, String>)>) -> Vec<Value> {
    let dir = crate::preview::dir().join("vscode");
    let _ = std::fs::create_dir_all(&dir);
    let key = format!("{:016x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        file.hash(&mut h);
        h.finish()
    });
    if let Ok(old) = std::fs::read_dir(&dir) {
        for e in old.flatten() {
            if e.file_name().to_string_lossy().starts_with(&key) {
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

/// Sends the preview worker's progress and answers as they come.
fn relay(client: Arc<Mutex<crate::preview::Client>>, waiting: Arc<Mutex<HashMap<PathBuf, Value>>>) {
    use crate::preview::Event;
    loop {
        let event = client.lock().ok().and_then(|c| c.poll());
        let Some(event) = event else {
            std::thread::sleep(std::time::Duration::from_millis(30));
            continue;
        };
        let take = |f: &Path| waiting.lock().ok().and_then(|mut w| w.remove(f));
        match event {
            Event::Status(text, progress) => {
                let mut v = json!({ "event": "preview_status", "text": text });
                if let Some((done, total)) = progress {
                    v["done"] = json!(done);
                    v["total"] = json!(total);
                }
                emit(&v);
            }
            Event::Done { file, cards } => {
                if let Some(id) = take(&file) {
                    emit(
                        &json!({ "id": id, "result": { "file": file, "cards": save_cards(&file, cards) } }),
                    );
                }
            }
            Event::Failed { file, error } => {
                if let Some(id) = take(&file) {
                    emit(&json!({ "id": id, "error": error }));
                }
            }
        }
    }
}

/// 0-based line and character column of byte `offset` in `text`.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count();
    let col = before.rsplit('\n').next().unwrap_or("").chars().count();
    (line, col)
}

fn hit_json(h: &Hit) -> Value {
    let text = std::fs::read_to_string(&h.path).unwrap_or_default();
    let (line, col) = position(&text, h.offset);
    json!({ "name": h.name, "detail": h.detail, "path": h.path, "line": line, "col": col })
}

fn lens(line: usize, title: String, targets: &[Hit]) -> Value {
    json!({ "line": line, "title": title, "targets": targets.iter().map(hit_json).collect::<Vec<_>>() })
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
        // the editor's unsaved text when sent, else the file on disk
        let text = |path: &Path| match params["text"].as_str() {
            Some(t) => Ok(t.to_string()),
            None => std::fs::read_to_string(path).map_err(|e| e.to_string()),
        };
        let device_tool = matches!(
            method,
            "adb" | "devices" | "screenshot" | "record_start" | "record_stop"
        );
        if !matches!(method, "ping" | "initialize") && !device_tool && !self.open {
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
                self.index.update(path, &text(path)?);
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
                        out.push(lens(line, format!("{verb} in {}", sets.join(", ")), &kmp));
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
                            out.push(lens(line, format!("↑ {}", d.supers.join(", ")), &supers));
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
                            out.push(lens(line, format!("↓ {verb} {n}"), &impls));
                        }
                    } else if let Some(c) = &d.container
                        && matches!(d.kind, Kind::Fun | Kind::Property)
                    {
                        if d.has_modifier("override") {
                            if let Some(h) = self.index.overridden(c, &d.name) {
                                out.push(lens(line, format!("↑ Overrides {}", h.name), &[h]));
                            }
                        } else {
                            let impls = self.index.member_implementations(c, &d.name);
                            if !impls.is_empty() {
                                let n = plural(impls.len(), "class", "classes");
                                out.push(lens(line, format!("↓ Implemented in {n}"), &impls));
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
                Ok(Value::Array(hits.iter().map(hit_json).collect()))
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
                                    v["line"] = json!(l);
                                    v["col"] = json!(c);
                                    v["end"] = json!(c + text[r.clone()].chars().count());
                                }
                                v
                            }).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                Ok(Value::Array(out))
            }
            "adb" => Ok(json!(adb()?)),
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
                if self.recording.is_some() {
                    return Err("a recording is already running".into());
                }
                self.recording = Some(Recording::start(&adb()?, s("serial")?)?);
                Ok(Value::Null)
            }
            // blocks a few seconds while the device finishes and sends the video
            "record_stop" => {
                let r = self.recording.take().ok_or("no recording is running")?;
                Ok(json!(r.finish(&capture::out_dir())?))
            }
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
        let client = self
            .previews
            .get_or_insert_with(|| {
                let c = Arc::new(Mutex::new(crate::preview::Client::new(
                    &egui::Context::default(),
                )));
                let (c2, w) = (c.clone(), self.waiting.clone());
                std::thread::spawn(move || relay(c2, w));
                c
            })
            .clone();
        let older = self
            .waiting
            .lock()
            .map_err(|e| e.to_string())?
            .insert(path.clone(), id.clone());
        if let Some(older) = older {
            emit(&json!({ "id": older, "error": "replaced by a newer request" }));
        }
        client
            .lock()
            .map_err(|e| e.to_string())?
            .request(crate::preview::Request {
                file: path,
                class,
                previews,
                project: self.project.clone(),
            });
        Ok(None)
    }

    /// Answers one request line.
    pub fn reply(&mut self, line: &str) -> Value {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return json!({ "id": null, "error": format!("bad JSON: {e}") }),
        };
        let id = req["id"].clone();
        match self.handle(req["method"].as_str().unwrap_or(""), &req["params"]) {
            Ok(result) => json!({ "id": id, "result": result }),
            Err(e) => json!({ "id": id, "error": e }),
        }
    }
}

/// Reads requests from stdin until it closes.
pub fn run() {
    let mut server = Server::default();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(&line).unwrap_or_default();
        if req["method"] == "previews" {
            match server.previews(&req["id"], &req["params"]) {
                Ok(Some(result)) => emit(&json!({ "id": req["id"], "result": result })),
                Ok(None) => {}
                Err(e) => emit(&json!({ "id": req["id"], "error": e })),
            }
            continue;
        }
        emit(&server.reply(&line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("droid-lite-serve-{}", std::process::id()));
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
        let root = project();
        let mut s = Server::default();
        assert_eq!(
            s.previews(&json!(1), &json!({ "path": "x" })),
            Err("call initialize first".into())
        );
        // device tools need no project
        assert_eq!(
            s.reply(r#"{"id":0,"method":"record_stop"}"#)["error"],
            "no recording is running"
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
}
