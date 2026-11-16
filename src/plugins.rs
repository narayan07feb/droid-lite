//! Plugins: outside tools (test runners, checkers) that Droid Lite drives through a small JSON
//! contract. A plugin is a folder with a `plugin.json`; the contract is in docs/PLUGINS.md.
//! Nothing here knows any one tool: the plugin's own commands print JSON, Droid Lite shows it.

use serde_json::Value;
use std::path::{Path, PathBuf};
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
    /// SHA-256 (hex) of the plugin.json bytes this manifest was read from.
    pub hash: String,
    /// Its commands may run: a plugin of the user's own folder, or a project plugin the user
    /// allowed with exactly this plugin.json ([`approve`]). [`Manifest::command`] refuses otherwise.
    pub allowed: bool,
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
        let dir = match v.get("dir").and_then(Value::as_str) {
            Some(d) => folder.join(expand_home(d)),
            None => folder.to_path_buf(),
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
            hash: sha256_hex(json.as_bytes()),
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
            "Needs your OK: runs {} from {}",
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
/// extension. A JSON object: the real path of a project plugin's folder -> the SHA-256 (hex) of the
/// plugin.json the user allowed.
pub fn approvals_file(config: &Path) -> PathBuf {
    config.join("approved-plugins.json")
}

fn approvals(store: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match std::fs::read_to_string(store) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(format!(
                "{} is not a JSON object; fix or delete it",
                store.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("{}: {e}", store.display())),
    }
}

/// The key a plugin is allowed under: the real path of its folder.
fn approval_key(m: &Manifest) -> Option<String> {
    m.folder
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Whether the user allowed this project plugin's folder with exactly this plugin.json.
pub fn is_approved(store: &Path, m: &Manifest) -> bool {
    let (Some(key), Ok(all)) = (approval_key(m), approvals(store)) else {
        return false;
    };
    all.get(&key).and_then(Value::as_str) == Some(m.hash.as_str())
}

/// Allows a project plugin as it was read (its plugin.json's hash): written to `store` through a
/// temp file and a rename, every other entry kept.
pub fn approve(store: &Path, m: &Manifest) -> Result<(), String> {
    let key = approval_key(m).ok_or_else(|| format!("{} is gone", m.folder.display()))?;
    let mut all = approvals(store)?;
    all.insert(key, Value::String(m.hash.clone()));
    let dir = store.parent().ok_or("no folder for the approvals file")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".approved-plugins.json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(&Value::Object(all)).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text + "\n")
        .and_then(|_| std::fs::rename(&tmp, store))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", store.display())
        })
}

/// Every `<dir>/<plugin>/plugin.json` under `user` and `project`, sorted by folder name; a broken
/// one is an error that names its file. A user plugin may run; a project plugin (it comes with
/// the project's files, so with anyone's commit) only once `store` has it ([`is_approved`]).
pub fn discover_all(
    user: &[PathBuf],
    project: &[PathBuf],
    store: Option<&Path>,
) -> Vec<Result<Manifest, String>> {
    let mut found: Vec<(PathBuf, bool)> = user
        .iter()
        .map(|d| (d, false))
        .chain(project.iter().map(|d| (d, true)))
        .filter_map(|(d, p)| std::fs::read_dir(d).ok().map(|rd| (rd, p)))
        .flat_map(|(rd, p)| rd.flatten().map(move |e| (e.path(), p)))
        .filter(|(f, _)| f.join("plugin.json").is_file())
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(f, in_project)| {
            let file = f.join("plugin.json");
            std::fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|j| Manifest::parse(&j, &f))
                .map(|mut m| {
                    m.allowed = !in_project || store.is_some_and(|s| is_approved(s, &m));
                    m
                })
                .map_err(|e| format!("{}: {e}", file.display()))
        })
        .collect()
}

/// [`discover_all`] of the user's own plugin folders: every plugin there may run.
#[cfg(test)]
fn discover(dirs: &[PathBuf]) -> Vec<Result<Manifest, String>> {
    discover_all(dirs, &[], None)
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

    #[test]
    fn a_project_plugin_runs_only_once_allowed_and_again_after_its_plugin_json_changes() {
        let d = std::env::temp_dir().join(format!("droid-lite-approve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (user, project) = (d.join("config/plugins"), d.join("proj/.droid-lite/plugins"));
        std::fs::create_dir_all(user.join("mine")).unwrap();
        std::fs::create_dir_all(project.join("shared")).unwrap();
        std::fs::write(
            user.join("mine/plugin.json"),
            r#"{"name":"Mine","list":["./list.sh"]}"#,
        )
        .unwrap();
        let shared = r#"{"name":"Shared","list":["sh","-c","touch pwned"],"actions":[{"name":"Go","run":["./go.sh"]}]}"#;
        std::fs::write(project.join("shared/plugin.json"), shared).unwrap();
        let store = approvals_file(&d.join("config"));
        // another tool's entry, kept by every write
        std::fs::write(&store, r#"{"/elsewhere/plugin":"00"}"#).unwrap();
        let found = |s: Option<&Path>| -> Vec<Manifest> {
            discover_all(
                std::slice::from_ref(&user),
                std::slice::from_ref(&project),
                s,
            )
            .into_iter()
            .map(Result::unwrap)
            .collect()
        };
        let [mine, sh] = &found(Some(&store))[..] else {
            panic!()
        };
        assert!(mine.allowed, "the user's own plugin needs no OK");
        assert!(!sh.allowed, "a project plugin waits for the user's OK");
        assert!(
            sh.command(sh.list.as_ref().unwrap(), None, "").is_err(),
            "and runs nothing meanwhile"
        );
        let real = project.join("shared").canonicalize().unwrap();
        assert_eq!(
            sh.approval_text(),
            format!(
                r#"Needs your OK: runs ["sh", "-c", "touch pwned"], ["./go.sh"] from {}"#,
                project.join("shared").display()
            )
        );
        approve(&store, sh).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["/elsewhere/plugin"], "00", "other entries stay");
        assert_eq!(
            v[real.to_str().unwrap()],
            sha256_hex(shared.as_bytes()),
            "real path -> sha-256 of the plugin.json bytes"
        );
        assert_eq!(
            std::fs::read_dir(d.join("config")).unwrap().count(),
            2,
            "no temp file left: {:?}",
            std::fs::read_dir(d.join("config"))
                .unwrap()
                .collect::<Vec<_>>()
        );
        let sh = found(Some(&store)).remove(1);
        assert!(sh.allowed);
        assert!(sh.command(sh.list.as_ref().unwrap(), None, "").is_ok());
        assert!(!found(None)[1].allowed, "no approvals file: not allowed");
        // a changed plugin.json needs the OK again
        std::fs::write(
            project.join("shared/plugin.json"),
            shared.replace("touch pwned", "rm -rf ~"),
        )
        .unwrap();
        assert!(!found(Some(&store))[1].allowed);
        // a broken approvals file is never overwritten (it may hold other approvals)
        std::fs::write(&store, "[").unwrap();
        assert!(approve(&store, &found(Some(&store))[1]).is_err());
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "[");
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
