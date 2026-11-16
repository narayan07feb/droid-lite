//! `droid-lite serve`: the engine without the window, for editor plugins (VS Code, Cursor).
//!
//! One JSON request per line on stdin, one reply per line on stdout:
//! `{"id":1,"method":"initialize","params":{"root":"/path"}}` →
//! `{"id":1,"result":{...}}` or `{"id":1,"error":"..."}`.
//! Positions are 0-based lines and columns in characters.
//!
//! Methods: `ping`, `initialize {root}`, `implementations {name}`, `stability {path}`.

use crate::compose::{self, Stability, Verdict};
use crate::index::{Hit, Index};
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
        if !matches!(method, "ping" | "initialize") && !self.open {
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
                let path = Path::new(s("path")?);
                let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
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
        let _ = std::fs::remove_dir_all(root);
    }
}
