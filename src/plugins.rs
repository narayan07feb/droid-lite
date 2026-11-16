//! Plugins: outside tools (test runners, checkers) that Droid Lite drives through a small JSON
//! contract. A plugin is a folder with a `plugin.json`; the contract is in docs/PLUGINS.md.
//! Nothing here knows any one tool: the plugin's own commands print JSON, Droid Lite shows it.

use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// A command (argv) with a name, for an action or a view.
#[derive(Clone, Debug, PartialEq)]
pub struct Named {
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    pub name: String,
    /// The plugin's folder (`{plugin}`).
    pub folder: PathBuf,
    /// Where its commands run: `dir` in the manifest, else the plugin's folder.
    pub dir: PathBuf,
    pub list: Option<Vec<String>>,
    pub run: Option<Vec<String>>,
    pub watch: Option<Vec<String>>,
    pub actions: Vec<Named>,
    pub views: Vec<Named>,
    /// (name, url)
    pub links: Vec<(String, String)>,
    /// A project plugin's approval key, as it was read; `None` for the user's own plugins.
    pub trust: Option<Trust>,
    /// Its commands may run: a plugin of the user's own folder, or a project plugin the user
    /// allowed with exactly these files ([`approve`]). [`Manifest::command`] refuses otherwise.
    pub allowed: bool,
}

/// What a project plugin is allowed under: the real path of the project, the real path of the
/// plugin's folder and the [`tree_hash`] of that folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trust {
    pub project: String,
    pub folder: String,
    pub hash: String,
}

/// One test case or group from the `list` command. A group without an id cannot run itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Case {
    pub id: Option<String>,
    pub label: String,
    pub detail: String,
    pub file: Option<PathBuf>,
    /// 1-based, as editors count; 0 when not given.
    pub line: usize,
    pub children: Vec<Case>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Passed,
    Failed,
    Skipped,
}

/// A result line of a run, or a row of a view: both use the same fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub case: Option<String>,
    pub label: String,
    pub status: Option<Status>,
    pub message: String,
    pub file: Option<PathBuf>,
    pub line: usize,
    pub url: Option<String>,
}

/// The `watch` command's latest state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub title: String,
    pub status: Option<Status>,
    pub rows: Vec<(String, String)>,
}

/// What one output line of `run` means.
#[derive(Debug, PartialEq)]
pub enum Event {
    Result(Entry),
    Log(String),
}

fn argv(v: &Value) -> Option<Vec<String>> {
    let a: Vec<String> = v
        .as_array()?
        .iter()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    (!a.is_empty()).then_some(a)
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn named(v: &Value, key: &str) -> Vec<Named> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| {
            Some(Named {
                name: text(x, "name"),
                argv: argv(x.get("run")?)?,
            })
        })
        .collect()
}

impl Manifest {
    pub fn parse(json: &str, folder: &Path) -> Result<Manifest, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let name = text(&v, "name");
        if name.is_empty() {
            return Err("\"name\" is missing".into());
        }
        // as the VS Code extension's path.resolve(real folder, dir)
        let real = folder
            .canonicalize()
            .unwrap_or_else(|_| folder.to_path_buf());
        let dir = match v.get("dir").and_then(Value::as_str) {
            Some(d) => normalize(&real.join(expand_home(d))),
            None => real,
        };
        let links = v
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|l| (text(l, "name"), text(l, "url")))
            .filter(|(_, u)| !u.is_empty())
            .collect();
        Ok(Manifest {
            name,
            folder: folder.to_path_buf(),
            dir,
            list: v.get("list").and_then(argv),
            run: v.get("run").and_then(argv),
            watch: v.get("watch").and_then(argv),
            actions: named(&v, "actions"),
            views: named(&v, "views"),
            links,
            trust: None,
            allowed: false,
        })
    }

    /// Every command the plugin may run, for the approval text.
    pub fn commands(&self) -> Vec<&[String]> {
        let mut out: Vec<&[String]> = [&self.list, &self.run, &self.watch]
            .into_iter()
            .flatten()
            .map(Vec::as_slice)
            .collect();
        out.extend(
            self.actions
                .iter()
                .chain(&self.views)
                .map(|n| n.argv.as_slice()),
        );
        out.dedup();
        out
    }

    /// What the Tests tab shows for a project plugin that is not allowed yet.
    pub fn approval_text(&self) -> String {
        let cmds: Vec<String> = self.commands().iter().map(|a| format!("{a:?}")).collect();
        format!(
            "Needs your OK: runs {} from {}\nAllowing trusts this plugin's folder. Commands may also \
             run project code outside it (for example a script in the repo); changes to that code do \
             not ask again.",
            cmds.join(", "),
            self.folder.display()
        )
    }

    /// The command for `argv`, with `{plugin}`, `{project}` and `{id}` filled in, run in `dir`.
    /// Its stdin is empty: a plugin must never wait for typed input. An error for a project plugin
    /// the user has not allowed ([`Manifest::allowed`]): nothing of it runs.
    pub fn command(
        &self,
        argv: &[String],
        project: Option<&Path>,
        id: &str,
    ) -> Result<Command, String> {
        if !self.allowed {
            return Err(format!("{} needs your OK first (Tests tab)", self.name));
        }
        let project = project.map(|p| p.display().to_string()).unwrap_or_default();
        let fill = |s: &str| {
            s.replace("{plugin}", &self.folder.display().to_string())
                .replace("{project}", &project)
                .replace("{id}", id)
        };
        let mut cmd = Command::new(expand_home(&fill(&argv[0])));
        cmd.args(argv[1..].iter().map(|a| fill(a)))
            .current_dir(&self.dir)
            .env("DROID_LITE_PROJECT", &project)
            .stdin(std::process::Stdio::null());
        // its own process group, so Stop reaches the tools it starts too
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        Ok(cmd)
    }
}

/// `p` with `.` and `..` taken out by its text, as Node's path.resolve does (`/..` is `/`).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(p),
    }
}

/// The plugin in a folder someone picked: the folder itself, or its `droid-lite-plugin/` folder
/// (where a tool keeps the plugin that drives it).
pub fn find_in(picked: &Path) -> Option<PathBuf> {
    [picked.to_path_buf(), picked.join("droid-lite-plugin")]
        .into_iter()
        .find(|f| f.join("plugin.json").is_file())
}

/// Adds the plugin in `picked` to `plugins` as a link, so it follows the tool's updates (a
/// `git pull`). Returns its name; a plugin already linked is not added twice.
pub fn add(plugins: &Path, picked: &Path) -> Result<String, String> {
    let folder = find_in(picked)
        .ok_or("No plugin.json in that folder or in its droid-lite-plugin folder")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let json = std::fs::read_to_string(folder.join("plugin.json")).map_err(|e| e.to_string())?;
    let m = Manifest::parse(&json, &folder)?;
    std::fs::create_dir_all(plugins).map_err(|e| e.to_string())?;
    let linked = std::fs::read_dir(plugins)
        .map_err(|e| e.to_string())?
        .flatten()
        .any(|e| e.path().canonicalize().is_ok_and(|p| p == folder));
    if linked {
        return Ok(m.name);
    }
    let slug: String = m
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let link = (1..)
        .map(|n| match n {
            1 => plugins.join(&slug),
            n => plugins.join(format!("{slug}-{n}")),
        })
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("a free name");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&folder, &link).map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    return Err("adding plugins needs links (macOS or Linux)".into());
    Ok(m.name)
}

/// Removes an added plugin: only its link in `plugins`, never the folder it points to.
pub fn remove(plugins: &Path, m: &Manifest) -> Result<(), String> {
    let is_link = std::fs::symlink_metadata(&m.folder).is_ok_and(|md| md.file_type().is_symlink());
    if m.folder.parent() != Some(plugins) || !is_link {
        return Err(format!(
            "{} was not added with Add plugin; delete or move that folder yourself",
            m.folder.display()
        ));
    }
    std::fs::remove_file(&m.folder).map_err(|e| e.to_string())
}

/// The approvals file: `approved-plugins.json` in Droid Lite's config folder, shared with the VS Code
/// extension: `{"version":2,"approved":{"<project real path>":{"<plugin folder real path>":"<tree
/// hash>"}}}`. Any other shape (an older flat map too) holds no approvals.
pub fn approvals_file(config: &Path) -> PathBuf {
    config.join("approved-plugins.json")
}

/// The approvals file as JSON; `None` when there is none. An error when it is not a JSON object.
fn read_store(store: &Path) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(store) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(v @ Value::Object(_)) => Ok(Some(v)),
            _ => Err(format!(
                "{} is not a JSON object; fix or delete it",
                store.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", store.display())),
    }
}

fn is_v2(v: &Value) -> bool {
    v["version"] == 2 && v["approved"].is_object()
}

/// Whether the user allowed this project plugin, for this project, with exactly these files.
pub fn is_approved(store: &Path, m: &Manifest) -> bool {
    let (Some(t), Ok(Some(v))) = (&m.trust, read_store(store)) else {
        return false;
    };
    is_v2(&v) && v["approved"][&t.project][&t.folder].as_str() == Some(t.hash.as_str())
}

/// Allows a project plugin as it was read ([`Manifest::trust`]), every other entry kept. Under a
/// lock on `<store>.lock`, so two windows (or the VS Code extension) allowing at once keep both;
/// written through a 0600 temp file and a rename. A file that is not a JSON object is never
/// overwritten; one of another shape (the older flat map) is replaced.
pub fn approve(store: &Path, m: &Manifest) -> Result<(), String> {
    let t = m
        .trust
        .as_ref()
        .ok_or_else(|| format!("{} is not a project plugin", m.name))?;
    let dir = store.parent().ok_or("no folder for the approvals file")?;
    let name = store
        .file_name()
        .ok_or("no name for the approvals file")?
        .to_string_lossy();
    let err = |e: std::io::Error| format!("{}: {e}", store.display());
    std::fs::create_dir_all(dir).map_err(err)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{name}.lock")))
        .map_err(err)?;
    lock.lock().map_err(err)?;
    let mut v = match read_store(store)? {
        Some(v) if is_v2(&v) => v,
        _ => serde_json::json!({"version": 2, "approved": {}}),
    };
    let project = &mut v["approved"][&t.project];
    if !project.is_object() {
        *project = serde_json::json!({});
    }
    project[&t.folder] = Value::String(t.hash.clone());
    let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())? + "\n";
    let salt = std::hash::BuildHasher::hash_one(
        &std::collections::hash_map::RandomState::new(),
        std::time::SystemTime::now(),
    );
    let tmp = dir.join(format!("{name}.{}.{salt:016x}.tmp", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, store))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err(e)
        })
}

/// A plugin folder's files are hashed only up to these: more is "too big to check".
const TREE_MAX_FILES: usize = 2000;
const TREE_MAX_BYTES: u64 = 8 << 20;

/// Every regular file under `root` (path relative to it, with `/`), sorted by path as UTF-8
/// bytes. An error for a link anywhere inside, a name that is not UTF-8, or more than
/// [`TREE_MAX_FILES`] files or [`TREE_MAX_BYTES`] bytes.
fn tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let (mut files, mut total) = (Vec::new(), 0u64);
    let mut dirs = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = dirs.pop() {
        for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = e
                .file_name()
                .into_string()
                .map_err(|n| format!("{n:?} is not a UTF-8 name"))?;
            let rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let kind = e.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(format!("contains links ({rel})"));
            } else if kind.is_dir() {
                dirs.push((e.path(), rel));
            } else if kind.is_file() {
                let mut bytes = Vec::new();
                std::fs::File::open(e.path())
                    .and_then(|f| f.take(TREE_MAX_BYTES - total + 1).read_to_end(&mut bytes))
                    .map_err(|e| format!("{rel}: {e}"))?;
                total += bytes.len() as u64;
                files.push((rel, bytes));
                if files.len() > TREE_MAX_FILES || total > TREE_MAX_BYTES {
                    return Err(format!(
                        "too big to check (over {TREE_MAX_FILES} files or {} MiB)",
                        TREE_MAX_BYTES >> 20
                    ));
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// SHA-256 (lowercase hex) over the [`tree`] of a plugin folder: for each file its path, one 0
/// byte, its length as 8 bytes big-endian, then its bytes. The VS Code extension hashes the same.
fn tree_hash(files: &[(String, Vec<u8>)]) -> String {
    let mut all = Vec::new();
    for (path, bytes) in files {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        all.extend_from_slice(bytes);
    }
    sha256_hex(&all)
}

/// A project plugin in `folder` (under `<project>/.droid-lite/plugins`), read from the same bytes
/// it is hashed from. Refused when its real folder is not inside the project's real plugins
/// folder, or when [`tree`] refuses it.
fn project_plugin(folder: &Path, project: &Path, plugins: &Path) -> Result<Manifest, String> {
    let real = folder.canonicalize().map_err(|e| e.to_string())?;
    if real == plugins || !real.starts_with(plugins) {
        return Err(format!("links outside the project ({})", real.display()));
    }
    let files = tree(&real)?;
    let json = files
        .iter()
        .find(|(p, _)| p == "plugin.json")
        .ok_or("no plugin.json")?;
    let json = std::str::from_utf8(&json.1).map_err(|e| e.to_string())?;
    let mut m = Manifest::parse(json, folder)?;
    m.trust = Some(Trust {
        project: project.to_string_lossy().into_owned(),
        folder: real.to_string_lossy().into_owned(),
        hash: tree_hash(&files),
    });
    Ok(m)
}

/// Every `<dir>/<plugin>/plugin.json` under `user` and `<project>/.droid-lite/plugins`, sorted by
/// folder name; a broken or refused one is an error that names its file. A user plugin may run;
/// a project plugin (it comes with the project's files, so with anyone's commit) only once
/// `store` has it for this project with these files ([`is_approved`]).
pub fn discover_all(
    user: &[PathBuf],
    project: Option<&Path>,
    store: Option<&Path>,
) -> Vec<Result<Manifest, String>> {
    let project_dir = project.map(|p| p.join(".droid-lite/plugins"));
    // (real project, real plugins folder): with neither, no project plugin is found
    let real = project
        .and_then(|p| p.canonicalize().ok())
        .zip(project_dir.as_ref().and_then(|d| d.canonicalize().ok()));
    let mut found: Vec<(PathBuf, bool)> = user
        .iter()
        .map(|d| (d, false))
        .chain(real.as_ref().and(project_dir.as_ref()).map(|d| (d, true)))
        .filter_map(|(d, p)| std::fs::read_dir(d).ok().map(|rd| (rd, p)))
        .flat_map(|(rd, p)| rd.flatten().map(move |e| (e.path(), p)))
        .filter(|(f, _)| f.join("plugin.json").is_file())
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(f, in_project)| {
            let file = f.join("plugin.json");
            match &real {
                Some((root, plugins)) if in_project => {
                    project_plugin(&f, root, plugins).map(|mut m| {
                        m.allowed = store.is_some_and(|s| is_approved(s, &m));
                        m
                    })
                }
                _ => std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|j| Manifest::parse(&j, &f))
                    .map(|m| Manifest { allowed: true, ..m }),
            }
            .map_err(|e| format!("{}: {e}", file.display()))
        })
        .collect()
}

/// [`discover_all`] of the user's own plugin folders: every plugin there may run.
#[cfg(test)]
fn discover(dirs: &[PathBuf]) -> Vec<Result<Manifest, String>> {
    discover_all(dirs, None, None)
}

/// SHA-256 of `data` as lowercase hex (FIPS 180-4).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn status(v: &Value) -> Option<Status> {
    Some(match v.get("status")?.as_str()? {
        "running" => Status::Running,
        "passed" | "pass" | "ok" => Status::Passed,
        "failed" | "fail" | "error" => Status::Failed,
        "skipped" | "skip" => Status::Skipped,
        _ => return None,
    })
}

/// A relative `file` is relative to the project.
fn file(v: &Value, project: Option<&Path>) -> Option<PathBuf> {
    let f = expand_home(v.get("file")?.as_str()?);
    Some(match project {
        Some(p) if f.is_relative() => p.join(f),
        _ => f,
    })
}

fn line(v: &Value) -> usize {
    v.get("line").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn entry(v: &Value, project: Option<&Path>) -> Entry {
    let case = v.get("case").and_then(Value::as_str).map(str::to_owned);
    let label = match text(v, "label") {
        l if l.is_empty() => case.clone().unwrap_or_default(),
        l => l,
    };
    Entry {
        case,
        label,
        status: status(v),
        message: text(v, "message"),
        file: file(v, project),
        line: line(v),
        url: v.get("url").and_then(Value::as_str).map(str::to_owned),
    }
}

fn case(v: &Value, project: Option<&Path>) -> Case {
    let id = v.get("id").and_then(Value::as_str).map(str::to_owned);
    Case {
        label: match text(v, "label") {
            l if l.is_empty() => id.clone().unwrap_or_default(),
            l => l,
        },
        id,
        detail: text(v, "detail"),
        file: file(v, project),
        line: line(v),
        children: v
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| case(c, project))
            .collect(),
    }
}

/// The `list` command's output: a JSON array of cases.
pub fn parse_cases(json: &str, project: Option<&Path>) -> Result<Vec<Case>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let a = v.as_array().ok_or("the list is not a JSON array")?;
    Ok(a.iter().map(|c| case(c, project)).collect())
}

/// A view command's output: a JSON array of entries.
pub fn parse_entries(json: &str, project: Option<&Path>) -> Result<Vec<Entry>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let a = v.as_array().ok_or("the view is not a JSON array")?;
    Ok(a.iter().map(|e| entry(e, project)).collect())
}

/// One line of `run` or an action: a JSON object with a status is a result, anything else is log.
pub fn parse_event(line: &str, project: Option<&Path>) -> Event {
    let t = line.trim_start();
    if t.starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(t)
    {
        if v.get("status").is_some() {
            return Event::Result(entry(&v, project));
        }
        if let Some(l) = v.get("log").and_then(Value::as_str) {
            return Event::Log(l.to_owned());
        }
    }
    Event::Log(line.to_owned())
}

/// One line of `watch`; None when it is not a state object.
pub fn parse_live(line: &str) -> Option<Live> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    v.as_object()?;
    Some(Live {
        title: text(&v, "title"),
        status: status(&v),
        rows: v
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let r = r.as_array()?;
                Some((
                    r.first()?.as_str()?.to_owned(),
                    r.get(1).map_or(String::new(), |v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    }),
                ))
            })
            .collect(),
    })
}

/// Every runnable case under `cases` that names a file: (file, line, id, label).
pub fn file_cases(cases: &[Case], out: &mut Vec<(PathBuf, usize, String, String)>) {
    for c in cases {
        if let (Some(f), Some(id)) = (&c.file, &c.id) {
            out.push((f.clone(), c.line, id.clone(), c.label.clone()));
        }
        file_cases(&c.children, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_fills_defaults_and_placeholders() {
        let m = Manifest::parse(
            r#"{"name":"Demo","list":["./list.sh","{project}"],"run":["./run.sh","{id}"],
                "actions":[{"name":"Check","run":["./check.sh"]},{"name":"broken"}],
                "views":[{"name":"Failures","run":["./failures.sh"]}],
                "links":[{"name":"Live","url":"http://127.0.0.1:1"},{"name":"no url"}]}"#,
            Path::new("/p/demo"),
        )
        .unwrap();
        assert_eq!(m.dir, Path::new("/p/demo"));
        assert_eq!(m.actions.len(), 1, "an action without a command is dropped");
        assert_eq!(m.views[0].name, "Failures");
        assert_eq!(m.links, vec![("Live".into(), "http://127.0.0.1:1".into())]);
        assert!(m.watch.is_none());
        assert!(
            m.command(m.run.as_ref().unwrap(), None, "").is_err(),
            "not allowed: nothing runs"
        );
        let m = Manifest { allowed: true, ..m };
        let cmd = m
            .command(m.run.as_ref().unwrap(), Some(Path::new("/proj")), "a b")
            .unwrap();
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["a b"], "the id is one argument, never split");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/p/demo")));
        let list = m
            .command(m.list.as_ref().unwrap(), Some(Path::new("/proj")), "")
            .unwrap();
        assert_eq!(list.get_args().collect::<Vec<_>>(), ["/proj"]);
        assert!(Manifest::parse(r#"{"list":["x"]}"#, Path::new("/p")).is_err());
        assert!(Manifest::parse("not json", Path::new("/p")).is_err());
    }

    #[test]
    fn cases_nest_and_files_are_project_relative() {
        let c = parse_cases(
            r#"[{"label":"Group","children":[{"id":"a","detail":"d","file":"flows/a.yaml","line":3}]},
                {"id":"b","label":"B","file":"/abs/b.yaml"}]"#,
            Some(Path::new("/proj")),
        )
        .unwrap();
        assert_eq!(c[0].id, None);
        assert_eq!(
            c[0].children[0].label, "a",
            "the label falls back to the id"
        );
        let mut files = Vec::new();
        file_cases(&c, &mut files);
        assert_eq!(
            files,
            vec![
                (
                    PathBuf::from("/proj/flows/a.yaml"),
                    3,
                    "a".into(),
                    "a".into()
                ),
                (PathBuf::from("/abs/b.yaml"), 0, "b".into(), "B".into())
            ]
        );
        assert!(parse_cases("{}", None).is_err());
    }

    #[test]
    fn run_lines_are_results_or_log() {
        let Event::Result(e) = parse_event(
            r#"{"case":"ng","status":"failed","message":"stuck","url":"http://x/F-1"}"#,
            None,
        ) else {
            panic!("a status line is a result")
        };
        assert_eq!(
            (e.label.as_str(), e.status, e.message.as_str()),
            ("ng", Some(Status::Failed), "stuck")
        );
        assert_eq!(
            parse_event(r#"{"log":"hello"}"#, None),
            Event::Log("hello".into())
        );
        assert_eq!(
            parse_event("plain {text}", None),
            Event::Log("plain {text}".into())
        );
        assert_eq!(
            parse_event(r#"{"other":1}"#, None),
            Event::Log(r#"{"other":1}"#.into())
        );
    }

    #[test]
    fn live_state_rows() {
        let l = parse_live(
            r#"{"title":"Run ng","status":"running","rows":[["Phone","Mi A3"],["Step",4]]}"#,
        )
        .unwrap();
        assert_eq!(l.status, Some(Status::Running));
        assert_eq!(l.rows[1], ("Step".into(), "4".into()));
        assert!(parse_live("[1]").is_none());
        assert!(parse_live("starting…").is_none());
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let two_blocks = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(two_blocks),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// A fresh temp folder for one test.
    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("droid-lite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The plugins of `project` (no user plugins), each unwrapped.
    fn project_plugins(project: &Path, store: Option<&Path>) -> Vec<Manifest> {
        discover_all(&[], Some(project), store)
            .into_iter()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_project_plugin_runs_only_once_allowed_and_again_after_any_file_of_it_changes() {
        let d = temp("approve");
        let (user, project) = (d.join("config/plugins"), d.join("proj"));
        let shared = project.join(".droid-lite/plugins/shared");
        std::fs::create_dir_all(user.join("mine")).unwrap();
        std::fs::create_dir_all(shared.join("bin")).unwrap();
        std::fs::write(
            user.join("mine/plugin.json"),
            r#"{"name":"Mine","list":["./list.sh"]}"#,
        )
        .unwrap();
        let json = r#"{"name":"Shared","list":["sh","-c","touch pwned"],"actions":[{"name":"Go","run":["./bin/go.sh"]}]}"#;
        std::fs::write(shared.join("plugin.json"), json).unwrap();
        std::fs::write(shared.join("bin/go.sh"), "echo go\n").unwrap();
        let store = approvals_file(&d.join("config"));
        let found = |s: Option<&Path>| -> Vec<Manifest> {
            discover_all(std::slice::from_ref(&user), Some(&project), s)
                .into_iter()
                .map(Result::unwrap)
                .collect()
        };
        let [mine, sh] = &found(Some(&store))[..] else {
            panic!()
        };
        assert!(mine.allowed, "the user's own plugin needs no OK");
        assert!(mine.trust.is_none());
        assert!(!sh.allowed, "a project plugin waits for the user's OK");
        assert!(
            sh.command(sh.list.as_ref().unwrap(), None, "").is_err(),
            "and runs nothing meanwhile"
        );
        assert_eq!(
            sh.approval_text(),
            format!(
                "Needs your OK: runs [\"sh\", \"-c\", \"touch pwned\"], [\"./bin/go.sh\"] from {}\n\
                 Allowing trusts this plugin's folder. Commands may also run project code outside \
                 it (for example a script in the repo); changes to that code do not ask again.",
                shared.display()
            )
        );
        // another project's entry, kept by every write
        std::fs::write(
            &store,
            r#"{"version":2,"approved":{"/other":{"/other/.droid-lite/plugins/p":"00"}}}"#,
        )
        .unwrap();
        approve(&store, sh).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(
            v["approved"]["/other"]["/other/.droid-lite/plugins/p"],
            "00"
        );
        let (root, real) = (
            project.canonicalize().unwrap(),
            shared.canonicalize().unwrap(),
        );
        assert_eq!(
            v["approved"][root.to_str().unwrap()][real.to_str().unwrap()],
            tree_hash(&tree(&shared).unwrap()),
            "project real path -> folder real path -> tree hash"
        );
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&store).unwrap().permissions()
            ) & 0o777,
            0o600
        );
        let mut left: Vec<_> = std::fs::read_dir(d.join("config"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "approved-plugins.json",
                "approved-plugins.json.lock",
                "plugins"
            ],
            "no temp file left"
        );
        let sh = found(Some(&store)).remove(1);
        assert!(sh.allowed);
        assert!(sh.command(sh.list.as_ref().unwrap(), None, "").is_ok());
        assert!(!found(None)[1].allowed, "no approvals file: not allowed");
        // a changed script in the folder needs the OK again, and so does a new file
        std::fs::write(shared.join("bin/go.sh"), "rm -rf ~\n").unwrap();
        assert!(!found(Some(&store))[1].allowed);
        approve(&store, &found(Some(&store))[1]).unwrap();
        assert!(found(Some(&store))[1].allowed);
        std::fs::write(shared.join("bin/extra.sh"), "").unwrap();
        assert!(!found(Some(&store))[1].allowed);
        // and a changed plugin.json
        std::fs::remove_file(shared.join("bin/extra.sh")).unwrap();
        assert!(found(Some(&store))[1].allowed);
        std::fs::write(
            shared.join("plugin.json"),
            json.replace("touch pwned", "rm -rf ~"),
        )
        .unwrap();
        assert!(!found(Some(&store))[1].allowed);
        // a broken approvals file is never overwritten (it may hold other approvals)
        std::fs::write(&store, "[").unwrap();
        assert!(approve(&store, &found(Some(&store))[1]).is_err());
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "[");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_project_plugin_that_links_outside_the_project_or_holds_links_is_refused() {
        let d = temp("refuse");
        let project = d.join("proj");
        let plugins = project.join(".droid-lite/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/plugin.json"), r#"{"name":"Out"}"#).unwrap();
        std::os::unix::fs::symlink(d.join("outside"), plugins.join("out")).unwrap();
        // inside the plugins folder: a link there is fine
        std::fs::create_dir_all(plugins.join("real")).unwrap();
        std::fs::write(plugins.join("real/plugin.json"), r#"{"name":"Real"}"#).unwrap();
        std::os::unix::fs::symlink(plugins.join("real"), plugins.join("same")).unwrap();
        // a link inside the plugin's folder
        std::fs::create_dir_all(plugins.join("inner")).unwrap();
        std::fs::write(plugins.join("inner/plugin.json"), r#"{"name":"Inner"}"#).unwrap();
        std::os::unix::fs::symlink("/bin/sh", plugins.join("inner/run")).unwrap();
        let store = approvals_file(&d.join("config"));
        let found = discover_all(&[], Some(&project), Some(&store));
        let [inner, out, real, same] = &found[..] else {
            panic!("{found:?}")
        };
        assert!(
            inner.as_ref().unwrap_err().contains("contains links"),
            "{inner:?}"
        );
        assert!(
            out.as_ref()
                .unwrap_err()
                .contains("links outside the project"),
            "{out:?}"
        );
        let (real, same) = (real.as_ref().unwrap(), same.as_ref().unwrap());
        assert_eq!(real.trust, same.trust, "one folder, one key");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_plugin_folder_too_big_to_check_is_refused() {
        let d = temp("big");
        let p = d.join("proj/.droid-lite/plugins/big");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("plugin.json"), r#"{"name":"Big"}"#).unwrap();
        std::fs::write(p.join("blob"), vec![0u8; TREE_MAX_BYTES as usize]).unwrap();
        let err = discover_all(&[], Some(&d.join("proj")), None).remove(0);
        assert!(err.unwrap_err().contains("too big to check"));
        std::fs::remove_file(p.join("blob")).unwrap();
        for i in 0..TREE_MAX_FILES {
            std::fs::write(p.join(format!("f{i}")), "").unwrap();
        }
        let err = discover_all(&[], Some(&d.join("proj")), None).remove(0);
        assert!(err.unwrap_err().contains("too big to check"));
        std::fs::remove_file(p.join("f0")).unwrap();
        assert!(
            discover_all(&[], Some(&d.join("proj")), None)[0].is_ok(),
            "at the cap: fine"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn an_ok_for_one_project_is_not_an_ok_for_another_with_the_same_plugin_folder() {
        let d = temp("per-project");
        let a = d.join("a");
        std::fs::create_dir_all(a.join(".droid-lite/plugins/p")).unwrap();
        std::fs::write(
            a.join(".droid-lite/plugins/p/plugin.json"),
            r#"{"name":"P"}"#,
        )
        .unwrap();
        // project b's .droid-lite is project a's: the same real plugin folder
        std::fs::create_dir_all(d.join("b")).unwrap();
        std::os::unix::fs::symlink(a.join(".droid-lite"), d.join("b/.droid-lite")).unwrap();
        let store = approvals_file(&d.join("config"));
        let pa = project_plugins(&a, Some(&store)).remove(0);
        approve(&store, &pa).unwrap();
        assert!(project_plugins(&a, Some(&store))[0].allowed);
        let pb = project_plugins(&d.join("b"), Some(&store)).remove(0);
        assert_eq!(
            pb.trust.as_ref().unwrap().folder,
            pa.trust.as_ref().unwrap().folder
        );
        assert!(!pb.allowed, "allowed for a, not for b");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_old_flat_approvals_file_asks_again_and_the_first_ok_replaces_it() {
        let d = temp("v1");
        let p = d.join("proj/.droid-lite/plugins/p");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("plugin.json"), r#"{"name":"P"}"#).unwrap();
        let store = approvals_file(&d.join("config"));
        std::fs::create_dir_all(d.join("config")).unwrap();
        let m = project_plugins(&d.join("proj"), None).remove(0);
        let t = m.trust.clone().unwrap();
        // what the first version kept: folder real path -> hash
        let v1 = serde_json::json!({ &t.folder: &t.hash, "/x": "00" });
        std::fs::write(&store, v1.to_string()).unwrap();
        assert!(!project_plugins(&d.join("proj"), Some(&store))[0].allowed);
        approve(&store, &m).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"version": 2, "approved": {&t.project: {&t.folder: &t.hash}}}),
            "v1 entries dropped"
        );
        assert!(project_plugins(&d.join("proj"), Some(&store))[0].allowed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_oks_at_once_keep_both_entries() {
        let d = temp("concurrent");
        let store = approvals_file(&d.join("config"));
        for round in 0..20 {
            let threads: Vec<_> = (0..4)
                .map(|i| {
                    let m = Manifest {
                        name: "P".into(),
                        trust: Some(Trust {
                            project: "/proj".into(),
                            folder: format!("/proj/.droid-lite/plugins/p{round}-{i}"),
                            hash: "ab".into(),
                        }),
                        ..Default::default()
                    };
                    let store = store.clone();
                    std::thread::spawn(move || approve(&store, &m).unwrap())
                })
                .collect();
            threads.into_iter().for_each(|t| t.join().unwrap());
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["approved"]["/proj"].as_object().unwrap().len(), 80);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The vector the VS Code extension's test uses too.
    #[test]
    fn tree_hash_of_a_fixed_folder() {
        let d = temp("vector");
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a.txt"), "x").unwrap();
        std::fs::write(d.join("a/b"), "").unwrap();
        std::fs::write(d.join("plugin.json"), r#"{"name":"T"}"#).unwrap();
        let files = tree(&d).unwrap();
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["a.txt", "a/b", "plugin.json"],
            "sorted as bytes: '.' < '/'"
        );
        assert_eq!(
            tree_hash(&files),
            "785f791d17fa1ef53828f417bb59ba3da0d7e7e426a16e1839df07d75c88577f"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dir_is_resolved_from_the_real_folder_by_text() {
        let d = temp("dir");
        std::fs::create_dir_all(d.join("tool/plugin")).unwrap();
        std::fs::create_dir_all(d.join("elsewhere/deep")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("elsewhere/deep"), d.join("tool/plugin/sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("tool/plugin"), d.join("link")).unwrap();
        let real = d.join("tool/plugin").canonicalize().unwrap();
        let m =
            Manifest::parse(r#"{"name":"T","dir":"sub/../x/./y/.."}"#, &d.join("link")).unwrap();
        assert_eq!(m.dir, real.join("x"), "path.resolve(real folder, dir)");
        assert_eq!(m.folder, d.join("link"), "{{plugin}} stays as found");
        let m = Manifest::parse(r#"{"name":"T","dir":"/abs/../q"}"#, &d.join("link")).unwrap();
        assert_eq!(m.dir, Path::new("/q"));
        let m = Manifest::parse(r#"{"name":"T"}"#, &d.join("link")).unwrap();
        assert_eq!(m.dir, real);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_reads_each_folder_and_names_broken_files() {
        let d = std::env::temp_dir().join(format!("droid-lite-plugins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("b-good")).unwrap();
        std::fs::create_dir_all(d.join("a-bad")).unwrap();
        std::fs::create_dir_all(d.join("c-none")).unwrap();
        std::fs::write(d.join("b-good/plugin.json"), r#"{"name":"Good"}"#).unwrap();
        std::fs::write(d.join("a-bad/plugin.json"), "{").unwrap();
        let found = discover(&[d.clone(), d.join("missing")]);
        assert_eq!(found.len(), 2);
        assert!(found[0].as_ref().unwrap_err().contains("a-bad/plugin.json"));
        assert_eq!(found[1].as_ref().unwrap().name, "Good");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn add_links_the_plugin_once_and_remove_drops_only_the_link() {
        let d = std::env::temp_dir().join(format!("droid-lite-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let tool = d.join("tool");
        std::fs::create_dir_all(tool.join("droid-lite-plugin")).unwrap();
        std::fs::write(
            tool.join("droid-lite-plugin/plugin.json"),
            r#"{"name":"My Tool","dir":".."}"#,
        )
        .unwrap();
        let plugins = d.join("plugins");
        assert!(add(&plugins, &d).is_err(), "no plugin.json there");
        assert_eq!(add(&plugins, &tool).unwrap(), "My Tool");
        assert_eq!(
            add(&plugins, &tool.join("droid-lite-plugin")).unwrap(),
            "My Tool"
        );
        let found = discover(std::slice::from_ref(&plugins));
        assert_eq!(found.len(), 1, "added once");
        let m = found[0].as_ref().unwrap();
        assert_eq!(m.folder, plugins.join("my-tool"));
        // "dir": ".." is the tool's folder, through the link
        assert_eq!(m.dir.canonicalize().unwrap(), tool.canonicalize().unwrap());
        remove(&plugins, m).unwrap();
        assert!(discover(std::slice::from_ref(&plugins)).is_empty());
        assert!(
            tool.join("droid-lite-plugin/plugin.json").is_file(),
            "the folder stays"
        );
        // a real folder in the plugins folder is never deleted
        std::fs::create_dir_all(plugins.join("own")).unwrap();
        std::fs::write(plugins.join("own/plugin.json"), r#"{"name":"Own"}"#).unwrap();
        let own = discover(std::slice::from_ref(&plugins)).remove(0).unwrap();
        assert!(remove(&plugins, &own).is_err());
        assert!(plugins.join("own/plugin.json").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }
}
