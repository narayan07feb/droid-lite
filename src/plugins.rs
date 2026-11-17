//! Plugins: outside tools (test runners, checkers) that Droid Lite drives through a small JSON
//! contract. A plugin is a folder with a `plugin.json`; the contract is in docs/PLUGINS.md.
//! Nothing here knows any one tool: the plugin's own commands print JSON, Droid Lite shows it.

use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A command (argv) with a name, for an action or a view.
#[derive(Clone, Debug, PartialEq)]
pub struct Named {
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    pub name: String,
    /// The plugin's folder, as found.
    pub folder: PathBuf,
    /// Its real path (links resolved): `{plugin}`, as in the VS Code extension.
    pub real: PathBuf,
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
    /// Why a project plugin can no longer run at all: its folder, checked again, links outside the
    /// project, holds a link or is too big. It can never run or be allowed (no OK to give).
    pub broken: Option<String>,
    /// A project plugin's folder as the walk before its last full read saw it (each file's path,
    /// size, times, inode and mode, [`Walk::stat`]): while it still looks the same, a check needs
    /// no new read of every byte ([`Manifest::recheck`]).
    pub seen: String,
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
            None => real.clone(),
        };
        let links = v
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|l| (text(l, "name"), text(l, "url")))
            .filter(|(_, u)| is_web_link(u))
            .collect();
        Ok(Manifest {
            name,
            folder: folder.to_path_buf(),
            real,
            dir,
            list: v.get("list").and_then(argv),
            run: v.get("run").and_then(argv),
            watch: v.get("watch").and_then(argv),
            actions: named(&v, "actions"),
            views: named(&v, "views"),
            links,
            trust: None,
            allowed: false,
            broken: None,
            seen: String::new(),
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

    /// What the Tests tab shows for a project plugin that is not allowed yet: its commands, and
    /// where they run when that is not its own folder (`dir`), as the VS Code extension's OK does.
    pub fn approval_text(&self) -> String {
        let cmds: Vec<String> = self.commands().iter().map(|a| format!("{a:?}")).collect();
        let dir = if self.dir == self.real {
            String::new()
        } else {
            format!("\nRuns in: {}", self.dir.display())
        };
        format!(
            "Needs your OK: runs {} from {}{dir}\nAllowing trusts this plugin's folder. Commands may \
             also run project code outside it (for example a script in the repo); changes to that \
             code do not ask again.",
            cmds.join(", "),
            self.folder.display()
        )
    }

    /// The command for `argv`, with `{plugin}` (the real folder), `{project}` and `{id}` filled
    /// in, run in `dir`. Its stdin is empty: a plugin must never wait for typed input. An error for
    /// a project plugin the user has not allowed ([`Manifest::allowed`]): nothing of it runs. A
    /// project plugin's folder is checked again first ([`Manifest::recheck`]), and Python writes
    /// no `__pycache__` into it (that would change the folder and ask for the OK again).
    pub fn command(
        &mut self,
        argv: &[String],
        project: Option<&Path>,
        id: &str,
    ) -> Result<Command, String> {
        if !self.allowed {
            return Err(match &self.broken {
                Some(why) => why.clone(),
                None => format!("{} needs your OK first (Tests tab)", self.name),
            });
        }
        self.recheck()?;
        let project = project.map(|p| p.display().to_string()).unwrap_or_default();
        let fill = |s: &str| {
            s.replace("{plugin}", &self.real.display().to_string())
                .replace("{project}", &project)
                .replace("{id}", id)
        };
        let mut cmd = Command::new(expand_home(&fill(&argv[0])));
        cmd.args(argv[1..].iter().map(|a| fill(a)))
            .current_dir(&self.dir)
            .env("DROID_LITE_PROJECT", &project)
            .stdin(std::process::Stdio::null());
        if self.trust.is_some() {
            cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        }
        // its own process group, so Stop reaches the tools it starts too
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        Ok(cmd)
    }

    /// A project plugin's folder as it is on disk now must be the one the user allowed: read
    /// again (the same checks as when it was found) and hashed. A folder whose every file still
    /// looks as it did before the last full read ([`Manifest::seen`]) is not read again. When it is
    /// not the one allowed, it is no longer allowed and is replaced by what its folder holds now
    /// (so the OK shows the new commands), and the error says to allow it again; so it does when a
    /// file was gone or locked for a moment (a save, a `git checkout`). When the folder can no
    /// longer be checked at all (links outside, too big), it is [`Manifest::broken`]. The user's
    /// own plugins pass.
    pub fn recheck(&mut self) -> Result<(), String> {
        let Some(t) = &self.trust else {
            return Ok(());
        };
        let root = Path::new(&t.project);
        let plugins = root
            .join(".droid-lite/plugins")
            .canonicalize()
            .map_err(|e| io_refusal(".droid-lite/plugins", e));
        let looks_same = plugins
            .as_ref()
            .ok()
            .and_then(|p| real_inside(&self.folder, root, p).ok())
            .filter(|real| real.as_os_str() == Path::new(&t.folder).as_os_str())
            .and_then(|real| walk(&real).ok())
            .is_some_and(|w| !self.seen.is_empty() && w.stat == self.seen);
        if looks_same {
            return Ok(());
        }
        let now = plugins.and_then(|plugins| project_plugin(&self.folder, root, &plugins));
        if let Ok(m) = &now
            && m.trust.as_ref() == Some(t)
        {
            self.seen = m.seen.clone();
            return Ok(());
        }
        self.allowed = false;
        match now {
            Ok(m) => *self = m,
            Err(r) if r.broken => {
                let why = format!(
                    "{} changed since you allowed it and cannot run now ({})",
                    self.name, r.why
                );
                self.broken = Some(why.clone());
                return Err(why);
            }
            // gone or locked for a moment: changed, and it may be allowed again
            Err(_) => {}
        }
        Err(format!(
            "{} changed since you allowed it; allow it again",
            self.name
        ))
    }
}

/// A link a plugin may show and open: an `http://` or `https://` page (as the VS Code extension
/// takes), never a file, a share (`smb://`) or another app's scheme.
pub fn is_web_link(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://")
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
/// `git pull`). Returns its name and its real folder ([`Manifest::real`], its id: two plugins may
/// share a name); a plugin already linked is not added twice.
pub fn add(plugins: &Path, picked: &Path) -> Result<(String, PathBuf), String> {
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
        return Ok((m.name, folder));
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
    Ok((m.name, folder))
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

/// Version 2 as a JSON number (`2.0` too, as the VS Code extension reads it), with an object of
/// approvals.
fn is_v2(v: &Value) -> bool {
    v["version"].as_f64() == Some(2.0) && v["approved"].is_object()
}

/// Whether the user allowed this project plugin, for this project, with exactly these files.
pub fn is_approved(store: &Path, m: &Manifest) -> bool {
    let (Some(t), Ok(Some(v))) = (&m.trust, read_store(store)) else {
        return false;
    };
    is_v2(&v) && v["approved"][&t.project][&t.folder].as_str() == Some(t.hash.as_str())
}

/// How long [`approve`] waits for the lock, and the age at which a lock counts as left by a
/// process that died (as in the VS Code extension).
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_STALE: Duration = Duration::from_secs(30);

/// A number for names and lock tokens that no other call (of this or another process) uses.
fn unique() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::hash::BuildHasher::hash_one(
        &std::collections::hash_map::RandomState::new(),
        (
            std::time::SystemTime::now(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ),
    )
}

/// `<store>.lock`, holding this holder's token (its pid and a random number) until dropped.
struct Lock {
    path: PathBuf,
    token: String,
}

impl Drop for Lock {
    /// Removes the lock only while it still holds this token: one taken away as stale meanwhile
    /// (and made again by another process) is not ours.
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).is_ok_and(|t| t == self.token) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Takes the lock file `path` the way the VS Code extension does: created only when it does not
/// exist yet, with this holder's token in it, else tried again with a growing pause for up to
/// `patience`. Every try that fails waits and checks the time, whatever is in the way: a lock file
/// older than [`LOCK_STALE`] is taken away ([`remove_stale`]); anything else (a fresh lock, a
/// folder, a link, a stale lock that could not be taken away) is held.
fn lock(path: PathBuf, patience: Duration) -> Result<Lock, String> {
    let end = Instant::now() + patience;
    let mut wait = Duration::from_millis(10);
    let token = format!("{} {:016x}", std::process::id(), unique());
    loop {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        match o.open(&path) {
            Ok(mut f) => {
                return match f.write_all(token.as_bytes()) {
                    Ok(()) => Ok(Lock { path, token }),
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        Err(format!("{}: {e}", path.display()))
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        if let Ok(md) = std::fs::symlink_metadata(&path)
            && is_stale(&md)
        {
            remove_stale(&path, &md);
        }
        if Instant::now() > end {
            return Err(format!(
                "{} is held by another Droid Lite; try again",
                path.display()
            ));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(250));
    }
}

/// A lock file left by a process that died: its time is more than [`LOCK_STALE`] away from now, in
/// the past or (a clock set back, a copied file) in the future.
fn is_stale(md: &std::fs::Metadata) -> bool {
    md.is_file()
        && md.modified().is_ok_and(|t| match t.elapsed() {
            Ok(age) => age > LOCK_STALE,
            Err(ahead) => ahead.duration() > LOCK_STALE,
        })
}

/// Takes away the stale lock file `path` that `seen` described: looked at again right before (a
/// fresh lock in its place is left alone), renamed to a name of its own, then deleted only when
/// what was renamed is still that file. Another process may have taken the stale one away and made
/// a fresh lock meanwhile: that one is put back; when it cannot be put back (a newer lock took its
/// place), it stays under its aside name and is never deleted (someone may hold it).
fn remove_stale(path: &Path, seen: &std::fs::Metadata) {
    remove_stale_with(path, seen, &mut |_| {});
}

/// [`remove_stale`], calling `step` before the rename (`"rename"`) and before the put back (`"put
/// back"`), where tests play the other process.
fn remove_stale_with(path: &Path, seen: &std::fs::Metadata, step: &mut dyn FnMut(&str)) {
    let still =
        std::fs::symlink_metadata(path).is_ok_and(|now| same_file(&now, seen) && is_stale(&now));
    if !still {
        return;
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".stale.{}.{:016x}", std::process::id(), unique()));
    let aside = PathBuf::from(aside);
    step("rename");
    if std::fs::rename(path, &aside).is_err() {
        return;
    }
    if std::fs::symlink_metadata(&aside).is_ok_and(|now| same_file(&now, seen)) {
        let _ = std::fs::remove_file(&aside);
        return;
    }
    step("put back");
    match std::fs::hard_link(&aside, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&aside);
        }
        Err(e) => eprintln!(
            "droid-lite: left a lock taken by mistake at {} ({e})",
            aside.display()
        ),
    }
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    // a new file may get a freed inode number again: its time tells it apart
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    };
    #[cfg(not(unix))]
    let same = a.len() == b.len();
    same && a.modified().ok() == b.modified().ok()
}

/// Allows a project plugin as it was read ([`Manifest::trust`]), every other entry kept. Under
/// the lock file `<store>.lock` ([`lock`]), shared with the VS Code extension, so two windows (or
/// the extension) allowing at once keep both; written through a 0600 temp file and a rename. A
/// file that is not a JSON object is never overwritten; one of another shape (the older flat map)
/// is replaced.
pub fn approve(store: &Path, m: &Manifest) -> Result<(), String> {
    if let Some(why) = &m.broken {
        return Err(why.clone());
    }
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
    let _lock = lock(dir.join(format!("{name}.lock")), LOCK_WAIT)?;
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
    let tmp = dir.join(format!(
        "{name}.{}.{:016x}.tmp",
        std::process::id(),
        unique()
    ));
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

/// Why a project plugin's folder cannot be used now. `broken`: it never can (links outside, a link
/// inside, a name that is not UTF-8, too big). Else it changed while it was read: a file was gone
/// or locked for a moment (an atomic save, a `git checkout`), or something other than a file took
/// a file's place; it may be allowed again.
#[derive(Debug)]
struct Refusal {
    why: String,
    broken: bool,
}

impl Refusal {
    fn broken(why: impl Into<String>) -> Refusal {
        Refusal {
            why: why.into(),
            broken: true,
        }
    }

    fn changed(why: impl Into<String>) -> Refusal {
        Refusal {
            why: why.into(),
            broken: false,
        }
    }
}

/// An error reading `rel` of a plugin folder: a link in a file's place is "contains links"; a file
/// gone or locked for a moment is a change; anything else breaks it.
fn io_refusal(rel: &str, e: std::io::Error) -> Refusal {
    if is_link_error(&e) {
        return Refusal::broken(format!("contains links ({rel})"));
    }
    let why = format!("{rel}: {e}");
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            Refusal::changed(why)
        }
        _ => Refusal::broken(why),
    }
}

fn too_big() -> Refusal {
    Refusal::broken(format!(
        "too big to check (over {TREE_MAX_FILES} files or {} MiB)",
        TREE_MAX_BYTES >> 20
    ))
}

/// Every regular file under a plugin folder, as the walk found it.
struct Walk {
    /// (path relative to the folder with `/`, path), sorted by the relative path as UTF-8 bytes.
    files: Vec<(String, PathBuf)>,
    /// A digest of what the walk saw of each file (its path, size, times, inode, device and mode):
    /// the same digest later means the same bytes, without reading them ([`Manifest::seen`]).
    stat: String,
}

/// Walks the plugin folder `root` without reading any file. An error for a link anywhere inside, a
/// name that is not UTF-8, or more than [`TREE_MAX_FILES`] files or [`TREE_MAX_BYTES`] bytes.
fn walk(root: &Path) -> Result<Walk, Refusal> {
    let (mut files, mut total) = (Vec::new(), 0u64);
    let mut dirs = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = dirs.pop() {
        let here = if rel.is_empty() { "." } else { rel.as_str() };
        for e in std::fs::read_dir(&dir).map_err(|e| io_refusal(here, e))? {
            let e = e.map_err(|e| io_refusal(here, e))?;
            let name = utf8_name(e.file_name()).map_err(Refusal::broken)?;
            let rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            // lstat: a link is seen as a link
            let md = e.metadata().map_err(|err| io_refusal(&rel, err))?;
            if md.is_symlink() {
                return Err(Refusal::broken(format!("contains links ({rel})")));
            } else if md.is_dir() {
                dirs.push((e.path(), rel));
            } else if md.is_file() {
                total += md.len();
                files.push((rel, e.path(), stat_key(&md)));
                if files.len() > TREE_MAX_FILES || total > TREE_MAX_BYTES {
                    return Err(too_big());
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut stat = Sha256::new();
    for (rel, _, key) in &files {
        stat.update(rel.as_bytes());
        stat.update(&[0]);
        for k in key {
            stat.update(&k.to_be_bytes());
        }
    }
    Ok(Walk {
        files: files.into_iter().map(|(r, p, _)| (r, p)).collect(),
        stat: stat.hex(),
    })
}

/// What a check compares of a file without reading it. Any write changes its change time (`ctime`,
/// which no tool can set back), a replaced file its inode.
fn stat_key(md: &std::fs::Metadata) -> [i64; 8] {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        [
            md.size() as i64,
            md.mtime(),
            md.mtime_nsec(),
            md.ctime(),
            md.ctime_nsec(),
            md.ino() as i64,
            md.dev() as i64,
            md.mode() as i64,
        ]
    }
    #[cfg(not(unix))]
    {
        let t = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default();
        [
            md.len() as i64,
            t.as_secs() as i64,
            t.subsec_nanos() as i64,
            0,
            0,
            0,
            0,
            0,
        ]
    }
}

/// A file name as the folder hash takes it: refused when it is not UTF-8 (the VS Code extension
/// says the same).
fn utf8_name(n: std::ffi::OsString) -> Result<String, String> {
    n.into_string()
        .map_err(|n| format!("{} is not a UTF-8 name", n.to_string_lossy()))
}

/// Opens `p` for reading: a link in its place is an error ([`is_link_error`]), never followed, and
/// a FIFO opens without waiting for a writer (the caller then sees it is no file).
fn open_no_link(p: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    o.open(p)
}

fn is_link_error(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::ELOOP);
    #[cfg(not(unix))]
    return {
        let _ = e;
        false
    };
}

/// The permission bits of a file (`mode & 0o777`); 0 where there are none.
fn mode_bits(md: &std::fs::Metadata) -> u16 {
    #[cfg(unix)]
    return (std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o777) as u16;
    #[cfg(not(unix))]
    return {
        let _ = md;
        0
    };
}

/// SHA-256 (lowercase hex) over the files of a [`walk`] of a plugin folder, read one piece at a
/// time: for each file its path, one 0 byte, its length as 8 bytes big-endian, its permission bits
/// as 2 bytes big-endian, then its bytes. The VS Code extension hashes the same. Also gives the
/// bytes of `plugin.json` it hashed. A file that is no longer a regular file, or whose length
/// changed while it was read, is a change; one grown past [`TREE_MAX_BYTES`] is too big.
fn tree_hash(w: &Walk) -> Result<(String, Option<Vec<u8>>), Refusal> {
    #[cfg(test)]
    tests::READS.with(|r| r.set(r.get() + 1));
    let (mut h, mut total, mut manifest) = (Sha256::new(), 0u64, None);
    let mut buf = vec![0u8; 64 << 10];
    for (rel, path) in &w.files {
        let f = open_no_link(path).map_err(|e| io_refusal(rel, e))?;
        let md = f.metadata().map_err(|e| io_refusal(rel, e))?;
        if !md.is_file() {
            return Err(Refusal::changed(format!("{rel} is no longer a file")));
        }
        let len = md.len();
        total += len;
        if total > TREE_MAX_BYTES {
            return Err(too_big());
        }
        h.update(rel.as_bytes());
        h.update(&[0]);
        h.update(&len.to_be_bytes());
        h.update(&mode_bits(&md).to_be_bytes());
        let mut keep = (rel == "plugin.json").then(Vec::new);
        let (mut r, mut n) = (f.take(len + 1), 0u64);
        loop {
            let k = r.read(&mut buf).map_err(|e| io_refusal(rel, e))?;
            if k == 0 || n + k as u64 > len {
                n += k as u64;
                break;
            }
            n += k as u64;
            h.update(&buf[..k]);
            if let Some(keep) = &mut keep {
                keep.extend_from_slice(&buf[..k]);
            }
        }
        if n != len {
            return Err(Refusal::changed(format!("{rel} changed while it was read")));
        }
        if keep.is_some() {
            manifest = keep;
        }
    }
    Ok((h.hex(), manifest))
}

/// The real path of the plugin `folder` when it is inside the real plugins folder, itself inside
/// the real project (no linked `.droid-lite` or `.droid-lite/plugins`).
fn real_inside(folder: &Path, project: &Path, plugins: &Path) -> Result<PathBuf, Refusal> {
    let real = folder
        .canonicalize()
        .map_err(|e| io_refusal(&folder.display().to_string(), e))?;
    let inside = |dir: &Path, p: &Path| p != dir && p.starts_with(dir);
    if !inside(project, plugins) || !inside(plugins, &real) {
        return Err(Refusal::broken(format!(
            "links outside the project ({})",
            real.display()
        )));
    }
    Ok(real)
}

/// A project plugin in `folder` (under `<project>/.droid-lite/plugins`), read from the same bytes
/// it is hashed from. `project` and `plugins` are real paths. Refused when [`real_inside`],
/// [`walk`] or [`tree_hash`] refuses it.
fn project_plugin(folder: &Path, project: &Path, plugins: &Path) -> Result<Manifest, Refusal> {
    let real = real_inside(folder, project, plugins)?;
    let w = walk(&real)?;
    let (hash, json) = tree_hash(&w)?;
    let json = json.ok_or_else(|| Refusal::changed("no plugin.json"))?;
    let json = std::str::from_utf8(&json).map_err(|e| Refusal::broken(e.to_string()))?;
    let mut m = Manifest::parse(json, folder).map_err(Refusal::broken)?;
    m.trust = Some(Trust {
        project: project.to_string_lossy().into_owned(),
        folder: real.to_string_lossy().into_owned(),
        hash,
    });
    m.seen = w.stat;
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
                Some((root, plugins)) if in_project => project_plugin(&f, root, plugins)
                    .map(|mut m| {
                        m.allowed = store.is_some_and(|s| is_approved(s, &m));
                        m
                    })
                    .map_err(|r| r.why),
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

/// SHA-256 (FIPS 180-4), fed one piece at a time.
struct Sha256 {
    h: [u32; 8],
    /// Bytes not yet in a full 64-byte block.
    buf: Vec<u8>,
    len: u64,
}

impl Sha256 {
    fn new() -> Sha256 {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: Vec::with_capacity(64),
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if !self.buf.is_empty() {
            let take = (64 - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() < 64 {
                return;
            }
            let block: [u8; 64] = self.buf[..].try_into().expect("64 bytes");
            sha256_block(&mut self.h, &block);
            self.buf.clear();
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for block in blocks {
            sha256_block(&mut self.h, block);
        }
        self.buf.extend_from_slice(rest);
    }

    /// The digest as lowercase hex.
    fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf.len() != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

/// SHA-256 of `data` as lowercase hex.
#[cfg(test)]
fn sha256_hex(data: &[u8]) -> String {
    let mut s = Sha256::new();
    s.update(data);
    s.hex()
}

fn sha256_block(h: &mut [u32; 8], block: &[u8; 64]) {
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
    let mut v = *h;
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
        url: v
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| is_web_link(u))
            .map(str::to_owned),
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
        let (run, list) = (m.run.clone().unwrap(), m.list.clone().unwrap());
        let mut m = m;
        assert!(
            m.command(&run, None, "").is_err(),
            "not allowed: nothing runs"
        );
        m.allowed = true;
        let cmd = m.command(&run, Some(Path::new("/proj")), "a b").unwrap();
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["a b"], "the id is one argument, never split");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/p/demo")));
        let list = m.command(&list, Some(Path::new("/proj")), "").unwrap();
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

    /// The tree hash of the folder `d`.
    fn hash_of(d: &Path) -> String {
        tree_hash(&walk(d).unwrap()).unwrap().0
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
            sh.clone()
                .command(sh.list.as_ref().unwrap(), None, "")
                .is_err(),
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
            hash_of(&shared),
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
            ["approved-plugins.json", "plugins"],
            "no temp or lock file left"
        );
        let mut sh = found(Some(&store)).remove(1);
        assert!(sh.allowed);
        assert!(sh.command(&sh.list.clone().unwrap(), None, "").is_ok());
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

    #[test]
    fn an_ok_for_one_project_is_not_an_ok_for_another() {
        let d = temp("per-project");
        let a = d.join("a/.droid-lite/plugins/p");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("plugin.json"), r#"{"name":"P"}"#).unwrap();
        let store = approvals_file(&d.join("config"));
        let pa = project_plugins(&d.join("a"), Some(&store)).remove(0);
        let t = pa.trust.clone().unwrap();
        // the same folder and hash under another project's key
        std::fs::create_dir_all(d.join("config")).unwrap();
        let other = serde_json::json!({"version": 2, "approved": {"/other": {&t.folder: &t.hash}}});
        std::fs::write(&store, other.to_string()).unwrap();
        assert!(!project_plugins(&d.join("a"), Some(&store))[0].allowed);
        approve(&store, &pa).unwrap();
        assert!(project_plugins(&d.join("a"), Some(&store))[0].allowed);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The VS Code extension refuses these too: the real plugins folder must be inside the real
    /// project, or one OK would cover another project's files.
    #[cfg(unix)]
    #[test]
    fn a_project_whose_droid_lite_or_plugins_folder_links_outside_it_is_refused() {
        let d = temp("linked-root");
        let a = d.join("a/.droid-lite/plugins/p");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("plugin.json"), r#"{"name":"P","list":["x"]}"#).unwrap();
        let store = approvals_file(&d.join("config"));
        // b's .droid-lite is a's; c's .droid-lite/plugins is a's
        std::fs::create_dir_all(d.join("b")).unwrap();
        std::os::unix::fs::symlink(d.join("a/.droid-lite"), d.join("b/.droid-lite")).unwrap();
        std::fs::create_dir_all(d.join("c/.droid-lite")).unwrap();
        std::os::unix::fs::symlink(
            d.join("a/.droid-lite/plugins"),
            d.join("c/.droid-lite/plugins"),
        )
        .unwrap();
        for p in ["b", "c"] {
            let mut found = discover_all(&[], Some(&d.join(p)), Some(&store));
            assert_eq!(found.len(), 1, "{p}: {found:?}");
            let e = found.remove(0).unwrap_err();
            assert!(
                e.contains("p/plugin.json: links outside the project ("),
                "{p}: {e}"
            );
        }
        // a .droid-lite linked to a folder inside the project is fine
        std::fs::create_dir_all(d.join("e/config")).unwrap();
        std::fs::rename(d.join("a/.droid-lite"), d.join("e/config/dl")).unwrap();
        std::os::unix::fs::symlink(d.join("e/config/dl"), d.join("e/.droid-lite")).unwrap();
        assert!(
            project_plugins(&d.join("e"), Some(&store))[0]
                .trust
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_command_checks_the_folder_again_and_a_change_asks_for_the_ok_again() {
        let d = temp("recheck");
        let dir = d.join("proj/.droid-lite/plugins/p");
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{"name":"P","list":["./list.sh"],"run":["./run.sh","{id}"],"watch":["./w.sh"],
            "views":[{"name":"V","run":["./v.sh"]}],"actions":[{"name":"A","run":["./a.sh"]}]}"#;
        std::fs::write(dir.join("plugin.json"), json).unwrap();
        std::fs::write(dir.join("list.sh"), "echo []\n").unwrap();
        let store = approvals_file(&d.join("config"));
        let found = || project_plugins(&d.join("proj"), Some(&store)).remove(0);
        approve(&store, &found()).unwrap();
        let loaded = found();
        assert!(loaded.allowed);
        let argvs: Vec<Vec<String>> = loaded
            .commands()
            .into_iter()
            .map(<[String]>::to_vec)
            .collect();
        assert_eq!(argvs.len(), 5, "list, run, watch, view, action");
        for argv in &argvs {
            let mut m = loaded.clone();
            assert!(
                m.command(argv, None, "").is_ok(),
                "unchanged: {argv:?} runs"
            );
        }
        // any file of the folder changes after the load: no command runs
        std::fs::write(dir.join("list.sh"), "rm -rf ~\n").unwrap();
        let mut m = loaded.clone();
        for argv in &argvs {
            m = loaded.clone();
            let e = m.command(argv, None, "").unwrap_err();
            assert_eq!(
                e, "P changed since you allowed it; allow it again",
                "{argv:?}"
            );
            assert!(!m.allowed, "Needs your OK again");
            assert_ne!(m.trust, loaded.trust);
            assert_eq!(
                m.trust,
                found().trust,
                "read again: the folder as it is now"
            );
        }
        // allowing what it read now runs it again
        approve(&store, &m).unwrap();
        m.allowed = true;
        assert!(m.command(&argvs[0], None, "").is_ok());
        // a folder that can no longer be checked is refused with its reason
        let mut m = found();
        m.allowed = true;
        std::fs::write(dir.join("big"), vec![0u8; TREE_MAX_BYTES as usize]).unwrap();
        let e = m.command(&argvs[0], None, "").unwrap_err();
        assert!(
            e.starts_with("P changed since you allowed it and cannot run now (too big"),
            "{e}"
        );
        assert!(!m.allowed);
        // it is an error now, never an OK to give: not with the hash it was allowed under either
        assert_eq!(m.broken.as_deref(), Some(e.as_str()));
        assert_eq!(m.command(&argvs[0], None, "").unwrap_err(), e);
        let before = std::fs::read_to_string(&store).unwrap();
        assert_eq!(approve(&store, &m).unwrap_err(), e);
        assert_eq!(std::fs::read_to_string(&store).unwrap(), before);
        // the user's own plugins are not checked
        let mut own = Manifest::parse(json, &d.join("nowhere")).unwrap();
        own.allowed = true;
        assert!(own.command(&argvs[0], None, "").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The lock is a file created only when it does not exist, the same as in the VS Code
    /// extension, so a lock either one holds makes the other wait.
    #[test]
    fn an_ok_waits_for_the_lock_file_the_extension_holds_and_a_stale_one_is_removed() {
        let d = temp("lockfile");
        let store = approvals_file(&d.join("config"));
        let lock = d.join("config/approved-plugins.json.lock");
        std::fs::create_dir_all(d.join("config")).unwrap();
        let m = |folder: &str| Manifest {
            name: "P".into(),
            trust: Some(Trust {
                project: "/proj".into(),
                folder: folder.into(),
                hash: "ab".into(),
            }),
            ..Default::default()
        };
        // held by the extension (it creates the file with 'wx')
        std::fs::write(&lock, "").unwrap();
        let (a, s) = (m("/proj/a"), store.clone());
        let t0 = Instant::now();
        let waiting = std::thread::spawn(move || approve(&s, &a));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!waiting.is_finished(), "waits while the lock is held");
        assert!(!store.exists());
        // the holder writes, then removes its lock
        std::fs::write(
            &store,
            r#"{"version":2,"approved":{"/proj":{"/proj/b":"cd"}}}"#,
        )
        .unwrap();
        std::fs::remove_file(&lock).unwrap();
        waiting.join().unwrap().unwrap();
        assert!(t0.elapsed() < LOCK_WAIT);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["approved"]["/proj"]["/proj/a"], "ab");
        assert_eq!(
            v["approved"]["/proj"]["/proj/b"], "cd",
            "the holder's write is kept"
        );
        assert!(!lock.exists(), "removed after the rename");
        // a lock older than 30 s was left by a process that died
        let f = std::fs::File::create(&lock).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let t0 = Instant::now();
        approve(&store, &m("/proj/c")).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert!(!lock.exists());
        // a fresh lock that is never removed: it gives up after about 5 s
        std::fs::write(&lock, "").unwrap();
        let e = approve(&store, &m("/proj/d")).unwrap_err();
        assert!(
            e.ends_with("is held by another Droid Lite; try again"),
            "{e}"
        );
        assert!(lock.exists(), "another's lock is left alone");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert!(v["approved"]["/proj"].get("/proj/d").is_none());
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

    /// Writes `text` to `p` with permission bits `mode`.
    #[cfg(unix)]
    fn write_mode(p: &Path, text: &str, mode: u32) {
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
    }

    /// The vectors the VS Code extension's test uses too (recomputed with Python's hashlib).
    #[cfg(unix)]
    #[test]
    fn tree_hash_of_a_fixed_folder() {
        let d = temp("vector");
        std::fs::create_dir_all(d.join("a")).unwrap();
        write_mode(&d.join("a.txt"), "x", 0o644);
        write_mode(&d.join("a/b"), "", 0o644);
        write_mode(&d.join("plugin.json"), r#"{"name":"T"}"#, 0o644);
        let w = walk(&d).unwrap();
        let paths: Vec<&str> = w.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["a.txt", "a/b", "plugin.json"],
            "sorted as bytes: '.' < '/'"
        );
        assert_eq!(
            tree_hash(&w).unwrap(),
            (
                "b7b77b6b2174d5ec3f45452cb493a4a27fdce843557c41dcdecb1d7f98aff0bd".into(),
                Some(br#"{"name":"T"}"#.to_vec())
            )
        );
        let _ = std::fs::remove_dir_all(&d);
        // and the extension's own vector
        let d = temp("vector-js");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        write_mode(
            &d.join("plugin.json"),
            r#"{"name":"V","list":["sh","list.sh"]}"#,
            0o644,
        );
        write_mode(&d.join("list.sh"), "echo []\n", 0o755);
        write_mode(&d.join("sub/a.txt"), "a", 0o600);
        assert_eq!(
            hash_of(&d),
            "c8b8b3b7639db77b69849a4bdbd4858552d939e8f6a9e38e18d74f8ee18b1d1a"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `chmod +x` on an allowed script changes what runs: the OK is asked again.
    #[cfg(unix)]
    #[test]
    fn a_changed_file_mode_asks_for_the_ok_again() {
        let d = temp("mode");
        let dir = d.join("proj/.droid-lite/plugins/p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"name":"P","list":["./go.sh"]}"#,
        )
        .unwrap();
        write_mode(&dir.join("go.sh"), "echo []\n", 0o644);
        let store = approvals_file(&d.join("config"));
        let found = || project_plugins(&d.join("proj"), Some(&store)).remove(0);
        approve(&store, &found()).unwrap();
        let mut loaded = found();
        assert!(loaded.allowed);
        write_mode(&dir.join("go.sh"), "echo []\n", 0o755);
        assert!(!found().allowed, "the same bytes, another mode");
        let list = loaded.list.clone().unwrap();
        assert_eq!(
            loaded.command(&list, None, "").unwrap_err(),
            "P changed since you allowed it; allow it again"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_refused_with_the_extensions_words() {
        use std::os::unix::ffi::OsStringExt;
        let n = std::ffi::OsString::from_vec(b"a\xffb\xe2\x82".to_vec());
        assert_eq!(
            utf8_name(n).unwrap_err(),
            "a\u{fffd}b\u{fffd} is not a UTF-8 name"
        );
        assert_eq!(utf8_name("plugin.json".into()).unwrap(), "plugin.json");
    }

    /// A file swapped for a link after the folder was listed is never followed.
    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_a_file_is_never_opened() {
        let d = temp("nofollow");
        std::fs::write(d.join("target"), "secret").unwrap();
        std::os::unix::fs::symlink(d.join("target"), d.join("link")).unwrap();
        let e = open_no_link(&d.join("link")).unwrap_err();
        assert!(is_link_error(&e), "{e}");
        assert!(open_no_link(&d.join("target")).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `{plugin}` is the folder's real path, as in the VS Code extension; a project plugin's
    /// commands get PYTHONDONTWRITEBYTECODE (no `__pycache__` in the folder), the user's do not need it.
    #[cfg(unix)]
    #[test]
    fn plugin_is_the_real_folder_and_project_commands_write_no_bytecode() {
        let d = temp("real-plugin");
        let dir = d.join("proj/.droid-lite/plugins/real");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&dir, d.join("proj/.droid-lite/plugins/via")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"name":"P","list":["{plugin}/l.py","{plugin}"]}"#,
        )
        .unwrap();
        let store = approvals_file(&d.join("config"));
        let via = |s| {
            project_plugins(&d.join("proj"), s)
                .into_iter()
                .find(|m| m.folder.ends_with("via"))
                .unwrap()
        };
        approve(&store, &via(Some(&store))).unwrap();
        let mut m = via(Some(&store));
        let real = dir.canonicalize().unwrap();
        assert_eq!(m.real, real);
        let cmd = m.command(&m.list.clone().unwrap(), None, "").unwrap();
        assert_eq!(cmd.get_program(), real.join("l.py"));
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), [real.as_os_str()]);
        let env = |c: &Command| {
            c.get_envs()
                .find(|(k, _)| *k == "PYTHONDONTWRITEBYTECODE")
                .and_then(|(_, v)| v.map(|v| v.to_owned()))
        };
        assert_eq!(env(&cmd), Some("1".into()));
        let mut own = Manifest::parse(r#"{"name":"O","list":["x"]}"#, &dir).unwrap();
        own.allowed = true;
        assert_eq!(env(&own.command(&["x".into()], None, "").unwrap()), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_ok_says_where_the_commands_run_when_it_is_not_the_plugin_folder() {
        let d = temp("runs-in");
        std::fs::create_dir_all(d.join("p")).unwrap();
        let m = Manifest::parse(r#"{"name":"P","list":["x"],"dir":".."}"#, &d.join("p")).unwrap();
        let parent = d.canonicalize().unwrap();
        assert!(
            m.approval_text().contains(&format!(
                "from {}\nRuns in: {}\nAllowing",
                d.join("p").display(),
                parent.display()
            )),
            "{}",
            m.approval_text()
        );
        let own = Manifest::parse(r#"{"name":"P","list":["x"],"dir":"."}"#, &d.join("p")).unwrap();
        assert!(!own.approval_text().contains("Runs in"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_store_whose_version_is_2_0_counts_as_version_2() {
        for (text, v2) in [
            (r#"{"version":2.0,"approved":{}}"#, true),
            (r#"{"version":2,"approved":{}}"#, true),
            (r#"{"version":"2","approved":{}}"#, false),
            (r#"{"version":2.5,"approved":{}}"#, false),
            (r#"{"version":2,"approved":[]}"#, false),
        ] {
            assert_eq!(is_v2(&serde_json::from_str(text).unwrap()), v2, "{text}");
        }
    }

    /// Runs `lock` on a thread: an endless loop shows as a timeout, not a hung test.
    fn lock_soon(path: &Path, patience: Duration) -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(lock(path, patience).map(|l| l.token.clone()));
        });
        rx.recv_timeout(patience + Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("still trying after {patience:?}: it spins"))
    }

    /// Whatever is in the way of the lock (an old folder, a dangling link, an old lock file that
    /// cannot be taken away) waits and gives up in time, never spins, and is left alone.
    #[cfg(unix)]
    #[test]
    fn a_lock_in_the_way_that_cannot_be_taken_is_held_and_never_spins() {
        let d = temp("lock-odd");
        let dir = d.join("dir.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::open(&dir)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let link = d.join("link.lock");
        std::os::unix::fs::symlink(d.join("nowhere"), &link).unwrap();
        // a stale lock file in a folder that does not let it be renamed or removed
        let ro = d.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let stuck = ro.join("s.lock");
        std::fs::write(&stuck, "1 old").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stuck)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
        std::fs::set_permissions(&ro, mode(0o555)).unwrap();
        for p in [&dir, &link, &stuck] {
            let t0 = Instant::now();
            let e = lock_soon(p, Duration::from_millis(300)).unwrap_err();
            assert!(
                e.ends_with("is held by another Droid Lite; try again"),
                "{e}"
            );
            assert!(t0.elapsed() >= Duration::from_millis(300));
            assert!(std::fs::symlink_metadata(p).is_ok(), "left alone: {p:?}");
        }
        std::fs::set_permissions(&ro, mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_holder_removes_only_its_own_lock() {
        let d = temp("lock-token");
        let path = d.join("s.lock");
        let held = lock(path.clone(), Duration::ZERO).unwrap();
        let token = std::fs::read_to_string(&path).unwrap();
        assert_eq!(token, held.token);
        assert!(token.starts_with(&format!("{} ", std::process::id())));
        drop(held);
        assert!(!path.exists(), "its own lock: removed");
        // taken away as stale and made again by another process meanwhile: not its to remove
        let held = lock(path.clone(), Duration::ZERO).unwrap();
        assert_ne!(held.token, token, "a token per lock");
        std::fs::write(&path, "4242 another").unwrap();
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242 another");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two processes see one stale lock; the first takes it away and locks; the second, late,
    /// must not delete that fresh lock.
    #[test]
    fn two_takers_of_one_stale_lock_never_delete_the_fresh_one() {
        let d = temp("lock-stale-race");
        let path = d.join("s.lock");
        std::fs::write(&path, "1 old").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let seen = std::fs::symlink_metadata(&path).unwrap();
        remove_stale(&path, &seen);
        assert!(!path.exists(), "the first takes it away");
        let fresh = lock(path.clone(), Duration::ZERO).unwrap();
        remove_stale(&path, &seen);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            fresh.token,
            "the fresh lock survives"
        );
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["s.lock"], "nothing left aside");
        drop(fresh);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_http_and_https_links_are_kept() {
        let m = Manifest::parse(
            r#"{"name":"L","links":[{"name":"web","url":"https://example.com"},
                {"name":"up","url":"HTTP://127.0.0.1:8080"},{"name":"file","url":"file:///etc/passwd"},
                {"name":"share","url":"smb://evil/share"},{"name":"path","url":"/Applications/Calculator.app"},
                {"name":"js","url":"javascript:alert(1)"},{"name":"app","url":"x-apple.systempreferences:"},
                {"name":"spaced","url":" https://example.com"}]}"#,
            Path::new("/p"),
        )
        .unwrap();
        let names: Vec<&str> = m.links.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["web", "up"]);
        let rows = parse_entries(
            r#"[{"label":"a","url":"https://x.test/a"},{"label":"b","url":"file:///etc/hosts"}]"#,
            None,
        )
        .unwrap();
        assert_eq!(rows[0].url.as_deref(), Some("https://x.test/a"));
        assert_eq!(rows[1].url, None, "a row's file:// link is dropped too");
    }

    thread_local! {
        /// Full reads of a plugin folder ([`tree_hash`]) on this thread.
        pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// An allowed plugin whose folder looks as it did at the last full read (every file's size,
    /// times, inode, mode) is not read again: a load and the commands after it hash it once.
    #[test]
    fn a_check_reads_the_folder_again_only_when_a_file_looks_changed() {
        let d = temp("seen");
        let dir = d.join("proj/.droid-lite/plugins/p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"name":"P","list":["./l.sh"]}"#).unwrap();
        std::fs::write(dir.join("l.sh"), "echo []\n").unwrap();
        let store = approvals_file(&d.join("config"));
        approve(&store, &project_plugins(&d.join("proj"), None)[0]).unwrap();
        let mut m = project_plugins(&d.join("proj"), Some(&store)).remove(0);
        assert!(m.allowed && !m.seen.is_empty());
        let list = m.list.clone().unwrap();
        let reads = || READS.with(|r| r.get());
        let start = reads();
        for _ in 0..3 {
            m.command(&list, None, "").unwrap();
        }
        assert_eq!(reads(), start, "unchanged: no file is read again");
        // the same bytes written again: read once, then trusted again by its look
        std::fs::write(dir.join("l.sh"), "echo []\n").unwrap();
        m.command(&list, None, "").unwrap();
        m.command(&list, None, "").unwrap();
        assert_eq!(reads(), start + 1);
        // other bytes of the same size: read, and refused
        std::fs::write(dir.join("l.sh"), "rm -r ~\n").unwrap();
        assert_eq!(
            m.command(&list, None, "").unwrap_err(),
            "P changed since you allowed it; allow it again"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Reading a plugin folder keeps no copy of its files: 4 MiB hashed with far less memory.
    #[test]
    fn the_tree_hash_reads_files_a_piece_at_a_time() {
        let d = temp("stream");
        std::fs::write(d.join("plugin.json"), r#"{"name":"S"}"#).unwrap();
        std::fs::write(d.join("blob"), vec![7u8; 4 << 20]).unwrap();
        let w = walk(&d).unwrap();
        let before = crate::alloc_count::thread_allocated();
        let (hash, json) = tree_hash(&w).unwrap();
        let used = crate::alloc_count::thread_allocated() - before;
        assert!(used < 512 << 10, "{used} bytes allocated");
        assert_eq!(json.as_deref(), Some(br#"{"name":"S"}"#.as_slice()));
        // the same digest as all of it at once
        let mut all = Vec::new();
        for (rel, bytes, mode) in [
            ("blob", vec![7u8; 4 << 20], 0o644u16),
            ("plugin.json", br#"{"name":"S"}"#.to_vec(), 0o644),
        ] {
            all.extend_from_slice(rel.as_bytes());
            all.push(0);
            all.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            all.extend_from_slice(&mode.to_be_bytes());
            all.extend_from_slice(&bytes);
        }
        #[cfg(unix)]
        assert_eq!(hash, sha256_hex(&all));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sha256_fed_in_pieces_is_sha256_of_the_whole() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0, 1, 55, 56, 63, 64, 65, 127, 128, 300] {
            let whole = sha256_hex(&data[..len]);
            for cut in [0, 1, 3, 63, 64, 65, len / 2, len] {
                let cut = cut.min(len);
                let mut s = Sha256::new();
                s.update(&data[..cut]);
                s.update(&data[cut..len]);
                assert_eq!(s.hex(), whole, "{len} cut at {cut}");
            }
        }
    }

    /// A file gone or locked for a moment (an atomic save, a `git checkout`) is a change that may
    /// be allowed again, never a plugin that can no longer run.
    #[cfg(unix)]
    #[test]
    fn a_file_gone_or_locked_for_a_moment_asks_again_and_is_not_broken() {
        let d = temp("transient");
        let dir = d.join("proj/.droid-lite/plugins/p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"name":"P","list":["./l.sh"]}"#).unwrap();
        write_mode(&dir.join("l.sh"), "echo []\n", 0o644);
        let store = approvals_file(&d.join("config"));
        approve(&store, &project_plugins(&d.join("proj"), None)[0]).unwrap();
        let loaded = project_plugins(&d.join("proj"), Some(&store)).remove(0);
        let list = loaded.list.clone().unwrap();
        let again = "P changed since you allowed it; allow it again";
        // locked (EACCES): the mode is part of the hash, so set it back the same way
        let mut m = loaded.clone();
        let chmod = |mode| {
            std::fs::set_permissions(
                dir.join("l.sh"),
                std::os::unix::fs::PermissionsExt::from_mode(mode),
            )
            .unwrap()
        };
        chmod(0o000);
        assert_eq!(m.command(&list, None, "").unwrap_err(), again);
        assert!(m.broken.is_none() && !m.allowed);
        assert_eq!(m.trust, loaded.trust, "still the plugin that was allowed");
        chmod(0o644);
        m.allowed = true;
        assert!(m.command(&list, None, "").is_ok(), "back as it was: runs");
        // the whole folder gone for a moment (ENOENT)
        let mut m = loaded.clone();
        std::fs::rename(&dir, d.join("away")).unwrap();
        assert_eq!(m.command(&list, None, "").unwrap_err(), again);
        assert!(m.broken.is_none());
        std::fs::rename(d.join("away"), &dir).unwrap();
        m.allowed = true;
        assert!(m.command(&list, None, "").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A FIFO swapped in for a file after the walk is never waited on: it is a change.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_place_of_a_file_is_a_change_and_never_hangs() {
        let d = temp("fifo");
        std::fs::write(d.join("plugin.json"), r#"{"name":"F"}"#).unwrap();
        std::fs::write(d.join("x"), "x").unwrap();
        let w = walk(&d).unwrap();
        std::fs::remove_file(d.join("x")).unwrap();
        let fifo = std::ffi::CString::new(d.join("x").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(tree_hash(&w).map(|_| ()).map_err(|r| (r.why, r.broken)));
        });
        let r = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the hash waited on a FIFO");
        assert_eq!(r, Err(("x is no longer a file".into(), false)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The stale lock is looked at again right before it is taken away: a fresh lock in its place
    /// is never renamed (another process could lock in the gap).
    #[test]
    fn a_stale_lock_made_fresh_meanwhile_is_not_touched() {
        let d = temp("lock-restat");
        let path = d.join("s.lock");
        let old = |p: &Path| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
                .unwrap();
        };
        std::fs::write(&path, "1 old").unwrap();
        old(&path);
        let seen = std::fs::symlink_metadata(&path).unwrap();
        // another process takes it away and locks before we look again
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "2 fresh").unwrap();
        let mut steps = Vec::new();
        remove_stale_with(&path, &seen, &mut |s| {
            steps.push(s.to_owned());
            // a third process, were it renamed away
            if s == "put back" {
                std::fs::write(&path, "3 third").unwrap();
            }
        });
        assert!(steps.is_empty(), "{steps:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "2 fresh");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The lock renamed aside turns out to be another's fresh one and a newer lock took its place:
    /// it cannot be put back, so it is left aside, never deleted.
    #[test]
    fn a_fresh_lock_that_cannot_be_put_back_is_never_deleted() {
        let d = temp("lock-putback");
        let path = d.join("s.lock");
        std::fs::write(&path, "1 old").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let seen = std::fs::symlink_metadata(&path).unwrap();
        remove_stale_with(&path, &seen, &mut |s| match s {
            // between our look and our rename: another takes the stale one away and locks
            "rename" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::write(&path, "2 fresh").unwrap();
            }
            // and a third locks once we renamed that one aside
            _ => std::fs::write(&path, "3 third").unwrap(),
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "3 third");
        let aside: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".stale."))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(
            std::fs::read_to_string(d.join(&aside[0])).unwrap(),
            "2 fresh",
            "kept, not deleted"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_lock_dated_far_in_the_future_is_stale_too() {
        let d = temp("lock-future");
        let path = d.join("s.lock");
        let at = |ahead: u64| {
            std::fs::write(&path, "1 skewed").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(ahead))
                .unwrap();
            std::fs::symlink_metadata(&path).unwrap()
        };
        assert!(is_stale(&at(120)), "two minutes ahead");
        assert!(!is_stale(&at(5)), "a few seconds ahead: a clock a bit off");
        at(120);
        assert!(
            lock_soon(&path, Duration::from_secs(1)).is_ok(),
            "taken away, not waited for"
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
        let real = tool.join("droid-lite-plugin").canonicalize().unwrap();
        assert_eq!(
            add(&plugins, &tool).unwrap(),
            ("My Tool".into(), real.clone())
        );
        assert_eq!(
            add(&plugins, &tool.join("droid-lite-plugin")).unwrap(),
            ("My Tool".into(), real.clone())
        );
        let found = discover(std::slice::from_ref(&plugins));
        assert_eq!(found.len(), 1, "added once");
        let m = found[0].as_ref().unwrap();
        assert_eq!(m.folder, plugins.join("my-tool"));
        assert_eq!(m.real, real, "the id add returns");
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
