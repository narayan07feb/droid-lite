//! The Tests tab: each plugin's cases as a tree with ▶, live pass / fail, the plugin's live
//! state, its views (failures, …), actions and links. A ▶ also shows in the editor's gutter on
//! lines that a case names. The plugin contract is in docs/PLUGINS.md.

use super::{App, Bottom, config_dir};
use crate::gradle::{Line, Run, group_lives};
use crate::icons as ic;
use crate::plugins::{self, Case, Entry, Live, Manifest, Status, Trust};
use egui::{Align, Color32, Layout, RichText, Ui};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Output lines a run or action shows (status lines count too), as the VS Code extension keeps.
const MAX_LOG: usize = 20000;
/// And bytes of them in all.
const MAX_LOG_BYTES: usize = 8 << 20;
/// The most a `list` or a view may print: past it its process group is killed.
const MAX_FETCH: usize = 32 << 20;
/// The end of a failed `list`'s stderr kept for its error.
const ERR_TAIL: usize = 64 << 10;
/// How long a `list` or a view's output may still come after it exited (a tool it left running
/// may hold its output open).
const GRACE: Duration = Duration::from_millis(500);
/// How often a project plugin's folder is checked again while one of its commands runs.
const RECHECK: Duration = Duration::from_secs(3);
/// The longest pause before a `watch` that ended is started again (it doubles from 1 s).
const WATCH_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a `list` or a view may take before its process group is killed.
const FETCH_LIMIT: Duration = Duration::from_secs(120);

/// A `list` or a view running on a thread. Dropping it (a refused plugin, a reload, the app
/// quitting) kills its process group.
pub(super) struct Fetch {
    rx: Receiver<Result<String, String>>,
    slot: Arc<Mutex<Slot>>,
}

/// What of a [`Fetch`]'s process may be signalled, shared by its thread and its Drop.
enum Slot {
    Starting,
    /// The leader of process group `pgid`, not reaped (so the id is no other process's).
    Running(u32),
    /// Its leader was reaped; tools it started may still be in the group (while they are, the
    /// group id is no other process's).
    Reaped(u32),
    /// Dropped or done: nothing is signalled any more.
    Gone,
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

fn kill_group(pgid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
    }
}

/// Ends `slot`: SIGKILL to what of its group is left.
fn end_group(slot: &Mutex<Slot>) {
    match std::mem::replace(&mut *lock(slot), Slot::Gone) {
        Slot::Running(g) => kill_group(g),
        Slot::Reaped(g) if group_lives(g) => kill_group(g),
        _ => {}
    }
}

impl Fetch {
    fn try_recv(&self) -> Result<Result<String, String>, TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for Fetch {
    /// Kills its group at once, not at its thread's next look: the app may be gone by then.
    fn drop(&mut self) {
        end_group(&self.slot);
    }
}
/// A saved OK: the trust it covers, and whether it was saved.
type Allowing = Receiver<(Option<Trust>, Result<(), String>)>;
/// A folder check: the trust it checked, the plugin as read again, and whether it is unchanged.
type Check = Receiver<(Option<Trust>, Manifest, Result<(), String>)>;

/// A runnable case that names a file, for the gutter.
pub(super) struct Mark {
    pub file: PathBuf,
    /// 0-based editor row.
    pub row: usize,
    pub plugin: usize,
    pub id: String,
    pub label: String,
}

#[derive(Default)]
pub(super) struct Tests {
    pub(super) plugins: Vec<Plugin>,
    /// Broken manifests.
    pub(super) errors: Vec<String>,
    pub(super) active: usize,
    filter: String,
    /// Marks a gutter ▶ picked when its line has several cases; the list shows only these.
    pub(super) only: Vec<usize>,
    pub(super) marks: Vec<Mark>,
    /// Runs asked to stop (a watch no longer shown, a refused plugin's watch), and runs that
    /// ended while tools they started still run: kept until nothing of them is left, so the
    /// SIGKILL after SIGTERM still comes ([`Run::terminate`]) and a reload or quit (Drop) stops
    /// those tools too.
    stopping: Vec<Run>,
    /// The plugins being found and their folders checked, on a thread ([`App::load_plugins`]).
    discovering: Option<Receiver<Vec<Result<Manifest, String>>>>,
    /// The plugin folder to show once they are found (Add plugin).
    select: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct Plugin {
    pub(super) m: Manifest,
    pub(super) cases: Vec<Case>,
    pub(super) loading: Option<Fetch>,
    error: Option<String>,
    pub(super) status: HashMap<String, Status>,
    /// The running case or action: (its id, or "" for an action; title; process).
    pub(super) run: Option<(String, String, Run)>,
    pub(super) results: Vec<Entry>,
    pub(super) log: Vec<String>,
    pub(super) watch: Option<Run>,
    /// When the running `watch` started; the pause before starting an ended one again, and when.
    watch_started: Option<Instant>,
    pub(super) watch_backoff: Duration,
    pub(super) watch_next: Option<Instant>,
    /// Why the `watch` could not start (it is tried again after the pause).
    watch_error: Option<String>,
    pub(super) live: Option<Live>,
    /// The OK being saved on a thread (the lock may make it wait): for the trust it covers.
    allowing: Option<Allowing>,
    /// A check of the folder on a thread while a command runs ([`RECHECK`]); when the last started.
    check: Option<Check>,
    checked: Option<Instant>,
    /// None: the cases; Some(i): view i.
    view: Option<usize>,
    pub(super) rows: Vec<Entry>,
    rows_rx: Option<Fetch>,
    rows_error: Option<String>,
    /// What the running case or action may still show ([`MAX_LOG`], [`MAX_LOG_BYTES`]).
    room: Room,
    /// Commands waiting for the full check of a folder that looked changed ([`App::gated`]).
    pub(super) pending: Vec<Pending>,
    /// That check passed just now: the waiting commands start without another.
    verified: bool,
}

/// A command of a plugin that waits for its folder's full check.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Pending {
    List,
    View(usize),
    /// A case id, or an action.
    Run(String, Option<usize>),
    Watch,
}

/// Output lines and bytes a run showed so far, and whether it said the rest is not shown.
#[derive(Default)]
struct Room {
    lines: usize,
    bytes: usize,
    told: bool,
}

impl Plugin {
    fn new(m: Manifest) -> Plugin {
        Plugin {
            m,
            ..Default::default()
        }
    }

    /// One output line of the running case or action: a result, a case's status or a log line,
    /// while the run has room ([`MAX_LOG`] lines, [`MAX_LOG_BYTES`]; every line counts, as in the
    /// VS Code extension). Past that one line says so, and only a case still running ends (once).
    fn run_line(&mut self, line: &str, root: Option<&Path>) {
        self.room.lines += 1;
        self.room.bytes += line.len();
        let full = self.room.lines > MAX_LOG || self.room.bytes > MAX_LOG_BYTES;
        if full && !self.room.told {
            self.room.told = true;
            self.log.push(format!(
                "[more than {MAX_LOG} lines or {} MB: the rest is not shown]",
                MAX_LOG_BYTES >> 20
            ));
        }
        match plugins::parse_event(line, root) {
            plugins::Event::Log(s) if !full => self.log.push(s),
            plugins::Event::Log(_) => {}
            plugins::Event::Result(e) => {
                if let (Some(c), Some(s)) = (&e.case, e.status) {
                    let ends = s != Status::Running && self.status.get(c) == Some(&Status::Running);
                    if !full || ends {
                        self.status.insert(c.clone(), s);
                    }
                }
                if !full && e.status != Some(Status::Running) {
                    self.results.push(e);
                }
            }
        }
    }

    /// The `watch` ended or could not start: it starts again after a pause that doubles (1 s up
    /// to [`WATCH_BACKOFF_MAX`]) while it keeps ending soon, so it never starts every frame.
    fn watch_ended(&mut self) {
        let lived = self
            .watch_started
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.watch_backoff = if lived >= WATCH_BACKOFF_MAX {
            Duration::ZERO
        } else {
            (self.watch_backoff * 2).clamp(Duration::from_secs(1), WATCH_BACKOFF_MAX)
        };
        self.watch_next = Some(Instant::now() + self.watch_backoff);
    }
}

/// Runs `cmd` to the end on a thread ([`fetch_run`]).
fn fetch(cmd: std::process::Command, ctx: &egui::Context) -> Fetch {
    let (tx, rx) = channel();
    let slot = Arc::new(Mutex::new(Slot::Starting));
    let (theirs, ctx) = (slot.clone(), ctx.clone());
    std::thread::spawn(move || {
        let _ = tx.send(fetch_run(cmd, &theirs, FETCH_LIMIT));
        ctx.request_repaint();
    });
    Fetch { rx, slot }
}

/// Checks plugin `p`'s folder in full on a thread ([`Manifest::recheck`]), unless a check runs:
/// [`App::poll_oks`] takes the result.
fn check_folder(p: &mut Plugin, ctx: &egui::Context) {
    if p.check.is_some() {
        return;
    }
    p.checked = Some(Instant::now());
    let (tx, rx) = channel();
    let (mut m, ctx) = (p.m.clone(), ctx.clone());
    std::thread::spawn(move || {
        let trust = m.trust.clone();
        let r = m.recheck();
        let _ = tx.send((trust, m, r));
        ctx.request_repaint();
    });
    p.check = Some(rx);
}

/// Reads `pipe` to its end on a thread into the buffer it gives: at most `cap` bytes (past them
/// `over` is set and reading stops), or with `tail` only about the last `cap` bytes.
fn read_pipe(
    pipe: Option<impl std::io::Read + Send + 'static>,
    cap: usize,
    tail: bool,
    over: Arc<AtomicBool>,
) -> (Arc<Mutex<Vec<u8>>>, std::thread::JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let into = buf.clone();
    let reader = std::thread::spawn(move || {
        let Some(mut p) = pipe else {
            return;
        };
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let k = match p.read(&mut chunk) {
                Ok(0) => return,
                Ok(k) => k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let mut v = into.lock().unwrap_or_else(|e| e.into_inner());
            if !tail && v.len() + k > cap {
                over.store(true, Ordering::Relaxed);
                return;
            }
            v.extend_from_slice(&chunk[..k]);
            if tail && v.len() > 2 * cap {
                let cut = v.len() - cap;
                v.drain(..cut);
            }
        }
    });
    (buf, reader)
}

/// Runs `cmd` (the leader of its own process group) to the end: its stdout, or an error with the
/// end of its stderr. Once it exited, output may come for [`GRACE`] more: then what was read is
/// used (tools it left may hold its output open), and what is left of its group gets SIGKILL.
/// Past `limit`, past [`MAX_FETCH`] of output, or once its [`Fetch`] is dropped (`slot` is gone),
/// its whole group gets SIGKILL. Its group is signalled only while its id cannot be another's ([`Slot`]).
fn fetch_run(
    mut cmd: std::process::Command,
    slot: &Mutex<Slot>,
    limit: Duration,
) -> Result<String, String> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pgid = child.id();
    {
        let mut s = lock(slot);
        if matches!(*s, Slot::Gone) {
            drop(s);
            kill_group(pgid);
            let _ = child.wait();
            return Err("stopped".into());
        }
        *s = Slot::Running(pgid);
    }
    let over = Arc::new(AtomicBool::new(false));
    let (out, out_done) = read_pipe(child.stdout.take(), MAX_FETCH, false, over.clone());
    let (err, err_done) = read_pipe(child.stderr.take(), ERR_TAIL, true, over.clone());
    let end = Instant::now() + limit;
    let mut exited = None;
    let why = loop {
        if over.load(Ordering::Relaxed) {
            break Some("output too big".to_owned());
        }
        {
            let mut s = lock(slot);
            if matches!(*s, Slot::Gone) {
                break Some("stopped".to_owned());
            }
            if exited.is_none() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *s = Slot::Reaped(pgid);
                        exited = Some((status, Instant::now()));
                    }
                    Ok(None) => {}
                    Err(e) => break Some(e.to_string()),
                }
            }
        }
        if let Some((_, at)) = exited
            && ((out_done.is_finished() && err_done.is_finished()) || at.elapsed() >= GRACE)
        {
            break None;
        }
        if Instant::now() >= end {
            break Some(format!("no answer in {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    end_group(slot);
    let Some((status, _)) = exited.filter(|_| why.is_none()) else {
        if exited.is_none() {
            let _ = child.wait();
        }
        return Err(why.unwrap_or_default());
    };
    let take =
        |b: &Mutex<Vec<u8>>| std::mem::take(&mut *b.lock().unwrap_or_else(|e| e.into_inner()));
    if status.success() {
        return Ok(String::from_utf8_lossy(&take(&out)).into_owned());
    }
    let err = take(&err);
    let err = String::from_utf8_lossy(&err);
    let tail: Vec<&str> = err.lines().rev().take(3).collect();
    Err(format!(
        "exit {}: {}",
        status.code().unwrap_or(-1),
        tail.into_iter().rev().collect::<Vec<_>>().join(" ")
    ))
}

/// Opens a web link (`http`, `https`: [`plugins::is_web_link`]; nothing else) in the browser
/// named in the `browser` setting file, else the system default.
pub(super) fn open_link(url: &str) {
    if !plugins::is_web_link(url) {
        return;
    }
    let browser = super::load_setting("browser").map(|b| b.trim().to_owned());
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if let Some(b) = browser.filter(|b| !b.is_empty()) {
            cmd.args(["-a", &b]);
        }
        let _ = cmd.arg(url).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let b = browser
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "xdg-open".into());
        let _ = std::process::Command::new(b).arg(url).spawn();
    }
}

fn status_look(s: Option<Status>) -> (&'static str, Color32) {
    let t = crate::theme::t();
    match s {
        Some(Status::Running) => (ic::CLOCK_COUNTER_CLOCKWISE, crate::highlight::WARN),
        Some(Status::Passed) => (ic::CHECK_CIRCLE, Color32::from_rgb(0x5F, 0xB8, 0x65)),
        Some(Status::Failed) => (ic::X, crate::highlight::ERROR),
        Some(Status::Skipped) => ("–", t.weak),
        None => (" ", t.weak),
    }
}

/// What a click in the tab asks for, done after drawing.
enum Click {
    Run(String),
    Action(usize),
    Stop,
    View(Option<usize>),
    Reload,
    Open(PathBuf, usize),
    Link(String),
    Add,
    Remove,
    Allow,
}

impl Tests {
    /// The plugins were found ([`App::load_plugins`] runs on a thread).
    #[cfg(test)]
    pub(super) fn found(&self) -> bool {
        self.discovering.is_none()
    }

    pub(super) fn running(&self) -> bool {
        self.plugins.iter().any(|p| p.run.is_some())
    }

    /// Rows of `file` with a ▶, and the marks on each.
    pub(super) fn marks_at(&self, file: &Path) -> Vec<(usize, Vec<usize>)> {
        let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, m) in self
            .marks
            .iter()
            .enumerate()
            .filter(|(_, m)| m.file == file)
        {
            match out.iter_mut().find(|(r, _)| *r == m.row) {
                Some((_, v)) => v.push(i),
                None => out.push((m.row, vec![i])),
            }
        }
        out
    }

    fn rebuild_marks(&mut self) {
        self.marks.clear();
        for (i, p) in self.plugins.iter().enumerate() {
            let mut found = Vec::new();
            plugins::file_cases(&p.cases, &mut found);
            self.marks
                .extend(found.into_iter().map(|(file, line, id, label)| Mark {
                    // a file that does not exist yet still matches its open tab by name
                    file: file.canonicalize().unwrap_or(file),
                    row: line.saturating_sub(1),
                    plugin: i,
                    id,
                    label,
                }));
        }
        self.only.clear();
    }
}

impl App {
    /// Where Add plugin puts its links.
    fn plugins_home(&self) -> Option<PathBuf> {
        config_dir().map(|d| d.join("plugins"))
    }

    fn pick_plugin(&mut self, ctx: &egui::Context) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Choose a test tool's folder (or its plugin folder)")
            .pick_folder()
        {
            self.add_plugin(&dir, ctx);
        }
    }

    /// Adds the plugin in `picked` (see `plugins::add`), then shows it.
    pub(super) fn add_plugin(&mut self, picked: &Path, ctx: &egui::Context) {
        let Some(home) = self.plugins_home() else {
            return;
        };
        match plugins::add(&home, picked) {
            Ok((name, real)) => {
                self.load_plugins(ctx);
                // by folder (two plugins may share a name), once they are found
                self.tests.select = Some(real);
                self.bottom = Bottom::Tests;
                self.status = format!("Added the plugin {name}");
            }
            Err(e) => self.status = e,
        }
    }

    /// Where the user's OKs for project plugins are kept (shared with the VS Code extension).
    fn approvals(&self) -> Option<PathBuf> {
        config_dir().map(|d| plugins::approvals_file(&d))
    }

    /// Finds the plugins (config folder and the project's `.droid-lite/plugins`) on a thread (a
    /// project plugin's folder is read in full: [`App::poll_found`] takes them), then lists their
    /// cases in the background. A project plugin the user has not allowed runs nothing: the tab
    /// asks for the OK first. Running cases of the old set stop (Drop).
    pub(super) fn load_plugins(&mut self, ctx: &egui::Context) {
        let project = self.project.as_ref().map(|p| p.root.clone());
        let user: Vec<PathBuf> = self.plugins_home().into_iter().collect();
        let store = self.approvals();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(plugins::discover_all(
                &user,
                project.as_deref(),
                store.as_deref(),
            ));
            ctx.request_repaint();
        });
        self.tests = Tests {
            discovering: Some(rx),
            ..Default::default()
        };
    }

    /// The plugins [`App::load_plugins`] found: shown, and their cases listed.
    fn poll_found(&mut self, ctx: &egui::Context) {
        let Some(Ok(found)) = self.tests.discovering.as_ref().map(|rx| rx.try_recv()) else {
            return;
        };
        self.tests.discovering = None;
        for m in found {
            match m {
                Ok(m) => self.tests.plugins.push(Plugin::new(m)),
                Err(e) => self.tests.errors.push(e),
            }
        }
        if let Some(real) = self.tests.select.take()
            && let Some(i) = self.tests.plugins.iter().position(|p| p.m.real == real)
        {
            self.tests.active = i;
        }
        for i in 0..self.tests.plugins.len() {
            self.list_cases(i, ctx);
        }
    }

    /// The command for `argv` of plugin `i` (`id` fills `{id}`), at once when its folder looks
    /// as at its last full read ([`Manifest::quick_command`]: a walk, no file read). Else `then`
    /// waits for a full check of the folder on a thread ([`check_folder`]) and runs once it
    /// passes: `Ok(None)`. An error when the plugin may not run.
    fn gated(
        &mut self,
        i: usize,
        argv: &[String],
        id: &str,
        then: Pending,
        ctx: &egui::Context,
    ) -> Result<Option<std::process::Command>, String> {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let p = &mut self.tests.plugins[i];
        let cmd = p.m.quick_command(argv, root.as_deref(), id, p.verified)?;
        if cmd.is_none() {
            if !p.pending.contains(&then) {
                p.pending.push(then);
            }
            check_folder(p, ctx);
        }
        Ok(cmd)
    }

    /// The commands of plugin `i` that waited for the check of its folder, which passed just now.
    fn run_pending(&mut self, i: usize, ctx: &egui::Context) {
        for then in std::mem::take(&mut self.tests.plugins[i].pending) {
            self.tests.plugins[i].verified = true;
            match then {
                Pending::List => self.list_cases(i, ctx),
                Pending::View(v) => self.load_view(i, v, ctx),
                Pending::Run(id, action) => self.run_case(i, &id, action, ctx),
                Pending::Watch => {
                    let p = &self.tests.plugins[i];
                    if self.bottom == Bottom::Tests && self.tests.active == i && p.watch.is_none() {
                        self.start_watch(i, ctx);
                    }
                }
            }
            if let Some(p) = self.tests.plugins.get_mut(i) {
                p.verified = false;
            }
        }
    }

    fn list_cases(&mut self, i: usize, ctx: &egui::Context) {
        let p = &self.tests.plugins[i];
        let Some(list) = p.m.list.clone() else {
            return;
        };
        let allowed = p.m.allowed;
        match self.gated(i, &list, "", Pending::List, ctx) {
            Ok(Some(cmd)) => self.tests.plugins[i].loading = Some(fetch(cmd, ctx)),
            Ok(None) => {}
            // a not allowed plugin just waits for the OK; an allowed one changed meanwhile
            Err(e) if allowed => self.refused(i, e),
            Err(_) => {}
        }
    }

    /// A command of plugin `i` was refused because its folder changed since the OK: what of it
    /// still runs (its watch, a run or an action) stops, and it shows Needs your OK again (or its
    /// error, when it can no longer run at all), with why.
    fn refused(&mut self, i: usize, why: String) {
        if let Some(p) = self.tests.plugins.get_mut(i) {
            if let Some(mut w) = p.watch.take() {
                w.terminate();
                self.tests.stopping.push(w);
            }
            p.live = None;
            if let Some((_, _, run)) = p.run.as_mut() {
                run.terminate();
            }
            // a list or view still running: its group is killed (Fetch's Drop)
            p.loading = None;
            p.rows_rx = None;
            p.pending.clear();
            p.verified = false;
            p.error = Some(why.clone());
        }
        self.status = why;
    }

    /// The user's OK for project plugin `i` as it was read: kept in the approvals file on a thread
    /// (the lock may make it wait for seconds), then its cases are listed ([`App::poll_oks`]).
    pub(super) fn allow_plugin(&mut self, i: usize, ctx: &egui::Context) {
        let Some(store) = self.approvals() else {
            self.status = "No config folder for the approvals file".into();
            return;
        };
        let Some(p) = self.tests.plugins.get_mut(i) else {
            return;
        };
        if p.allowing.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (m, ctx) = (p.m.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send((m.trust.clone(), plugins::approve(&store, &m)));
            ctx.request_repaint();
        });
        p.allowing = Some(rx);
        self.status = format!("Allowing the plugin {}…", p.m.name);
    }

    /// Saved OKs and finished folder checks (commands that waited for one start); starts the
    /// next check of each project plugin that runs a command (its folder may change while a
    /// watch runs for hours).
    fn poll_oks(&mut self, ctx: &egui::Context) {
        let (mut listed, mut refused, mut ready) = (Vec::new(), Vec::new(), Vec::new());
        for (i, p) in self.tests.plugins.iter_mut().enumerate() {
            if let Some(rx) = &p.allowing
                && let Ok((trust, r)) = rx.try_recv()
            {
                p.allowing = None;
                match r {
                    // the plugin was read again meanwhile: that is not what was allowed
                    Ok(()) if trust == p.m.trust => {
                        p.m.allowed = true;
                        p.error = None;
                        self.status = format!("Allowed the plugin {}", p.m.name);
                        listed.push(i);
                    }
                    Ok(()) => {}
                    Err(e) => self.status = e,
                }
            }
            if let Some(rx) = &p.check
                && let Ok((trust, m, r)) = rx.try_recv()
            {
                p.check = None;
                if p.m.allowed && trust == p.m.trust {
                    match r {
                        // unchanged: how it looks now, so the next check needs no full read
                        Ok(()) => {
                            p.m.seen = m.seen;
                            if !p.pending.is_empty() {
                                ready.push(i);
                            }
                        }
                        Err(e) => {
                            p.m = m;
                            refused.push((i, e));
                        }
                    }
                }
            }
            let busy = p.watch.is_some() || p.run.is_some();
            if p.m.trust.is_some() && p.m.allowed && busy {
                if p.checked.is_none_or(|t| t.elapsed() >= RECHECK) {
                    check_folder(p, ctx);
                }
                ctx.request_repaint_after(RECHECK);
            }
            // commands wait for a check (one of an older OK did not count)
            if p.m.allowed && !p.pending.is_empty() && !ready.contains(&i) {
                check_folder(p, ctx);
            }
        }
        for (i, e) in refused {
            self.refused(i, e);
        }
        for i in listed {
            self.list_cases(i, ctx);
        }
        for i in ready {
            self.run_pending(i, ctx);
        }
    }

    fn load_view(&mut self, i: usize, v: usize, ctx: &egui::Context) {
        let p = &self.tests.plugins[i];
        let Some(argv) = p.m.views.get(v).map(|n| n.argv.clone()) else {
            return;
        };
        let allowed = p.m.allowed;
        match self.gated(i, &argv, "", Pending::View(v), ctx) {
            Ok(Some(cmd)) => self.tests.plugins[i].rows_rx = Some(fetch(cmd, ctx)),
            Ok(None) => {}
            Err(e) if allowed => self.refused(i, e),
            Err(e) => self.tests.plugins[i].rows_error = Some(e),
        }
    }

    /// Runs case `id` of plugin `i`, or action `action` when given.
    pub(super) fn run_case(
        &mut self,
        i: usize,
        id: &str,
        action: Option<usize>,
        ctx: &egui::Context,
    ) {
        let Some(p) = self.tests.plugins.get(i) else {
            return;
        };
        if p.run.is_some() {
            return;
        }
        let (argv, title) = match action {
            Some(a) => match p.m.actions.get(a) {
                Some(n) => (n.argv.clone(), n.name.clone()),
                None => return,
            },
            None => match &p.m.run {
                Some(r) => (r.clone(), id.to_owned()),
                None => return,
            },
        };
        let allowed = p.m.allowed;
        let then = Pending::Run(id.to_owned(), action);
        let cmd = match self.gated(i, &argv, id, then, ctx) {
            Ok(Some(c)) => c,
            // its folder looks changed: it starts once a full check on a thread passes
            Ok(None) => {
                self.status = format!("{}: checking its folder…", self.tests.plugins[i].m.name);
                self.tests.active = i;
                self.bottom = Bottom::Tests;
                return;
            }
            Err(e) if allowed => {
                self.refused(i, e);
                self.tests.active = i;
                self.bottom = Bottom::Tests;
                return;
            }
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let p = &mut self.tests.plugins[i];
        p.results.clear();
        p.log.clear();
        p.room = Room::default();
        match Run::spawn_group(cmd, ctx.clone()) {
            Ok(run) => {
                if action.is_none() {
                    p.status.insert(id.to_owned(), Status::Running);
                }
                let key = if action.is_none() { id } else { "" };
                p.run = Some((key.to_owned(), title.clone(), run));
                self.status = format!("{}: running {title}", p.m.name);
            }
            Err(e) => p.log.push(format!("Could not start {}: {e}", argv[0])),
        }
        self.tests.active = i;
        self.bottom = Bottom::Tests;
    }

    /// Reads every plugin's finished lists and new run, watch and view output. Called each frame.
    pub(super) fn poll_tests(&mut self) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        self.tests.stopping.retain_mut(|r| {
            r.poll();
            r.lives()
        });
        let mut lists_changed = false;
        let mut ended_runs = Vec::new();
        // ended runs whose tools still run: stopped with the rest on a reload or quit
        let mut left = Vec::new();
        for p in &mut self.tests.plugins {
            if let Some(rx) = &p.loading
                && let Ok(r) = rx.try_recv()
            {
                p.loading = None;
                match r.and_then(|j| plugins::parse_cases(&j, root.as_deref())) {
                    Ok(c) => {
                        p.cases = c;
                        p.error = None;
                    }
                    Err(e) => p.error = Some(format!("list: {e}")),
                }
                lists_changed = true;
            }
            if let Some(rx) = &p.rows_rx
                && let Ok(r) = rx.try_recv()
            {
                p.rows_rx = None;
                match r.and_then(|j| plugins::parse_entries(&j, root.as_deref())) {
                    Ok(rows) => {
                        p.rows = rows;
                        p.rows_error = None;
                    }
                    Err(e) => p.rows_error = Some(e),
                }
            }
            if let Some(w) = p.watch.as_mut() {
                for l in w.poll() {
                    match l {
                        Line::Out(s) => {
                            if let Some(live) = plugins::parse_live(&s) {
                                p.live = Some(live);
                            }
                        }
                        Line::Done(_) => {
                            left.extend(p.watch.take().filter(Run::lives));
                            p.watch_ended();
                        }
                    }
                }
            }
            let Some((id, title, run)) = p.run.as_mut() else {
                continue;
            };
            let (id, title) = (id.clone(), title.clone());
            let mut ended = None;
            for l in run.poll() {
                match l {
                    Line::Out(s) => p.run_line(&s, root.as_deref()),
                    Line::Done(code) => ended = Some(code),
                }
            }
            if let Some(code) = ended {
                left.extend(p.run.take().map(|(_, _, r)| r).filter(Run::lives));
                let ok = code == Some(0);
                // a run that never said how its own case ended gets the exit code's verdict
                if !id.is_empty() && p.status.get(&id) == Some(&Status::Running) {
                    p.status
                        .insert(id.clone(), if ok { Status::Passed } else { Status::Failed });
                }
                let failed = p
                    .results
                    .iter()
                    .filter(|e| e.status == Some(Status::Failed))
                    .count();
                let line = match code {
                    Some(c) => format!("{title} ended (exit {c})"),
                    None => format!("{title} stopped"),
                };
                p.log.push(line.clone());
                self.status = if failed > 0 {
                    format!("{}: {title}: {failed} failed", p.m.name)
                } else {
                    format!("{}: {line}", p.m.name)
                };
                ended_runs.push((self.status.clone(), failed == 0 && ok));
            }
        }
        self.tests.stopping.extend(left);
        for (text, ok) in ended_runs {
            self.toast(text, ok, Bottom::Tests);
        }
        if lists_changed {
            self.tests.rebuild_marks();
        }
    }

    /// The live state runs only while the Tests tab shows; one that ended starts again after a
    /// pause that grows while it keeps ending soon. Also takes saved OKs and folder checks
    /// ([`App::poll_oks`]).
    pub(super) fn keep_watch(&mut self, ctx: &egui::Context) {
        self.poll_found(ctx);
        self.poll_oks(ctx);
        // reaped soon (each frame polls them); a SIGKILL due asks for its own frame
        if self.tests.stopping.iter().any(|r| !r.ended()) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let shown = self.bottom == Bottom::Tests;
        let mut start = None;
        for (i, p) in self.tests.plugins.iter_mut().enumerate() {
            let want = shown && i == self.tests.active;
            let wait = p.watch_next.map_or(Duration::ZERO, |t| {
                t.saturating_duration_since(Instant::now())
            });
            match (&p.watch, &p.m.watch) {
                (None, Some(_)) if want && p.m.allowed && !wait.is_zero() => {
                    ctx.request_repaint_after(wait);
                }
                // not allowed (or refused once): nothing to start, no check each frame; one
                // waiting for the check of its folder starts when that passes
                (None, Some(_)) if want && p.m.allowed && !p.pending.contains(&Pending::Watch) => {
                    start = Some(i);
                }
                (Some(_), _) if !want => {
                    if let Some(mut w) = p.watch.take() {
                        w.terminate();
                        self.tests.stopping.push(w);
                    }
                    p.live = None;
                    p.watch_started = None;
                }
                _ => {}
            }
        }
        if let Some(i) = start {
            self.start_watch(i, ctx);
        }
    }

    /// Starts plugin `i`'s `watch` (once its folder's check passed, [`App::gated`]).
    fn start_watch(&mut self, i: usize, ctx: &egui::Context) {
        let Some(w) = self.tests.plugins[i].m.watch.clone() else {
            return;
        };
        match self.gated(i, &w, "", Pending::Watch, ctx) {
            Ok(Some(c)) => {
                let p = &mut self.tests.plugins[i];
                p.watch_started = Some(Instant::now());
                match Run::spawn_group(c, ctx.clone()) {
                    Ok(run) => {
                        p.watch = Some(run);
                        p.watch_error = None;
                    }
                    // tried again after the pause, not every frame
                    Err(e) => {
                        p.watch_error = Some(format!("watch: could not start {}: {e}", w[0]));
                        p.watch_ended();
                    }
                }
            }
            Ok(None) => {}
            Err(e) => self.refused(i, e),
        }
    }

    /// A gutter ▶: one case runs at once; several show in the list to choose.
    pub(super) fn gutter_tests(&mut self, marks: Vec<usize>, ctx: &egui::Context) {
        if let [one] = marks[..] {
            let (plugin, id) = (
                self.tests.marks[one].plugin,
                self.tests.marks[one].id.clone(),
            );
            self.run_case(plugin, &id, None, ctx);
        } else {
            self.tests.active = self.tests.marks[marks[0]].plugin;
            self.tests.plugins[self.tests.active].view = None;
            self.tests.only = marks;
            self.bottom = Bottom::Tests;
        }
    }

    pub(super) fn tests_panel(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.tests.discovering.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking the plugins…");
            });
            return;
        }
        if self.tests.plugins.is_empty() {
            for e in &self.tests.errors {
                ui.colored_label(crate::highlight::ERROR, e);
            }
            ui.label("No test plugins yet. Add one: pick the folder of a test tool that has a Droid Lite plugin (a plugin.json, or a droid-lite-plugin folder).");
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{}  Add plugin…", ic::FOLDER_OPEN))
                    .clicked()
                {
                    self.pick_plugin(&ctx);
                }
                if ui
                    .button(format!("{}  Reload", ic::ARROW_COUNTER_CLOCKWISE))
                    .clicked()
                {
                    self.load_plugins(&ctx);
                }
            });
            return;
        }
        let active = self.tests.active.min(self.tests.plugins.len() - 1);
        self.tests.active = active;
        let mut click = None;
        // header: plugin, cases / views, then actions, links, reload and stop on the right
        ui.horizontal(|ui| {
            if self.tests.plugins.len() > 1 {
                let names: Vec<String> = self
                    .tests
                    .plugins
                    .iter()
                    .map(|p| p.m.name.clone())
                    .collect();
                egui::ComboBox::from_id_salt("plugin")
                    .selected_text(&names[active])
                    .truncate()
                    .show_ui(ui, |ui| {
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut self.tests.active, i, n);
                        }
                    });
            } else {
                ui.strong(&self.tests.plugins[active].m.name);
            }
            let p = &self.tests.plugins[active];
            if ui.selectable_label(p.view.is_none(), "Cases").clicked() {
                click = Some(Click::View(None));
            }
            for (v, n) in p.m.views.iter().enumerate() {
                if ui.selectable_label(p.view == Some(v), &n.name).clicked() {
                    click = Some(Click::View(Some(v)));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if p.run.is_some() {
                    if ui.button(format!("{}  Stop", ic::STOP)).clicked() {
                        click = Some(Click::Stop);
                    }
                } else {
                    if ui
                        .small_button(ic::ARROW_COUNTER_CLOCKWISE)
                        .on_hover_text("Reload the plugins and their lists")
                        .clicked()
                    {
                        click = Some(Click::Reload);
                    }
                    if ui
                        .small_button("Remove")
                        .on_hover_text("Remove this plugin from Droid Lite (its folder stays)")
                        .clicked()
                    {
                        click = Some(Click::Remove);
                    }
                    if ui
                        .small_button(format!("{}  Add plugin…", ic::FOLDER_OPEN))
                        .on_hover_text("Pick a test tool's folder that has a Droid Lite plugin")
                        .clicked()
                    {
                        click = Some(Click::Add);
                    }
                }
                // a project plugin not allowed shows nothing of its own to click
                let links = if p.m.allowed { &p.m.links[..] } else { &[] };
                for (name, url) in links.iter().rev() {
                    if ui.link(name).on_hover_text(url).clicked() {
                        click = Some(Click::Link(url.clone()));
                    }
                }
                for (a, n) in p.m.actions.iter().enumerate().rev() {
                    if ui
                        .add_enabled(
                            p.run.is_none() && p.m.allowed,
                            egui::Button::new(&n.name).small(),
                        )
                        .clicked()
                    {
                        click = Some(Click::Action(a));
                    }
                }
            });
        });
        ui.separator();
        // a project plugin the user has not allowed: what it would run, and nothing else; one that
        // can no longer run at all shows only why (no OK to give)
        let p = &self.tests.plugins[active];
        if let (false, Some(why)) = (p.m.allowed, &p.m.broken) {
            ui.colored_label(crate::highlight::ERROR, why);
        } else if !p.m.allowed {
            if let Some(e) = &p.error {
                ui.colored_label(crate::highlight::ERROR, e);
            }
            ui.label(RichText::new(p.m.approval_text()).strong());
            ui.label("It comes with the project's files. Allow it only if you trust what it runs; a change to any file in its folder asks again.");
            if ui
                .add_enabled(p.allowing.is_none(), egui::Button::new("Allow"))
                .clicked()
            {
                click = Some(Click::Allow);
            }
        } else {
            click = self.tests_lists(ui, active, click);
        }
        match click {
            Some(Click::Run(id)) => self.run_case(active, &id, None, &ctx),
            Some(Click::Action(a)) => self.run_case(active, "", Some(a), &ctx),
            Some(Click::Stop) => {
                if let Some((_, _, run)) = self.tests.plugins[active].run.as_mut() {
                    run.terminate();
                }
            }
            Some(Click::View(v)) => {
                self.tests.plugins[active].view = v;
                if let Some(v) = v {
                    self.load_view(active, v, &ctx);
                }
            }
            Some(Click::Reload) => self.load_plugins(&ctx),
            Some(Click::Open(f, line)) => {
                let offset = std::fs::read_to_string(&f)
                    .map(|t| crate::lsp::offset(&t, line.saturating_sub(1) as u32, 0))
                    .ok();
                self.open_file(&f, offset);
            }
            Some(Click::Link(u)) if self.tests.plugins[active].m.allowed => open_link(&u),
            Some(Click::Link(_)) => {}
            Some(Click::Add) => self.pick_plugin(&ctx),
            Some(Click::Allow) => self.allow_plugin(active, &ctx),
            Some(Click::Remove) => {
                let m = self.tests.plugins[active].m.clone();
                match self.plugins_home().map(|d| plugins::remove(&d, &m)) {
                    Some(Ok(())) => {
                        self.status = format!("Removed the plugin {}", m.name);
                        self.load_plugins(&ctx);
                    }
                    Some(Err(e)) => self.status = e,
                    None => {}
                }
            }
            None => {}
        }
    }

    /// The case list (or a view's rows) on the left and the run's output on the right.
    fn tests_lists(
        &mut self,
        ui: &mut Ui,
        active: usize,
        mut click: Option<Click>,
    ) -> Option<Click> {
        egui::Panel::left("tests-list")
            .resizable(true)
            .default_size(360.0)
            .show(ui, |ui| {
                // keep the dragged width: narrow rows must not shrink the panel
                ui.set_min_width(ui.available_width());
                let left = match self.tests.plugins[active].view {
                    None => self.cases_list(ui, active),
                    Some(_) => rows_list(ui, &self.tests.plugins[active]),
                };
                click = click.take().or(left);
            });
        let right = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 0))
            .show(ui, |ui| output(ui, &self.tests.plugins[active]))
            .inner;
        click = click.or(right);
        click
    }

    /// The case tree, a filtered flat list, or the cases a gutter ▶ picked.
    fn cases_list(&mut self, ui: &mut Ui, active: usize) -> Option<Click> {
        let mut click = None;
        let t = &mut self.tests;
        let p = &t.plugins[active];
        if let Some(e) = &p.error {
            ui.colored_label(crate::highlight::ERROR, e);
        }
        if p.loading.is_some() || p.pending.contains(&Pending::List) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Listing…");
            });
        }
        let busy = p.run.is_some();
        if !t.only.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Cases on this line").strong());
                if ui.small_button("Show all").clicked() {
                    t.only.clear();
                }
            });
            for &m in &t.only {
                let m = &t.marks[m];
                case_row(
                    ui,
                    &m.id,
                    &m.label,
                    "",
                    None,
                    p.status.get(&m.id).copied(),
                    busy,
                    &mut click,
                );
            }
            return click;
        }
        ui.add(
            egui::TextEdit::singleline(&mut t.filter)
                .hint_text("Filter cases")
                .desired_width(ui.available_width()),
        );
        let p = &t.plugins[active];
        let filter = t.filter.to_lowercase();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if filter.is_empty() {
                    tree(ui, &p.cases, &p.status, busy, &mut click);
                } else {
                    flat(ui, &p.cases, &filter, &p.status, busy, &mut click);
                }
            });
        click
    }
}

#[allow(clippy::too_many_arguments)]
fn case_row(
    ui: &mut Ui,
    id: &str,
    label: &str,
    detail: &str,
    file: Option<(&PathBuf, usize)>,
    status: Option<Status>,
    busy: bool,
    click: &mut Option<Click>,
) {
    ui.horizontal(|ui| {
        let (icon, color) = status_look(status);
        ui.label(RichText::new(icon).color(color));
        if ui
            .add_enabled(!busy, egui::Button::new(ic::PLAY).small())
            .on_hover_text(format!("Run {id}"))
            .clicked()
        {
            *click = Some(Click::Run(id.to_owned()));
        }
        let mut r = ui.add(
            egui::Label::new(label)
                .truncate()
                .sense(egui::Sense::click()),
        );
        if !detail.is_empty() {
            r = r.on_hover_text(detail);
        }
        if let Some((f, line)) = file
            && r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        {
            *click = Some(Click::Open(f.clone(), line));
        }
    });
}

fn tree(
    ui: &mut Ui,
    cases: &[Case],
    status: &HashMap<String, Status>,
    busy: bool,
    click: &mut Option<Click>,
) {
    for c in cases {
        let st = c.id.as_ref().and_then(|i| status.get(i).copied());
        if c.children.is_empty() {
            match &c.id {
                Some(id) => case_row(
                    ui,
                    id,
                    &c.label,
                    &c.detail,
                    c.file.as_ref().map(|f| (f, c.line)),
                    st,
                    busy,
                    click,
                ),
                None => {
                    ui.label(&c.label);
                }
            }
            continue;
        }
        let id = ui.make_persistent_id(("case", &c.label, &c.id));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                let (icon, color) = status_look(st);
                ui.label(RichText::new(icon).color(color));
                if let Some(id) = &c.id
                    && ui
                        .add_enabled(!busy, egui::Button::new(ic::PLAY).small())
                        .on_hover_text(format!("Run all of {}", c.label))
                        .clicked()
                {
                    *click = Some(Click::Run(id.clone()));
                }
                ui.add(egui::Label::new(RichText::new(&c.label).strong()).truncate())
                    .on_hover_text(&c.detail);
            })
            .body(|ui| tree(ui, &c.children, status, busy, click));
    }
}

/// Every runnable case whose id, label or detail has `filter`, without the groups.
fn flat(
    ui: &mut Ui,
    cases: &[Case],
    filter: &str,
    status: &HashMap<String, Status>,
    busy: bool,
    click: &mut Option<Click>,
) {
    for c in cases {
        if let Some(id) = &c.id
            && [id, &c.label, &c.detail]
                .iter()
                .any(|s| s.to_lowercase().contains(filter))
        {
            let st = status.get(id).copied();
            case_row(
                ui,
                id,
                &c.label,
                &c.detail,
                c.file.as_ref().map(|f| (f, c.line)),
                st,
                busy,
                click,
            );
        }
        flat(ui, &c.children, filter, status, busy, click);
    }
}

/// Clickable entries: a result or a view row opens its file or link.
fn entries(ui: &mut Ui, rows: &[Entry], click: &mut Option<Click>) {
    for e in rows {
        ui.horizontal(|ui| {
            let (icon, color) = status_look(e.status);
            ui.label(RichText::new(icon).color(color));
            let target = e.file.is_some() || e.url.is_some();
            let text = if e.message.is_empty() {
                e.label.clone()
            } else {
                format!("{}  {}", e.label, e.message)
            };
            let r = ui.add(
                egui::Label::new(text)
                    .truncate()
                    .sense(egui::Sense::click()),
            );
            let r = r.on_hover_text(match (&e.file, &e.url) {
                (Some(f), _) => format!("{}\n{}", e.message, f.display()),
                (None, Some(u)) => format!("{}\n{u}", e.message),
                _ => e.message.clone(),
            });
            if target && r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                *click = Some(match (&e.file, &e.url) {
                    (Some(f), _) => Click::Open(f.clone(), e.line),
                    (None, Some(u)) => Click::Link(u.clone()),
                    _ => unreachable!(),
                });
            }
        });
    }
}

fn rows_list(ui: &mut Ui, p: &Plugin) -> Option<Click> {
    let mut click = None;
    if p.rows_rx.is_some() {
        ui.spinner();
    }
    if let Some(e) = &p.rows_error {
        ui.colored_label(crate::highlight::ERROR, e);
    }
    if p.rows.is_empty() && p.rows_rx.is_none() && p.rows_error.is_none() {
        ui.weak("Nothing here.");
    }
    egui::ScrollArea::vertical()
        .id_salt("rows")
        .auto_shrink([false, false])
        .show(ui, |ui| entries(ui, &p.rows, &mut click));
    click
}

/// Right side: the live state, the run's results, then its log.
fn output(ui: &mut Ui, p: &Plugin) -> Option<Click> {
    let mut click = None;
    if !p.pending.is_empty() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking the plugin's folder…");
        });
    }
    if let Some(e) = &p.watch_error {
        ui.colored_label(crate::highlight::ERROR, e);
    }
    if let Some(l) = &p.live {
        ui.horizontal(|ui| {
            let (icon, color) = status_look(l.status);
            ui.label(RichText::new(icon).color(color));
            ui.strong(&l.title);
        });
        egui::Grid::new("live").num_columns(2).show(ui, |ui| {
            for (k, v) in &l.rows {
                ui.weak(k);
                ui.add(egui::Label::new(v).truncate());
                ui.end_row();
            }
        });
        ui.separator();
    }
    if let Some((_, title, _)) = &p.run {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Running {title}…"));
        });
    }
    entries(ui, &p.results, &mut click);
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt("plugin-log")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_h, p.log.len(), |ui, range| {
            for line in &p.log[range] {
                ui.label(
                    RichText::new(line)
                        .monospace()
                        .color(crate::theme::t().code),
                );
            }
        });
    click
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: &str) -> bool {
        unsafe { libc::kill(pid.trim().parse().unwrap(), 0) == 0 }
    }

    /// `sh -c script` as a plugin command runs it: the leader of its own process group.
    fn group(script: &str, dir: &Path) -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", script]).current_dir(dir);
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c
    }

    fn temp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("droid-lite-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pid_in(f: &Path) -> String {
        for _ in 0..250 {
            if let Ok(p) = std::fs::read_to_string(f)
                && !p.trim().is_empty()
            {
                return p;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no pid in {}", f.display());
    }

    /// A new [`Slot`] for [`fetch_run`].
    fn slot() -> Mutex<Slot> {
        Mutex::new(Slot::Starting)
    }

    /// Waits up to 3 s for `pid` to end.
    fn ends(pid: &str) -> bool {
        let t0 = Instant::now();
        while alive(pid) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        !alive(pid)
    }

    #[test]
    fn a_list_past_its_time_ends_with_its_whole_group() {
        let d = temp("limit");
        let t0 = Instant::now();
        let r = fetch_run(
            group("sleep 30 & echo $! > child.pid; wait", &d),
            &slot(),
            Duration::from_millis(500),
        );
        assert_eq!(r, Err("no answer in 0 s".into()));
        assert!(t0.elapsed() < Duration::from_secs(3));
        let child = pid_in(&d.join("child.pid"));
        assert!(ends(&child), "the tool it started ended too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_dropped_fetch_kills_its_group_and_a_finished_one_gives_its_output() {
        let d = temp("drop");
        let f = fetch(
            group("sleep 30 & echo $! > child.pid; wait", &d),
            &egui::Context::default(),
        );
        let child = pid_in(&d.join("child.pid"));
        assert!(alive(&child));
        drop(f);
        assert!(ends(&child), "a refused plugin's list stops");
        let ok = fetch_run(group("echo '[1]'; echo oops >&2", &d), &slot(), FETCH_LIMIT);
        assert_eq!(ok, Ok("[1]\n".into()));
        let bad = fetch_run(
            group("echo a >&2; echo b >&2; exit 3", &d),
            &slot(),
            FETCH_LIMIT,
        );
        assert_eq!(bad, Err("exit 3: a b".into()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_list_that_prints_too_much_is_stopped_with_its_group() {
        let d = temp("big");
        let t0 = Instant::now();
        let r = fetch_run(
            group(
                "sleep 30 >/dev/null & echo $! > child.pid; head -c 40000000 /dev/zero; wait",
                &d,
            ),
            &slot(),
            Duration::from_secs(3),
        );
        assert_eq!(r, Err("output too big".into()));
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(ends(&pid_in(&d.join("child.pid"))));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A list whose tool left a process holding its output: what it printed counts half a
    /// second after it exited, and that process is killed.
    #[test]
    fn a_list_that_left_a_tool_holding_its_output_ends_after_a_grace() {
        let d = temp("grace");
        let t0 = Instant::now();
        let r = fetch_run(
            group("sleep 30 & echo $! > child.pid; echo '[1]'", &d),
            &slot(),
            Duration::from_secs(5),
        );
        assert_eq!(r, Ok("[1]\n".into()));
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(ends(&pid_in(&d.join("child.pid"))));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Not a test by itself: the next test runs it in a process of its own, which drops its
    /// plugins' list and run and quits at once, as the app does.
    #[test]
    #[ignore]
    fn quit_helper() {
        let Some(d) = std::env::var_os("DROID_LITE_QUIT_DIR").map(PathBuf::from) else {
            return;
        };
        let ctx = egui::Context::default();
        let mut p = Plugin::new(Manifest::default());
        p.loading = Some(fetch(
            group("sleep 30 & echo $! > list.pid; wait", &d),
            &ctx,
        ));
        let run = Run::spawn_group(group("sleep 30 & echo $! > run.pid; wait", &d), ctx).unwrap();
        p.run = Some((String::new(), "t".into(), run));
        pid_in(&d.join("list.pid"));
        pid_in(&d.join("run.pid"));
        let mut t = Tests::default();
        t.plugins.push(p);
        drop(t);
        std::process::exit(0);
    }

    #[test]
    fn quitting_ends_every_list_and_run_of_the_plugins() {
        let d = temp("quit");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["app::tests_ui::tests::quit_helper", "--exact", "--ignored"])
            .env("DROID_LITE_QUIT_DIR", &d)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        for f in ["list.pid", "run.pid"] {
            assert!(ends(&pid_in(&d.join(f))), "{f}: ended with the app");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A run shows at most 20000 lines and 8 MB, status lines too (as the VS Code extension);
    /// past that a case still running ends once, and no new case gets a status.
    #[test]
    fn a_flooding_run_keeps_its_output_bounded() {
        let mut p = Plugin::new(Manifest::default());
        p.status.insert("flood".into(), Status::Running);
        for i in 0..30000 {
            p.run_line(
                &format!(r#"{{"case":"c{i}","status":"passed","label":"step"}}"#),
                None,
            );
        }
        for _ in 0..30000 {
            p.run_line(r#"{"case":"flood","status":"failed","message":"m"}"#, None);
        }
        let notice = "[more than 20000 lines or 8 MB: the rest is not shown]";
        assert!(
            p.results.len() + p.log.len() <= MAX_LOG + 1,
            "{}",
            p.results.len()
        );
        assert_eq!(p.log, [notice]);
        assert!(p.status.len() <= MAX_LOG + 1);
        assert_eq!(p.status.get("flood"), Some(&Status::Failed));
        assert_eq!(
            p.results
                .iter()
                .filter(|e| e.case.as_deref() == Some("flood"))
                .count(),
            0
        );
        let mut p = Plugin::new(Manifest::default());
        let line = "x".repeat(60000);
        for _ in 0..200 {
            p.run_line(&line, None);
        }
        p.run_line("after", None);
        let bytes: usize = p.log.iter().map(String::len).sum();
        assert!(bytes <= MAX_LOG_BYTES + notice.len() && p.log.len() > 100);
        assert_eq!(p.log.last().map(String::as_str), Some(notice));
    }
}
