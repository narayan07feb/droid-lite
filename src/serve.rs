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

use crate::capture::{self, Recording};
use crate::compose::{self, Stability, Verdict};
use crate::index::{Hit, Index};
use crate::kotlin::Kind;
use crate::project::Project;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

#[derive(Default)]
pub struct Server {
    index: Index,
    /// Compose compiler / stability-analyzer reports by function name.
    reports: HashMap<String, Stability>,
    open: bool,
    recording: Option<Recording>,
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
                        json!({
                            "function": d.name,
                            "line": line,
                            "col": col,
                            "skippable": st.skippable,
                            "source": st.source,
                            "params": st.params.iter().map(|p| json!({
                                "name": p.name,
                                "verdict": verdict(p.verdict),
                                "reason": p.reason,
                            })).collect::<Vec<_>>(),
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
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}", server.reply(&line));
        let _ = out.flush();
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
