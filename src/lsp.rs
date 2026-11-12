//! A small Language Server Protocol client for type-aware Kotlin completion and go-to-definition.
//! It starts JetBrains' `kotlin-lsp` (or fwcd's `kotlin-language-server`) when one is installed.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

/// One LSP text edit: start line, start col, end line, end col (UTF-16), new text.
pub type LspEdit = (u32, u32, u32, u32, String);
/// Edits per file from a WorkspaceEdit.
pub type WorkspaceEdits = Vec<(PathBuf, Vec<LspEdit>)>;

pub enum Event {
    Completion {
        at: usize,
        items: Vec<(String, String, Option<String>)>,
    },
    Definition(Vec<(PathBuf, u32, u32)>),
    /// Text edits per file from a rename: (start line, start col, end line, end col, new text).
    /// Empty when the server failed or refused.
    Rename(WorkspaceEdits),
    /// Every reference to a symbol: (file, line, UTF-16 column).
    References(Vec<(PathBuf, u32, u32)>),
    Diagnostics(PathBuf, Vec<(u32, u32, u8, String)>),
}

#[derive(Clone, Copy)]
enum Pending {
    Init,
    Completion(usize),
    Definition,
    Rename,
    References,
}

pub struct Lsp {
    pub name: String,
    pub ready: bool,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    versions: HashMap<PathBuf, i32>,
    queued: Vec<Value>,
}

/// Finds a Kotlin language server: `$DROID_LITE_LSP`, then `kotlin-lsp`, then `kotlin-language-server`.
pub fn find_server() -> Option<(PathBuf, Vec<String>)> {
    if let Ok(cmd) = std::env::var("DROID_LITE_LSP") {
        let mut parts = cmd.split_whitespace().map(String::from);
        return parts.next().map(|p| (PathBuf::from(p), parts.collect()));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs = path
        .split(':')
        .map(PathBuf::from)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    let dirs: Vec<PathBuf> = dirs.collect();
    for (bin, args) in [
        ("kotlin-lsp", vec!["--stdio".to_string()]),
        ("kotlin-language-server", vec![]),
    ] {
        if let Some(p) = dirs.iter().map(|d| d.join(bin)).find(|p| p.is_file()) {
            return Some((p, args));
        }
    }
    None
}

pub fn uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace(' ', "%20"))
}

fn path_of(uri: &str) -> PathBuf {
    PathBuf::from(uri.trim_start_matches("file://").replace("%20", " "))
}

impl Lsp {
    pub fn start(root: &Path, ctx: egui::Context) -> Option<Lsp> {
        let (bin, args) = find_server()?;
        let heap = std::env::var("DROID_LITE_LSP_HEAP").unwrap_or_else(|_| "2g".into());
        let mut child = Command::new(&bin)
            .args(&args)
            // every JVM reads JAVA_TOOL_OPTIONS, so this caps kotlin-lsp whatever its launcher does
            .env("JAVA_TOOL_OPTIONS", format!("-Xmx{heap}"))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0; len];
                if r.read_exact(&mut buf).is_err() {
                    return;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                    if tx.send(v).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        });
        let name = bin
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut lsp = Lsp {
            name,
            ready: false,
            child,
            stdin,
            rx,
            next_id: 1,
            pending: HashMap::new(),
            versions: HashMap::new(),
            queued: Vec::new(),
        };
        let params = json!({
            "processId": std::process::id(),
            "rootUri": uri(root),
            "workspaceFolders": [{ "uri": uri(root), "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }],
            "capabilities": {
                "textDocument": {
                    "completion": { "completionItem": { "snippetSupport": false, "documentationFormat": ["plaintext"] } },
                    "definition": {},
                    "publishDiagnostics": {},
                    "synchronization": { "didSave": true }
                },
                "workspace": { "workspaceFolders": true }
            }
        });
        lsp.request("initialize", params, Pending::Init, true);
        Some(lsp)
    }

    fn write(&mut self, msg: &Value) {
        let body = msg.to_string();
        let _ = write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = self.stdin.flush();
    }

    fn send(&mut self, msg: Value, now: bool) {
        if self.ready || now {
            self.write(&msg);
        } else {
            // a server that never finishes starting must not make this grow forever
            if self.queued.len() >= 500 {
                self.queued.remove(0);
            }
            self.queued.push(msg);
        }
    }

    fn request(&mut self, method: &str, params: Value, kind: Pending, now: bool) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.send(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            now,
        );
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(
            json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            false,
        );
    }

    pub fn did_open(&mut self, path: &Path, text: &str) {
        self.versions.insert(path.to_path_buf(), 1);
        self.notify("textDocument/didOpen", json!({ "textDocument": { "uri": uri(path), "languageId": language(path), "version": 1, "text": text } }));
    }

    pub fn did_change(&mut self, path: &Path, text: &str) {
        let v = self.versions.entry(path.to_path_buf()).or_insert(1);
        *v += 1;
        let v = *v;
        self.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri(path), "version": v }, "contentChanges": [{ "text": text }] }));
    }

    pub fn did_close(&mut self, path: &Path) {
        if self.versions.remove(path).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri(path) } }),
            );
        }
    }

    pub fn did_save(&mut self, path: &Path) {
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri(path) } }),
        );
    }

    /// `at` is the byte offset the answer belongs to, so a late answer can be ignored.
    pub fn completion(&mut self, path: &Path, line: u32, col: u32, at: usize) {
        self.request("textDocument/completion", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": col } }), Pending::Completion(at), false);
    }

    pub fn definition(&mut self, path: &Path, line: u32, col: u32) {
        self.request("textDocument/definition", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": col } }), Pending::Definition, false);
    }

    pub fn rename(&mut self, path: &Path, line: u32, col: u32, new_name: &str) {
        self.request("textDocument/rename", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": col }, "newName": new_name }), Pending::Rename, false);
    }

    pub fn references(&mut self, path: &Path, line: u32, col: u32) {
        self.request(
            "textDocument/references",
            json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": col }, "context": { "includeDeclaration": true } }),
            Pending::References,
            false,
        );
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(id) = msg.get("id").and_then(Value::as_i64)
                && msg.get("method").is_none()
            {
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                match self.pending.remove(&id) {
                    Some(Pending::Init) => {
                        self.ready = true;
                        self.write(
                            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
                        );
                        for m in std::mem::take(&mut self.queued) {
                            self.write(&m);
                        }
                    }
                    Some(Pending::Completion(at)) => {
                        let list = result.get("items").cloned().unwrap_or(result);
                        let items = list
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|i| {
                                        let label = i.get("label")?.as_str()?.to_string();
                                        let detail = i
                                            .get("detail")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string();
                                        let insert = i
                                            .pointer("/textEdit/newText")
                                            .or(i.get("insertText"))
                                            .and_then(Value::as_str)
                                            .map(String::from);
                                        Some((label, detail, insert))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        out.push(Event::Completion { at, items });
                    }
                    Some(Pending::Definition) => out.push(Event::Definition(locations(&result))),
                    Some(Pending::Rename) => out.push(Event::Rename(workspace_edits(&result))),
                    Some(Pending::References) => out.push(Event::References(locations(&result))),
                    None => {}
                }
            } else if msg.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                let p = &msg["params"];
                let diags = p["diagnostics"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|d| {
                                (
                                    d.pointer("/range/start/line")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0) as u32,
                                    d.pointer("/range/start/character")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0) as u32,
                                    d.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
                                    d.get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Event::Diagnostics(
                    path_of(p["uri"].as_str().unwrap_or("")),
                    diags,
                ));
            } else if let (Some(id), Some(_)) = (msg.get("id"), msg.get("method")) {
                // server-to-client request (e.g. workspace/configuration): answer with null
                let id = id.clone();
                self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }));
            }
        }
        out
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait(); // reap it, so no zombie process stays behind
    }
}

/// Start positions of a Location, Location[] or LocationLink[] result.
fn locations(v: &Value) -> Vec<(PathBuf, u32, u32)> {
    let list = match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        v => vec![v.clone()],
    };
    list.iter()
        .filter_map(|l| {
            let u = l.get("uri").or(l.get("targetUri"))?.as_str()?;
            let r = l.get("range").or(l.get("targetSelectionRange"))?;
            Some((
                path_of(u),
                r.pointer("/start/line")?.as_u64()? as u32,
                r.pointer("/start/character")?.as_u64()? as u32,
            ))
        })
        .collect()
}

/// Reads a WorkspaceEdit in either form: `changes` (uri → edits) or `documentChanges`.
fn workspace_edits(v: &Value) -> WorkspaceEdits {
    let edit = |e: &Value| {
        let n = |p: &str| e.pointer(p).and_then(Value::as_u64).map(|x| x as u32);
        Some((
            n("/range/start/line")?,
            n("/range/start/character")?,
            n("/range/end/line")?,
            n("/range/end/character")?,
            e.get("newText")?.as_str()?.to_string(),
        ))
    };
    let mut out = Vec::new();
    if let Some(map) = v.get("changes").and_then(Value::as_object) {
        for (u, edits) in map {
            out.push((
                path_of(u),
                edits
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(edit)
                    .collect(),
            ));
        }
    }
    for dc in v
        .get("documentChanges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(u), Some(edits)) = (
            dc.pointer("/textDocument/uri").and_then(Value::as_str),
            dc.get("edits").and_then(Value::as_array),
        ) {
            out.push((path_of(u), edits.iter().filter_map(edit).collect()));
        }
    }
    out
}

fn language(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("java") => "java",
        _ => "kotlin",
    }
}

/// Byte offset to LSP (line, UTF-16 column).
pub fn position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() as u32;
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[start..].encode_utf16().count() as u32)
}

/// LSP (line, UTF-16 column) to byte offset.
pub fn offset(text: &str, line: u32, col: u32) -> usize {
    let start: usize = text
        .split_inclusive('\n')
        .take(line as usize)
        .map(str::len)
        .sum();
    let mut units = 0;
    for (i, c) in text[start..].char_indices() {
        if units >= col as usize || c == '\n' {
            return start + i;
        }
        units += c.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_workspace_edit_forms() {
        let a = json!({ "changes": { "file:///a.kt": [{ "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 5 } }, "newText": "b" }] } });
        assert_eq!(
            workspace_edits(&a),
            vec![(PathBuf::from("/a.kt"), vec![(1, 2, 1, 5, "b".to_string())])]
        );
        let d = json!({ "documentChanges": [{ "textDocument": { "uri": "file:///x%20y.kt", "version": 3 }, "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "newText": "Z" }] }] });
        assert_eq!(workspace_edits(&d)[0].0, PathBuf::from("/x y.kt"));
        assert!(workspace_edits(&Value::Null).is_empty());
    }

    #[test]
    fn positions_round_trip_with_utf16() {
        let t = "ab\nc😀d\n";
        assert_eq!(position(t, 4 + 4), (1, 3));
        assert_eq!(offset(t, 1, 3), 8);
        assert_eq!(offset(t, 0, 99), 2);
    }
}
