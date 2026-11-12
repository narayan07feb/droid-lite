//! The IDE window: project tree, editor tabs, search popups, completion, Compose panel, Gradle console.

mod disk;
#[cfg(test)]
mod e2e;
mod git_ui;
mod refactor_ui;

use crate::compose::{self, Lint, Severity, Stability, Verdict};
use crate::edit;
use crate::gradle::{self, PreviewTool};
use crate::highlight::{self, ERROR, INFO, TEXT, WARN};
use crate::icons as ic;
use crate::index::{self, Hit, Index};
use crate::keymap::{self, Action, Keymap};
use crate::kotlin::{Decl, FileSyms, scan};
use crate::lsp::{self, Lsp};
use crate::project::{Project, is_code};
use crate::theme::{self, DARK};
use egui::text::{CCursor, CCursorRange};
use egui::{Align, Align2, Color32, FontId, Key, Modifiers, RichText, TextEdit, Ui};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Instant, SystemTime};

const FONT_SIZE: f32 = 13.0;
/// Back/forward and recent-file lists are capped so a long session does not grow them forever.
const MAX_HISTORY: usize = 200;
const MAX_RECENT: usize = 50;

fn push_capped<T>(v: &mut Vec<T>, item: T) {
    v.push(item);
    if v.len() > MAX_HISTORY {
        v.remove(0);
    }
}

struct Tab {
    path: PathBuf,
    text: String,
    saved: String,
    syms: FileSyms,
    lints: Vec<Lint>,
    diagnostics: Vec<(usize, u8, String)>,
    /// Laid-out text, reused until the text, the problem marks or the zoom change.
    cache: Option<EditorCache>,
    /// The next layout must re-colour the whole file (set when analysis settles).
    recolor: bool,
    /// Text changed at this time and is not analysed yet; analysis waits for a typing pause.
    dirty_since: Option<Instant>,
    /// Byte offset to put the cursor at (and scroll to) on the next frame.
    goto: Option<usize>,
    cursor: usize,
    /// Selected byte range (empty when nothing is selected).
    selection: std::ops::Range<usize>,
    /// Changed on disk while this tab had unsaved edits.
    disk_changed: bool,
    deleted: bool,
    git: git_ui::TabBlame,
    /// Bumped on every text change; caches key on it.
    rev: u64,
}

impl Tab {
    fn dirty(&self) -> bool {
        self.text != self.saved
    }
    fn reanalyse(&mut self) {
        self.rev += 1;
        self.syms = scan(&self.text);
        self.lints = if is_code(&self.path) {
            compose::lint(&self.text, &self.syms)
        } else {
            Vec::new()
        };
        self.recolor = true;
        self.dirty_since = None;
    }
}

/// Times the parts of a frame. A frame over `LIMIT` is reported with its slowest part, so a
/// freeze is never silent: it shows in the status bar and on stderr.
struct Watchdog {
    start: Instant,
    stages: Vec<(&'static str, f32)>,
}

impl Watchdog {
    const LIMIT_MS: f32 = 100.0;

    fn start() -> Watchdog {
        Watchdog {
            start: Instant::now(),
            stages: Vec::new(),
        }
    }

    fn stage<R>(&mut self, name: &'static str, f: impl FnOnce() -> R) -> R {
        let t = Instant::now();
        let r = f();
        self.stages.push((name, t.elapsed().as_secs_f32() * 1000.0));
        r
    }

    /// The report for a slow frame. The first frames (fonts, first layout) are skipped.
    fn finish(self, frame: u64) -> Option<String> {
        let total = self.start.elapsed().as_secs_f32() * 1000.0;
        if frame <= 3 || total < Self::LIMIT_MS {
            return None;
        }
        let (name, ms) = self
            .stages
            .iter()
            .cloned()
            .fold(("", 0.0), |a, b| if b.1 > a.1 { b } else { a });
        Some(format!(
            "slow frame {total:.0} ms, mostly {name} ({ms:.0} ms)"
        ))
    }
}

/// Coloured text and its layout, reused across frames and patched while typing.
struct EditorCache {
    key: u64,
    job: egui::text::LayoutJob,
    galley: std::sync::Arc<egui::Galley>,
}

/// Analysis waits this long after the last keystroke, so typing in big files stays smooth.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq)]
enum SearchKind {
    Class,
    File,
    Symbol,
    Everywhere,
    Recent,
    Structure,
    FindInFiles,
    GotoLine,
}

impl SearchKind {
    fn action(self) -> Action {
        match self {
            SearchKind::Class => Action::GotoClass,
            SearchKind::File => Action::GotoFile,
            SearchKind::Symbol => Action::GotoSymbol,
            SearchKind::Everywhere => Action::Everywhere,
            SearchKind::Recent => Action::Recent,
            SearchKind::Structure => Action::Structure,
            SearchKind::FindInFiles => Action::FindInFiles,
            SearchKind::GotoLine => Action::GotoLine,
        }
    }
    fn hint(self) -> &'static str {
        match self {
            SearchKind::FindInFiles => "Text to find in every file, then Enter",
            SearchKind::GotoLine => "line or line:column, then Enter",
            _ => "Type to search; camel humps work: HSVM → HomeScreenViewModel",
        }
    }
}

struct Search {
    kind: SearchKind,
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    stale: bool,
}

struct Completion {
    start: usize,
    items: Vec<Item>,
    selected: usize,
}

#[derive(Clone)]
struct Item {
    label: String,
    detail: String,
    insert: String,
    import: Option<String>,
}

fn found_line(path: &Path, text: &str, o: usize) -> FoundLine {
    let ls = text[..o].rfind('\n').map_or(0, |i| i + 1);
    let le = text[o..].find('\n').map_or(text.len(), |i| o + i);
    FoundLine {
        path: path.to_path_buf(),
        offset: o,
        line: text[..o].matches('\n').count() + 1,
        text: text[ls..le].trim().chars().take(160).collect(),
    }
}

#[derive(Clone)]
struct FoundLine {
    path: PathBuf,
    offset: usize,
    line: usize,
    text: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Bottom {
    Problems,
    Build,
    Find,
    Git,
}

/// What the Compose panel shows for one composable; rebuilt only when its inputs change.
struct ComposeView {
    key: (PathBuf, u64, u64, u64, String),
    tree: Vec<compose::Node>,
    stability: Stability,
    guessed: bool,
    tool: Option<(crate::project::Module, PreviewTool)>,
    images: Vec<(String, PathBuf)>,
}

#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: Vec<PathBuf>,
}

pub struct App {
    project: Option<Project>,
    tree: Vec<(String, DirNode)>,
    index: Index,
    indexing: Option<Receiver<(Project, Index, Option<String>)>>,
    /// kotlin-lsp was started (or found missing) for this project.
    lsp_tried: bool,
    stability: HashMap<String, Stability>,
    stability_rx: Option<Receiver<HashMap<String, Stability>>>,
    lsp: Option<Lsp>,
    tabs: Vec<Tab>,
    active: usize,
    history: Vec<(PathBuf, usize)>,
    forward: Vec<(PathBuf, usize)>,
    recent: Vec<PathBuf>,
    search: Option<Search>,
    completion: Option<Completion>,
    gradle: Option<gradle::Run>,
    gradle_task: String,
    console: Vec<String>,
    found: Vec<FoundLine>,
    find_rx: Option<Receiver<Vec<FoundLine>>>,
    bottom: Bottom,
    selected_composable: Option<String>,
    textures: HashMap<PathBuf, (SystemTime, egui::TextureHandle)>,
    shift_down: bool,
    last_shift: Option<Instant>,
    status: String,
    /// Dev check: `DROID_LITE_SHOT=out.png` saves a screenshot once indexing is done, then quits.
    shot: Option<(PathBuf, u32)>,
    refactor: Option<refactor_ui::Refactor>,
    undo: Option<refactor_ui::Undo>,
    watcher: Option<crate::watch::Watcher>,
    /// Disk changes being read and scanned on worker threads.
    disk_rx: Vec<Receiver<Vec<disk::DiskChange>>>,
    /// Liquid Glass is behind the window.
    glass: bool,
    /// Content runs under the title bar: leave room for the window buttons.
    fullsize: bool,
    git: git_ui::GitState,
    /// Current git branch of the project, shown in the toolbar.
    branch: Option<String>,
    /// Bumped when a build or a report reload may have changed previews and stability.
    outputs_gen: u64,
    compose_view: Option<ComposeView>,
    ctx: Option<egui::Context>,
    keymap: Keymap,
    show_shortcuts: bool,
    /// What the Find tab lists, e.g. "References to `User`".
    found_title: String,
    frames: u64,
    /// Last slow-frame report from the watchdog, shown in the status bar for a while.
    slow: Option<(String, Instant)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>, glass_wanted: bool) -> App {
        let glass = glass_wanted && crate::glass::apply(cc);
        if glass_wanted && !glass {
            eprintln!(
                "droid-lite: Liquid Glass not available (macOS 26+ needed); using solid colours"
            );
        }
        crate::theme::install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(crate::theme::visuals(glass));
        crate::theme::style(&cc.egui_ctx);
        let mut app = App {
            project: None,
            tree: Vec::new(),
            index: Index::default(),
            indexing: None,
            lsp_tried: false,
            stability: HashMap::new(),
            stability_rx: None,
            lsp: None,
            tabs: Vec::new(),
            active: 0,
            history: Vec::new(),
            forward: Vec::new(),
            recent: Vec::new(),
            search: None,
            completion: None,
            gradle: None,
            gradle_task: "assembleDebug".into(),
            console: Vec::new(),
            found: Vec::new(),
            find_rx: None,
            bottom: Bottom::Problems,
            selected_composable: None,
            textures: HashMap::new(),
            shift_down: false,
            last_shift: None,
            status: String::new(),
            shot: std::env::var_os("DROID_LITE_SHOT").map(|p| (PathBuf::from(p), 0)),
            refactor: None,
            undo: None,
            watcher: None,
            disk_rx: Vec::new(),
            glass,
            fullsize: glass_wanted,
            git: Default::default(),
            branch: None,
            outputs_gen: 0,
            compose_view: None,
            ctx: Some(cc.egui_ctx.clone()),
            keymap: load_setting("keymap").map_or(Keymap::Eclipse, |s| Keymap::from_name(&s)),
            show_shortcuts: false,
            found_title: String::new(),
            frames: 0,
            slow: None,
        };
        if let Some(dir) = open.or_else(last_project) {
            app.open_project(&dir, &cc.egui_ctx);
        }
        if let Some(f) = std::env::var_os("DROID_LITE_OPEN") {
            app.open_file(Path::new(&f), None);
        }
        if std::env::var_os("DROID_LITE_BLAME").is_some() {
            app.git.show_column = true;
            app.bottom = Bottom::Git;
        }
        if let Ok(q) = std::env::var("DROID_LITE_SEARCH") {
            app.search = Some(Search {
                kind: SearchKind::Class,
                query: q,
                hits: Vec::new(),
                selected: 0,
                stale: true,
            });
        }
        app
    }

    /// Opens a project without blocking the window: listing files, indexing and reading the git
    /// branch run on a worker thread; `poll` installs the result.
    fn open_project(&mut self, dir: &Path, ctx: &egui::Context) {
        for t in std::mem::take(&mut self.tabs) {
            self.forget_tab(&t);
        }
        self.history.clear();
        self.forward.clear();
        self.recent.clear();
        self.index = Index::default();
        self.gradle = None; // Drop stops a running build of the old project
        self.compose_view = None;
        self.textures.clear();
        self.found.clear();
        self.console.clear();
        self.git = Default::default();
        self.stability.clear();
        self.refactor = None;
        self.undo = None;
        self.lsp = None;
        self.lsp_tried = false;
        self.watcher = None;
        self.project = None;
        self.tree.clear();
        let (tx, rx) = channel();
        self.status = format!("Opening {}…", dir.display());
        let (dir, c) = (dir.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let project = Project::open(&dir);
            let index = Index::build(&project.files);
            let branch = crate::git::branch(&project.root);
            let _ = tx.send((project, index, branch));
            c.request_repaint();
        });
        self.indexing = Some(rx);
    }

    /// Installs a project opened in the background.
    fn project_ready(
        &mut self,
        project: Project,
        index: Index,
        branch: Option<String>,
        ctx: &egui::Context,
    ) {
        remember_project(&project.root);
        self.tree = build_tree(&project);
        self.index = index;
        for t in &self.tabs {
            self.index.update(&t.path, &t.text);
        }
        self.branch = branch;
        self.reload_stability(&project, ctx);
        self.watcher = crate::watch::Watcher::start(&project.root, ctx.clone());
        self.status = format!(
            "Opened {} ({} modules, {} files, {} symbols)",
            project.name,
            project.modules.len(),
            project.files.len(),
            self.index.symbol_count()
        );
        self.project = Some(project);
        if self.tabs.iter().any(|t| is_code(&t.path)) {
            self.start_lsp();
        }
    }

    /// kotlin-lsp is a JVM that can use gigabytes, so it starts only when a code file opens,
    /// with its heap capped (`DROID_LITE_LSP_HEAP`, default 2g).
    fn start_lsp(&mut self) {
        if self.lsp_tried {
            return;
        }
        let (Some(p), Some(ctx)) = (&self.project, &self.ctx) else {
            return;
        };
        self.lsp_tried = true;
        self.lsp = Lsp::start(&p.root, ctx.clone());
        if let Some(l) = self.lsp.as_mut() {
            for t in self.tabs.iter().filter(|t| is_code(&t.path)) {
                l.did_open(&t.path, &t.text);
            }
        }
    }

    fn reload_stability(&mut self, project: &Project, ctx: &egui::Context) {
        let dirs: Vec<PathBuf> = project.modules.iter().map(|m| m.dir.clone()).collect();
        let (tx, rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(compose::load_reports(&dirs));
            c.request_repaint();
        });
        self.stability_rx = Some(rx);
    }

    fn open_file(&mut self, path: &Path, offset: Option<usize>) {
        if let Some(t) = self.tabs.get(self.active) {
            push_capped(&mut self.history, (t.path.clone(), t.cursor));
            self.forward.clear();
        }
        self.show_file(path, offset);
    }

    /// Opens or focuses a tab without touching the back/forward history.
    fn show_file(&mut self, path: &Path, offset: Option<usize>) {
        self.completion = None;
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
        } else {
            let Ok(text) = std::fs::read_to_string(path) else {
                self.status = format!("Cannot read {}", path.display());
                return;
            };
            let mut tab = Tab {
                path: path.to_path_buf(),
                saved: text.clone(),
                text,
                syms: FileSyms::default(),
                lints: Vec::new(),
                diagnostics: Vec::new(),
                cache: None,
                recolor: false,
                dirty_since: None,
                goto: None,
                cursor: 0,
                selection: 0..0,
                disk_changed: false,
                deleted: false,
                git: Default::default(),
                rev: 0,
            };
            tab.reanalyse();
            if let Some(l) = self.lsp.as_mut().filter(|_| is_code(path)) {
                l.did_open(path, &tab.text);
            }
            let start_lsp = is_code(path) && !self.lsp_tried;
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
            if start_lsp {
                self.start_lsp();
            }
        }
        let tab = &mut self.tabs[self.active];
        tab.goto = Some(offset.unwrap_or(tab.cursor));
    }

    fn back(&mut self) {
        if let Some((p, o)) = self.history.pop() {
            if let Some(t) = self.tabs.get(self.active) {
                self.forward.push((t.path.clone(), t.cursor));
            }
            self.show_file(&p, Some(o));
        }
    }

    fn fwd(&mut self) {
        if let Some((p, o)) = self.forward.pop() {
            if let Some(t) = self.tabs.get(self.active) {
                push_capped(&mut self.history, (t.path.clone(), t.cursor));
            }
            self.show_file(&p, Some(o));
        }
    }

    fn save(&mut self) {
        self.settle(true);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        match std::fs::write(&tab.path, &tab.text) {
            Ok(()) => {
                tab.saved = tab.text.clone();
                self.index.update(&tab.path, &tab.text);
                if let Some(l) = self.lsp.as_mut() {
                    l.did_save(&tab.path);
                }
                self.status = format!("Saved {}", tab.path.display());
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            if self.tabs[i].dirty() {
                self.status = format!(
                    "{} has unsaved changes: save (⌘S) or revert first",
                    self.tabs[i].path.display()
                );
                return;
            }
            let t = self.tabs.remove(i);
            self.forget_tab(&t);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        }
    }

    /// Frees what a closed tab left behind: the editor's undo history in egui memory and the LSP document.
    fn forget_tab(&mut self, t: &Tab) {
        // must match the editor's TextEdit id
        let id = egui::Id::unique(("editor", &t.path));
        if let Some(ctx) = &self.ctx {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
        }
        if let Some(l) = self.lsp.as_mut() {
            l.did_close(&t.path);
        }
        if self
            .compose_view
            .as_ref()
            .is_some_and(|v| v.key.0 == t.path)
        {
            self.compose_view = None;
        }
    }

    fn revert(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.text = t.saved.clone();
            t.reanalyse();
        }
    }

    fn word_at_cursor(&self) -> Option<(String, usize)> {
        let t = self.tabs.get(self.active)?;
        let (a, b) = word_bounds(&t.text, t.cursor);
        (a < b).then(|| (t.text[a..b].to_string(), a))
    }

    fn goto_definition(&mut self) {
        self.settle(true);
        let Some((word, at)) = self.word_at_cursor() else {
            return;
        };
        let path = self.tabs[self.active].path.clone();
        if let Some(l) = self.lsp.as_mut().filter(|l| l.ready) {
            let (line, col) = lsp::position(&self.tabs[self.active].text, at);
            l.definition(&path, line, col);
        }
        let defs = self.index.definitions(&word, &path);
        match defs.len() {
            0 => self.status = format!("No declaration found for `{word}` in the project"),
            1 => self.open_file(&defs[0].path.clone(), Some(defs[0].offset)),
            _ => {
                self.search = Some(Search {
                    kind: SearchKind::Symbol,
                    query: word,
                    hits: defs,
                    selected: 0,
                    stale: false,
                });
            }
        }
    }

    /// Find References: kotlin-lsp when it runs, else every code reference in the project
    /// (Kotlin/Java/Gradle scripts by token, so strings and comments are skipped) plus XML files
    /// such as layouts and the manifest.
    fn find_references(&mut self, ctx: &egui::Context) {
        let Some((word, at)) = self.word_at_cursor() else {
            self.status = "Put the cursor on a name to find its references".into();
            return;
        };
        self.found_title = format!("References to `{word}`");
        self.bottom = Bottom::Find;
        if let Some(l) = self.lsp.as_mut().filter(|l| l.ready) {
            let t = &self.tabs[self.active];
            let (line, col) = lsp::position(&t.text, at);
            l.references(&t.path, line, col);
            self.status = format!("Asking kotlin-lsp for references to `{word}`…");
            return;
        }
        self.search_project(word, true, ctx);
    }

    /// Searches every project file (open tabs use their unsaved text) in the background.
    /// `code_refs`: whole identifiers only, outside strings and comments in code files.
    fn search_project(&mut self, word: String, code_refs: bool, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let files: Vec<PathBuf> = p.files.clone();
        let open: HashMap<PathBuf, String> = self
            .tabs
            .iter()
            .map(|t| (t.path.clone(), t.text.clone()))
            .collect();
        let (tx, rx) = channel();
        let c = ctx.clone();
        self.status = format!("Searching for `{word}`…");
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for f in files {
                let Some(text) = open
                    .get(&f)
                    .cloned()
                    .or_else(|| std::fs::read_to_string(&f).ok())
                else {
                    continue;
                };
                if !text.contains(&word) {
                    continue;
                }
                let hits: Vec<usize> = if code_refs && is_code(&f) {
                    crate::refactor::occurrences(&text, &word)
                        .into_iter()
                        .map(|r| r.start)
                        .collect()
                } else {
                    text.match_indices(&word)
                        .map(|(o, _)| o)
                        .filter(|&o| !code_refs || word_bounds(&text, o) == (o, o + word.len()))
                        .collect()
                };
                out.extend(hits.into_iter().map(|o| found_line(&f, &text, o)));
                if out.len() >= 5000 {
                    break;
                }
            }
            let _ = tx.send(out);
            c.request_repaint();
        });
        self.find_rx = Some(rx);
        self.bottom = Bottom::Find;
    }

    fn run_gradle(&mut self, task: String, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        if let Some(r) = self.gradle.as_mut() {
            r.stop();
        }
        self.console.clear();
        self.console.push(format!("> ./gradlew {task}"));
        match gradle::Run::start(p, &task, ctx.clone()) {
            Ok(r) => self.gradle = Some(r),
            Err(e) => self.console.push(format!("Could not start Gradle: {e}")),
        }
        self.bottom = Bottom::Build;
    }

    // ---------- per-frame background work ----------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.indexing
            && let Ok((project, index, branch)) = rx.try_recv()
        {
            self.indexing = None;
            self.project_ready(project, index, branch, ctx);
        }
        if let Some(rx) = &self.stability_rx
            && let Ok(s) = rx.try_recv()
        {
            self.stability = s;
            self.outputs_gen += 1;
            self.stability_rx = None;
        }
        if let Some(rx) = &self.find_rx
            && let Ok(f) = rx.try_recv()
        {
            self.status = format!("{} matches", f.len());
            self.found = f;
            self.find_rx = None;
        }
        if let Some(run) = self.gradle.as_mut() {
            let mut done = None;
            for l in run.poll() {
                match l {
                    gradle::Line::Out(s) => self.console.push(s),
                    gradle::Line::Done(code) => done = Some(code),
                }
            }
            if self.console.len() > 20_000 {
                self.console.drain(..self.console.len() - 20_000);
            }
            if let Some(code) = done {
                self.console.push(format!(
                    "Gradle finished: {}",
                    code.map_or("killed".into(), |c| if c == 0 {
                        "OK".into()
                    } else {
                        format!("exit {c}")
                    })
                ));
                self.gradle = None;
                self.outputs_gen += 1;
                // a build can write new stability dumps or Compose compiler reports
                if let Some(p) = self.project.take() {
                    self.reload_stability(&p, ctx);
                    self.project = Some(p);
                }
            }
        }
        let events = self.lsp.as_mut().map(|l| l.poll()).unwrap_or_default();
        for e in events {
            match e {
                lsp::Event::Completion { at, items } => {
                    if let Some(c) = self.completion.as_mut().filter(|c| c.start == at) {
                        let mut lsp_items: Vec<Item> = items
                            .into_iter()
                            .map(|(label, detail, insert)| Item {
                                insert: insert.unwrap_or_else(|| label.clone()),
                                label,
                                detail,
                                import: None,
                            })
                            .collect();
                        lsp_items.extend(c.items.drain(..).filter(|i| i.import.is_some()));
                        c.items = lsp_items;
                        c.items.truncate(80);
                    }
                }
                lsp::Event::Definition(locs) => {
                    if let Some((p, line, col)) = locs.into_iter().next()
                        && let Ok(text) = std::fs::read_to_string(&p)
                    {
                        let o = lsp::offset(&text, line, col);
                        if self
                            .tabs
                            .get(self.active)
                            .is_none_or(|t| t.path != p || t.cursor.abs_diff(o) > 0)
                        {
                            self.open_file(&p, Some(o));
                        }
                    }
                }
                lsp::Event::Rename(edits) => self.on_lsp_rename(edits),
                lsp::Event::References(locs) => {
                    let mut texts: HashMap<PathBuf, String> = HashMap::new();
                    let mut found = Vec::new();
                    for (p, line, col) in locs {
                        let text = texts.entry(p.clone()).or_insert_with(|| {
                            self.tabs
                                .iter()
                                .find(|t| t.path == p)
                                .map(|t| t.text.clone())
                                .or_else(|| std::fs::read_to_string(&p).ok())
                                .unwrap_or_default()
                        });
                        found.push(found_line(&p, text, lsp::offset(text, line, col)));
                    }
                    found.sort_by(|a, b| a.path.cmp(&b.path).then(a.offset.cmp(&b.offset)));
                    self.status = format!("{} references (kotlin-lsp)", found.len());
                    self.found = found;
                }
                lsp::Event::Diagnostics(path, diags) => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.diagnostics = diags
                            .into_iter()
                            .map(|(l, c, s, m)| (lsp::offset(&t.text, l, c), s, m))
                            .collect();
                        t.recolor = true;
                    }
                }
            }
        }
    }

    // ---------- keyboard ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        for (action, chord) in keymap::bindings(self.keymap) {
            if ctx.input_mut(|i| i.consume_shortcut(&chord)) {
                self.run(action, ctx);
            }
        }
        let open = |kind| {
            Some(Search {
                kind,
                query: String::new(),
                hits: Vec::new(),
                selected: 0,
                stale: true,
            })
        };
        // double Shift: two Shift presses within 400 ms with no other key between
        let (shift, other_key) = ctx.input(|i| {
            (
                i.modifiers.shift && !i.modifiers.command && !i.modifiers.alt,
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Key { .. } | egui::Event::Text(_))),
            )
        });
        if other_key {
            self.last_shift = None;
        }
        if shift && !self.shift_down {
            if self
                .last_shift
                .is_some_and(|t| t.elapsed().as_millis() < 400)
            {
                self.search = open(SearchKind::Everywhere);
                self.last_shift = None;
            } else {
                self.last_shift = Some(Instant::now());
            }
        }
        self.shift_down = shift;
    }

    /// Analyses edited tabs once typing pauses (or now, when `force`): symbols, Compose checks,
    /// the project index and kotlin-lsp. Commands that read them force it first.
    fn settle(&mut self, force: bool) {
        for t in self.tabs.iter_mut() {
            let Some(at) = t.dirty_since else { continue };
            if !force && at.elapsed() < SETTLE {
                if let Some(ctx) = &self.ctx {
                    ctx.request_repaint_after(SETTLE - at.elapsed());
                }
                continue;
            }
            t.reanalyse();
            self.index.update(&t.path, &t.text);
            if let Some(l) = self.lsp.as_mut() {
                l.did_change(&t.path, &t.text);
            }
        }
    }

    /// Runs one command, from a shortcut or a menu.
    fn run(&mut self, action: Action, ctx: &egui::Context) {
        self.settle(true);
        let open = |kind| {
            Some(Search {
                kind,
                query: String::new(),
                hits: Vec::new(),
                selected: 0,
                stale: true,
            })
        };
        match action {
            Action::GotoClass => self.search = open(SearchKind::Class),
            Action::GotoFile => self.search = open(SearchKind::File),
            Action::GotoSymbol => self.search = open(SearchKind::Symbol),
            Action::Everywhere => self.search = open(SearchKind::Everywhere),
            Action::Recent => self.search = open(SearchKind::Recent),
            Action::Structure => self.search = open(SearchKind::Structure),
            Action::GotoLine => self.search = open(SearchKind::GotoLine),
            Action::FindInFiles => self.search = open(SearchKind::FindInFiles),
            Action::GotoDefinition => self.goto_definition(),
            Action::References => self.find_references(ctx),
            Action::Back => self.back(),
            Action::Forward => self.fwd(),
            Action::Complete => {
                if !self.tabs.is_empty() {
                    self.update_completion(true);
                }
            }
            Action::Rename => self.start_rename(),
            Action::ExtractVariable => self.start_extract(),
            Action::OptimizeImports => self.optimize_imports(),
            Action::ToggleComment => self.line_edit(|t, s| Some(edit::toggle_comment(t, s))),
            Action::DeleteLine => self.line_edit(|t, s| Some(edit::delete_lines(t, s))),
            Action::DuplicateLine => self.line_edit(|t, s| Some(edit::duplicate_lines(t, s))),
            Action::MoveLineUp => self.line_edit(|t, s| edit::move_lines(t, s, true)),
            Action::MoveLineDown => self.line_edit(|t, s| edit::move_lines(t, s, false)),
            Action::Save => self.save(),
            Action::CloseTab => self.close_tab(self.active),
            Action::Shortcuts => self.show_shortcuts = !self.show_shortcuts,
        }
    }

    /// Applies a line command to the active tab's selection (or cursor line).
    fn line_edit(&mut self, f: impl Fn(&str, std::ops::Range<usize>) -> Option<(String, usize)>) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let sel = if t.selection.is_empty() {
            t.cursor..t.cursor
        } else {
            t.selection.clone()
        };
        let Some((text, cursor)) = f(&t.text, sel) else {
            return;
        };
        t.text = text;
        t.goto = Some(cursor.min(t.text.len()));
        t.reanalyse();
        let (path, text) = (t.path.clone(), t.text.clone());
        self.index.update(&path, &text);
        if let Some(l) = self.lsp.as_mut() {
            l.did_change(&path, &text);
        }
    }

    pub(super) fn set_keymap(&mut self, km: Keymap) {
        self.keymap = km;
        save_setting("keymap", km.name());
        self.status = format!("Keymap: {}", km.name());
    }

    fn help_menu(&mut self, ui: &mut Ui) {
        ui.menu_button("Help", |ui| {
            if ui
                .button(format!(
                    "{}  Keyboard shortcuts   {}",
                    ic::KEYBOARD,
                    keymap::text(self.keymap, Action::Shortcuts)
                ))
                .clicked()
            {
                self.show_shortcuts = true;
                ui.close();
            }
            ui.separator();
            ui.label(RichText::new("Keymap").color(DARK.weak).small());
            for km in [Keymap::Eclipse, Keymap::AndroidStudio] {
                if ui.radio(self.keymap == km, km.name()).clicked() {
                    self.set_keymap(km);
                    ui.close();
                }
            }
        });
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_shortcuts;
        egui::Window::new(format!("Keyboard shortcuts · {}", self.keymap.name()))
            .open(&mut open)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("shortcuts")
                    .striped(true)
                    .num_columns(2)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        let mut seen = Vec::new();
                        for (action, _) in keymap::bindings(self.keymap) {
                            if seen.contains(&action) {
                                continue;
                            }
                            seen.push(action);
                            ui.label(action.label());
                            ui.label(
                                RichText::new(keymap::text(self.keymap, action))
                                    .monospace()
                                    .color(DARK.accent),
                            );
                            ui.end_row();
                        }
                        ui.label("Go to declaration");
                        ui.label(RichText::new("⌘-click").monospace().color(DARK.accent));
                        ui.end_row();
                    });
            });
        self.show_shortcuts = open;
    }

    // ---------- search popup ----------

    fn search_window(&mut self, ctx: &egui::Context) {
        let Some(mut s) = self.search.take() else {
            return;
        };
        let mut chosen: Option<(PathBuf, usize)> = None;
        let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let down = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown));
        let up = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp));
        let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        if down {
            s.selected = (s.selected + 1).min(s.hits.len().saturating_sub(1));
        }
        if up {
            s.selected = s.selected.saturating_sub(1);
        }
        if s.stale {
            s.hits = self.search_hits(s.kind, &s.query);
            s.selected = 0;
            s.stale = false;
        }
        if enter {
            if s.kind == SearchKind::GotoLine {
                if let Some(t) = self.tabs.get(self.active)
                    && let Some(o) = edit::line_offset(&t.text, &s.query)
                {
                    chosen = Some((t.path.clone(), o));
                }
                close = true;
            } else if s.kind == SearchKind::FindInFiles {
                if !s.query.is_empty() {
                    self.found_title = format!("Text `{}`", s.query);
                    self.search_project(s.query.clone(), false, ctx);
                }
                close = true;
            } else if let Some(h) = s.hits.get(s.selected) {
                chosen = Some((h.path.clone(), h.offset));
            }
        }
        let title = format!(
            "{}   {}",
            s.kind.action().label(),
            keymap::text(self.keymap, s.kind.action())
        );
        let hint = s.kind.hint();
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 60.0])
            .default_width(640.0)
            .show(ctx, |ui| {
                let r = ui.add(
                    TextEdit::singleline(&mut s.query)
                        .desired_width(f32::INFINITY)
                        .hint_text(hint),
                );
                r.request_focus();
                if r.changed() {
                    s.stale = true;
                }
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        for (i, h) in s.hits.iter().enumerate() {
                            let sel = i == s.selected;
                            let resp = ui.horizontal(|ui| {
                                let r = ui.selectable_label(
                                    sel,
                                    RichText::new(&h.name).strong().monospace(),
                                );
                                ui.label(RichText::new(&h.detail).weak().small());
                                r
                            });
                            if sel && (up || down) {
                                resp.inner.scroll_to_me(Some(Align::Center));
                            }
                            if resp.inner.clicked() {
                                chosen = Some((h.path.clone(), h.offset));
                            }
                        }
                    });
            });
        if let Some((p, o)) = chosen {
            self.open_file(&p, Some(o));
            return;
        }
        if !close {
            self.search = Some(s);
        }
    }

    fn search_hits(&self, kind: SearchKind, q: &str) -> Vec<Hit> {
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        let file_hit = |p: &PathBuf| Hit {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            detail: p
                .strip_prefix(&root)
                .unwrap_or(p)
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
            path: p.clone(),
            offset: 0,
            score: 0,
        };
        fn file_name<'x>(p: &'x &PathBuf) -> &'x str {
            p.file_name().and_then(|n| n.to_str()).unwrap_or("")
        }
        let files = |q: &str, paths: &mut dyn Iterator<Item = &PathBuf>, limit| -> Vec<Hit> {
            index::rank_by(q, paths, file_name, limit)
                .into_iter()
                .map(|(score, p)| Hit {
                    score,
                    ..file_hit(p)
                })
                .collect()
        };
        match kind {
            SearchKind::Class => self.index.search(q, true, 60),
            SearchKind::Symbol => self.index.search(q, false, 60),
            SearchKind::File => files(q, &mut self.project.iter().flat_map(|p| p.files.iter()), 60),
            SearchKind::Recent => files(q, &mut self.recent.iter(), 60),
            SearchKind::Everywhere => {
                let mut v = self.index.search(q, false, 30);
                v.extend(files(
                    q,
                    &mut self.project.iter().flat_map(|p| p.files.iter()),
                    15,
                ));
                v.sort_by_key(|h| std::cmp::Reverse(h.score));
                v
            }
            SearchKind::Structure => {
                let Some(t) = self.tabs.get(self.active) else {
                    return Vec::new();
                };
                index::rank_by(q, t.syms.decls.iter(), |d| &d.name, 200)
                    .into_iter()
                    .map(|(score, d)| Hit {
                        name: match &d.container {
                            Some(c) => format!("{c}.{}", d.name),
                            None => d.name.clone(),
                        },
                        detail: index::describe(&t.syms, d),
                        path: t.path.clone(),
                        offset: d.offset,
                        score,
                    })
                    .collect()
            }
            SearchKind::FindInFiles | SearchKind::GotoLine => Vec::new(),
        }
    }

    // ---------- panels ----------

    fn top_bar(&mut self, ui: &mut Ui) {
        let open_search = |kind| Search {
            kind,
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            stale: true,
        };
        ui.horizontal(|ui| {
            let name = self
                .project
                .as_ref()
                .map_or("Open project…".to_string(), |p| p.name.clone());
            if ui
                .button(RichText::new(format!("{}  {name}", ic::FOLDER_OPEN)).strong())
                .on_hover_text("Open an Android project folder")
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("Open an Android project folder")
                    .pick_folder()
            {
                self.open_project(&dir, &ui.ctx().clone());
            }
            if let Some(b) = &self.branch {
                ui.label(RichText::new(format!("{}  {b}", ic::GIT_BRANCH)).color(DARK.weak));
            }
            ui.separator();
            if ui
                .add_enabled(!self.history.is_empty(), egui::Button::new(ic::ARROW_LEFT))
                .on_hover_text("Back  ⌘[")
                .clicked()
            {
                self.back();
            }
            if ui
                .add_enabled(!self.forward.is_empty(), egui::Button::new(ic::ARROW_RIGHT))
                .on_hover_text("Forward  ⌘]")
                .clicked()
            {
                self.fwd();
            }
            if ui
                .button(format!("{}  Search", ic::MAGNIFYING_GLASS))
                .on_hover_text("Go to class  ⌘O\nGo to file  ⇧⌘O\nSearch everywhere  ⇧⇧")
                .clicked()
            {
                self.search = Some(open_search(SearchKind::Everywhere));
            }
            self.refactor_menu(ui);
            self.help_menu(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = self.gradle.is_some();
                if running {
                    if ui
                        .button(
                            RichText::new(format!("{}  Stop", ic::STOP))
                                .color(crate::highlight::ERROR),
                        )
                        .clicked()
                        && let Some(r) = self.gradle.as_mut()
                    {
                        r.stop();
                    }
                    ui.spinner();
                } else if ui
                    .add_enabled(
                        self.project.is_some(),
                        egui::Button::new(
                            RichText::new(format!("{}  Run", ic::PLAY))
                                .color(Color32::from_rgb(0x5F, 0xB8, 0x65)),
                        ),
                    )
                    .on_hover_text("Run the Gradle task")
                    .clicked()
                {
                    let t = self.gradle_task.clone();
                    self.run_gradle(t, &ui.ctx().clone());
                }
                ui.menu_button("Tasks", |ui| {
                    for t in [
                        "assembleDebug",
                        "installDebug",
                        "testDebugUnitTest",
                        "lint",
                        "clean",
                        "stabilityDump",
                    ] {
                        if ui.selectable_label(self.gradle_task == t, t).clicked() {
                            self.gradle_task = t.into();
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Common Gradle tasks");
                ui.add(
                    TextEdit::singleline(&mut self.gradle_task)
                        .desired_width(200.0)
                        .hint_text("Gradle task"),
                );
                ui.label(RichText::new(ic::WRENCH).color(DARK.weak));
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if self.indexing.is_some() {
                ui.spinner();
                ui.label(RichText::new("Indexing…").color(DARK.weak));
            } else {
                ui.label(format!("{} symbols", self.index.symbol_count()));
            }
            ui.separator();
            match &self.lsp {
                Some(l) if l.ready => {
                    ui.label(RichText::new(format!("{} ready", l.name)).color(Color32::LIGHT_GREEN))
                }
                Some(l) => ui.label(format!("{} starting…", l.name)),
                None => ui
                    .label(
                        RichText::new("No Kotlin LSP: brew install JetBrains/utils/kotlin-lsp")
                            .weak(),
                    )
                    .on_hover_text("Without it, completion and go-to use the built-in index only."),
            };
            ui.separator();
            if let Some(t) = self.tabs.get(self.active) {
                let line = t.text[..t.cursor.min(t.text.len())].matches('\n').count() + 1;
                let col = t.cursor
                    - t.text[..t.cursor.min(t.text.len())]
                        .rfind('\n')
                        .map_or(0, |i| i + 1)
                    + 1;
                ui.label(format!("{line}:{col}"));
                ui.separator();
            }
            if let Some((report, at)) = &self.slow
                && at.elapsed().as_secs() < 15
            {
                ui.label(RichText::new(format!("{}  {report}", ic::WARNING)).color(WARN))
                    .on_hover_text(
                        "The window was blocked this long. The same line is printed to stderr.",
                    );
                ui.separator();
            }
            ui.label(&self.status);
        });
    }

    fn project_panel(&mut self, ui: &mut Ui) {
        theme::section(ui, ic::TREE_STRUCTURE, "Project");
        if self.project.is_none() {
            ui.label("Open an Android project folder (the one with settings.gradle).");
            return;
        }
        let mut open = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (module, node) in &self.tree {
                    let label = if module == ":" {
                        self.project
                            .as_ref()
                            .map_or(module.clone(), |p| format!("{} (root)", p.name))
                    } else {
                        module.clone()
                    };
                    egui::CollapsingHeader::new(
                        RichText::new(format!("{}  {label}", ic::PACKAGE)).strong(),
                    )
                    .id_salt(module)
                    .show(ui, |ui| {
                        dir_ui(ui, node, module, &mut open);
                    });
                }
            });
        if let Some(f) = open {
            self.open_file(&f, None);
        }
    }

    fn editor(&mut self, ui: &mut Ui) {
        if self.tabs.is_empty() {
            ui.centered_and_justified(|ui| {
                let km = self.keymap;
                let line = [
                    Action::GotoClass,
                    Action::GotoFile,
                    Action::Everywhere,
                    Action::Recent,
                    Action::GotoDefinition,
                    Action::References,
                    Action::Shortcuts,
                ]
                .iter()
                .map(|a| format!("{}  {}", a.label(), keymap::text(km, *a)))
                .collect::<Vec<_>>()
                .join("\n");
                ui.label(RichText::new(line).color(DARK.weak));
            });
            return;
        }
        let mut close = None;
        egui::ScrollArea::horizontal()
            .id_salt("tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (i, t) in self.tabs.iter().enumerate() {
                        let (clicked, closed) = tab_widget(ui, t, i == self.active);
                        if clicked {
                            self.active = i;
                            self.completion = None;
                        }
                        if closed {
                            close = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = close {
            self.close_tab(i);
            return;
        }
        self.disk_banner(ui);
        if self.tabs[self.active].dirty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Unsaved changes").color(DARK.weak).small());
                if ui
                    .small_button(format!("{}  Save  ⌘S", ic::FLOPPY_DISK))
                    .clicked()
                {
                    self.save();
                }
                if ui
                    .small_button(format!("{}  Revert", ic::ARROW_COUNTER_CLOCKWISE))
                    .clicked()
                {
                    self.revert();
                }
            });
        }
        ui.separator();

        // keys the completion popup owns
        let (mut accept, mut esc) = (false, false);
        if let Some(c) = self.completion.as_mut() {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                c.selected = (c.selected + 1).min(c.items.len().saturating_sub(1));
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                c.selected = c.selected.saturating_sub(1);
            }
            accept = ui.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Enter)
                    || i.consume_key(Modifiers::NONE, Key::Tab)
            });
            esc = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        }
        if esc {
            self.completion = None;
        }
        if accept {
            self.accept_completion();
        }

        let font = FontId::monospace(FONT_SIZE);
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let tab = &mut self.tabs[self.active];
        let id = egui::Id::unique(("editor", &tab.path));
        let mut marks: Vec<(std::ops::Range<usize>, Color32)> = tab
            .lints
            .iter()
            .map(|l| {
                (
                    l.offset..l.offset + l.len,
                    if l.severity == Severity::Warning {
                        WARN
                    } else {
                        INFO
                    },
                )
            })
            .collect();
        marks.extend(tab.diagnostics.iter().map(|(o, s, _)| {
            (
                *o..*o + word_bounds(&tab.text, *o).1.saturating_sub(*o).max(1),
                if *s == 1 { ERROR } else { WARN },
            )
        }));
        let lines = tab.text.matches('\n').count() + 1;
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let gutter_w = char_w * lines.to_string().len().max(3) as f32 + 18.0;
        let blame = tab.git.blame.clone();
        let show_column = self.git.show_column;
        let mut open_commit: Option<String> = None;
        let Tab {
            text,
            cache,
            recolor,
            goto,
            cursor,
            selection,
            lints,
            diagnostics,
            ..
        } = tab;
        let marks_key = marks.iter().fold(0x9e37u64, |h, (r, c)| {
            h.rotate_left(7)
                ^ (r.start as u64)
                ^ ((r.end as u64) << 20)
                ^ u64::from(c.to_array()[0])
        });
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
            let s = buf.as_str();
            let key = hash(s) ^ marks_key ^ u64::from(ui.ctx().pixels_per_point().to_bits());
            match cache.as_mut() {
                Some(c) if c.key == key && !*recolor => {}
                // typing: patch the colours at the edit, re-colour fully once analysis settles
                Some(c) if c.job.text != s && !*recolor => {
                    highlight::patch(&mut c.job, s);
                    c.galley = ui.fonts_mut(|f| f.layout_job(c.job.clone()));
                    c.key = key;
                }
                _ => {
                    let job = highlight::job(s, font.clone(), &marks);
                    let galley = ui.fonts_mut(|f| f.layout_job(job.clone()));
                    *cache = Some(EditorCache { key, job, galley });
                    *recolor = false;
                }
            }
            cache
                .as_ref()
                .map(|c| c.galley.clone())
                .unwrap_or_else(|| ui.fonts_mut(|f| f.layout_job(Default::default())))
        };
        let mut changed = false;
        let mut cmd_click = false;
        let mut popup_pos = None;
        egui::ScrollArea::both()
            .id_salt(id)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let blame_rect = show_column.then(|| {
                        ui.allocate_exact_size(
                            egui::vec2(git_ui::COLUMN_WIDTH, row_h * lines as f32),
                            egui::Sense::hover(),
                        )
                        .0
                    });
                    let (gutter, _) = ui.allocate_exact_size(
                        egui::vec2(gutter_w, row_h * lines as f32),
                        egui::Sense::hover(),
                    );
                    // filled in after layout, so it sits under the text
                    let line_bg = ui.painter().add(egui::Shape::Noop);
                    let out = TextEdit::multiline(text)
                        .id(id)
                        .code_editor()
                        .lock_focus(true)
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(f32::INFINITY)
                        .desired_rows(lines.max(30))
                        .layouter(&mut layouter)
                        .show(ui);
                    if let Some(target) = goto.take() {
                        let ci = text[..target.min(text.len())].chars().count();
                        let mut st = out.state.clone();
                        st.cursor
                            .set_char_range(Some(CCursorRange::one(CCursor::new(ci))));
                        st.store(ui.ctx(), id);
                        out.response.request_focus();
                        let r = out
                            .galley
                            .pos_from_cursor(CCursor::new(ci))
                            .translate(out.galley_pos.to_vec2());
                        ui.scroll_to_rect(
                            r.expand2(egui::vec2(0.0, row_h * 6.0)),
                            Some(Align::Center),
                        );
                        *cursor = target.min(text.len());
                        *selection = *cursor..*cursor;
                    } else if let Some(r) = out.cursor_range {
                        *cursor = byte_of_char(text, r.primary.index.0);
                        let other = byte_of_char(text, r.secondary.index.0);
                        *selection = (*cursor).min(other)..(*cursor).max(other);
                        popup_pos = Some(
                            out.galley
                                .pos_from_cursor(r.primary)
                                .translate(out.galley_pos.to_vec2())
                                .left_bottom(),
                        );
                    }
                    let cursor_line = text[..(*cursor).min(text.len())].matches('\n').count();
                    let theme = &crate::theme::DARK;
                    if let Some(row) = out.galley.rows.get(cursor_line) {
                        let y = out.galley_pos.y + row.pos.y;
                        let r = egui::Rect::from_min_max(
                            egui::pos2(gutter.left(), y),
                            egui::pos2(ui.clip_rect().right(), y + row_h),
                        );
                        ui.painter().set(
                            line_bg,
                            egui::Shape::rect_filled(r, 0.0, theme.current_line),
                        );
                    }
                    paint_line_numbers(
                        ui,
                        &out.galley,
                        out.galley_pos,
                        gutter,
                        cursor_line,
                        &font,
                        theme,
                    );
                    if let Some(rect) = blame_rect
                        && let Some(sha) = git_ui::blame_column(
                            ui,
                            blame.as_deref(),
                            rect,
                            &out.galley,
                            out.galley_pos,
                            row_h,
                        )
                    {
                        open_commit = Some(sha);
                    }
                    if let Some(b) = &blame {
                        let line = cursor_line;
                        if let Some(sha) =
                            git_ui::paint_inline_blame(ui, b, &out.galley, out.galley_pos, line)
                        {
                            open_commit = Some(sha);
                        }
                    }
                    changed = out.response.changed();
                    cmd_click = out.response.clicked() && ui.input(|i| i.modifiers.command);
                    // hover a problem to read it
                    if let Some(pos) = out.response.hover_pos() {
                        let c = out.galley.cursor_from_pos(pos - out.galley_pos);
                        let b = byte_of_char(text, c.index.0);
                        let msgs: Vec<String> = lints
                            .iter()
                            .filter(|l| (l.offset..l.offset + l.len).contains(&b))
                            .map(|l| format!("[{}] {}", l.rule, l.message))
                            .chain(
                                diagnostics
                                    .iter()
                                    .filter(|(o, _, _)| {
                                        (*o..word_bounds(text, *o).1.max(o + 1)).contains(&b)
                                    })
                                    .map(|(_, _, m)| m.clone()),
                            )
                            .collect();
                        if !msgs.is_empty() {
                            out.response.response.clone().on_hover_text(msgs.join("\n"));
                        }
                    }
                });
            });
        let path = tab.path.clone();
        if changed {
            // analysis, the index and kotlin-lsp catch up after a typing pause (see `settle`)
            tab.rev += 1;
            tab.dirty_since = Some(Instant::now());
            self.update_completion(false);
        }
        if cmd_click {
            self.goto_definition();
        }
        if let Some(sha) = open_commit {
            self.open_commit(&sha, &path, &ui.ctx().clone());
        }
        self.completion_popup(ui, popup_pos);
    }

    fn update_completion(&mut self, forced: bool) {
        let t = &self.tabs[self.active];
        let (start, _) = word_bounds(&t.text, t.cursor);
        let prefix = &t.text[start..t.cursor.min(t.text.len())];
        let after_dot = t.text[..start].ends_with('.');
        if prefix.is_empty() && !after_dot && !forced {
            self.completion = None;
            return;
        }
        if !forced && !after_dot && prefix.len() < 2 {
            self.completion = None;
            return;
        }
        let mut items: Vec<Item> = Vec::new();
        if !after_dot {
            let imported: Vec<&str> = t.syms.imports.iter().map(|s| s.as_str()).collect();
            for h in self.index.complete(prefix, 40) {
                let fq = self.index.import_for(&h.name);
                let pkg = fq
                    .as_deref()
                    .and_then(|f| f.rsplit_once('.'))
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                let needs = fq.clone().filter(|f| {
                    !imported.contains(&f.as_str())
                        && pkg != t.syms.package
                        && !pkg.is_empty()
                        && !imported
                            .iter()
                            .any(|i| i.ends_with(".*") && pkg == i.trim_end_matches(".*"))
                });
                items.push(Item {
                    label: h.name.clone(),
                    detail: h.detail,
                    insert: h.name,
                    import: needs,
                });
            }
            for k in crate::lexer::KEYWORDS
                .iter()
                .filter(|k| k.starts_with(prefix) && **k != prefix)
            {
                items.push(Item {
                    label: k.to_string(),
                    detail: "keyword".into(),
                    insert: k.to_string(),
                    import: None,
                });
            }
        }
        if self.lsp.as_ref().is_some_and(|l| l.ready)
            && self.tabs[self.active].dirty_since.is_some()
        {
            self.settle(true);
        }
        let t = &self.tabs[self.active];
        if let Some(l) = self.lsp.as_mut().filter(|l| l.ready) {
            let (line, col) = lsp::position(&t.text, t.cursor);
            l.completion(&t.path, line, col, start);
        }
        if items.is_empty() && self.lsp.is_none() {
            self.completion = None;
            return;
        }
        self.completion = Some(Completion {
            start,
            items,
            selected: 0,
        });
    }

    fn accept_completion(&mut self) {
        self.settle(true);
        let Some(c) = self.completion.take() else {
            return;
        };
        let Some(item) = c.items.get(c.selected).cloned() else {
            return;
        };
        let t = &mut self.tabs[self.active];
        let end = word_bounds(&t.text, t.cursor).1.max(t.cursor);
        t.text.replace_range(c.start..end, &item.insert);
        let mut cursor = c.start + item.insert.len();
        if let Some(fq) = item.import {
            let at = import_position(&t.text);
            let line = format!("import {fq}\n");
            t.text.insert_str(at, &line);
            if at <= cursor {
                cursor += line.len();
            }
        }
        t.goto = Some(cursor);
        t.reanalyse();
        let path = t.path.clone();
        self.index.update(&path, &self.tabs[self.active].text);
        if let Some(l) = self.lsp.as_mut() {
            l.did_change(&path, &self.tabs[self.active].text);
        }
    }

    fn completion_popup(&mut self, ui: &mut Ui, pos: Option<egui::Pos2>) {
        let Some(c) = &self.completion else { return };
        if c.items.is_empty() {
            return;
        }
        let Some(pos) = pos else { return };
        let mut pick = None;
        egui::Area::new(egui::Id::unique("completion"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(520.0);
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for (i, it) in c.items.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    let r = ui.selectable_label(
                                        i == c.selected,
                                        RichText::new(&it.label).monospace(),
                                    );
                                    if i == c.selected {
                                        r.scroll_to_me(None);
                                    }
                                    if r.clicked() {
                                        pick = Some(i);
                                    }
                                    let mut d = it.detail.clone();
                                    if it.import.is_some() {
                                        d.push_str("  (+import)");
                                    }
                                    ui.label(RichText::new(d).weak().small());
                                });
                            }
                        });
                });
            });
        if let Some(i) = pick {
            if let Some(c) = self.completion.as_mut() {
                c.selected = i;
            }
            self.accept_completion();
        }
    }

    // ---------- Compose panel ----------

    fn compose_panel(&mut self, ui: &mut Ui) {
        theme::section(ui, ic::CUBE, "Compose");
        let Some(tab) = self.tabs.get(self.active) else {
            ui.label("Open a Kotlin file to see its composables.");
            return;
        };
        let composables: Vec<&Decl> = tab
            .syms
            .decls
            .iter()
            .filter(|d| d.is_composable())
            .collect();
        if composables.is_empty() {
            ui.label("No @Composable functions in this file.");
            return;
        }
        // the composable under the cursor, unless one was picked in the list
        let at_cursor = composables
            .iter()
            .find(|d| d.body.as_ref().is_some_and(|b| b.contains(&tab.cursor)))
            .map(|d| d.name.clone());
        let current = at_cursor
            .or(self.selected_composable.clone())
            .filter(|n| composables.iter().any(|d| &d.name == n))
            .unwrap_or_else(|| composables[0].name.clone());
        let mut jump: Option<usize> = None;
        let mut pick = None;
        let mut render = false;
        let mut dump = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::CollapsingHeader::new(RichText::new(format!("{}  Composables ({})", ic::CUBE, composables.len())).strong()).default_open(composables.len() <= 8).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("composables").max_height(220.0).show(ui, |ui| {
                    for d in &composables {
                        let mut label = RichText::new(&d.name).monospace();
                        if d.is_preview() {
                            label = label.italics();
                        }
                        if ui.selectable_label(d.name == current, label).clicked() {
                            pick = Some(d.name.clone());
                            jump = Some(d.offset);
                        }
                    }
                });
            });
            let Some(d) = composables.iter().find(|d| d.name == current) else { return };
            let key = (tab.path.clone(), tab.rev, self.index.generation, self.outputs_gen, d.name.clone());
            if self.compose_view.as_ref().is_none_or(|v| v.key != key) {
                let is_ui = |n: &str| compose::is_ui_call(n, &self.index);
                let (stability, guessed) = match self.stability.get(&d.name) {
                    Some(s) => (s.clone(), false),
                    None => (compose::guess(d, &self.index), true),
                };
                let previews: Vec<String> = tab.syms.decls.iter().filter(|d| d.is_preview()).map(|d| d.name.clone()).collect();
                let tool = self.project.as_ref().and_then(|pr| pr.module_of(&tab.path)).and_then(|m| PreviewTool::detect(m).map(|t| (m.clone(), t)));
                let images = tool.as_ref().map(|(m, t)| t.images(m, &previews)).unwrap_or_default();
                self.compose_view = Some(ComposeView { key, tree: compose::tree(&tab.text, d, &is_ui), stability, guessed, tool, images });
            }
            let Some(view) = &self.compose_view else { return };
            ui.separator();
            theme::section(ui, ic::TREE_STRUCTURE, &format!("UI tree · {}", d.name));
            if view.tree.is_empty() {
                ui.label(RichText::new("No UI calls found.").weak());
            }
            for n in &view.tree {
                node_ui(ui, n, &mut jump);
            }

            ui.separator();
            theme::section(ui, ic::LIGHTNING, "Stability");
            let (st, guessed) = (&view.stability, view.guessed);
            ui.horizontal(|ui| {
                let yes_no = |b: Option<bool>| b.map_or("?", |b| if b { "yes" } else { "no" });
                ui.label(format!("skippable: {}   restartable: {}", yes_no(st.skippable), yes_no(st.restartable)));
            });
            ui.label(RichText::new(if guessed { "Source: local guess. Run stabilityDump (skydoves plugin) or a build with Compose compiler reports for exact results.".to_string() } else { format!("Source: {}", st.source) }).weak().small());
            egui::Grid::new("stability").striped(true).show(ui, |ui| {
                for p in &st.params {
                    let (txt, color) = match p.verdict {
                        Verdict::Stable => ("STABLE", Color32::LIGHT_GREEN),
                        Verdict::Unstable => ("UNSTABLE", ERROR),
                        Verdict::Runtime => ("RUNTIME", WARN),
                        Verdict::Unknown => ("UNKNOWN", Color32::GRAY),
                    };
                    ui.label(RichText::new(&p.name).monospace());
                    ui.label(RichText::new(txt).color(color).strong());
                    ui.label(RichText::new(&p.reason).small());
                    ui.end_row();
                }
            });
            if ui.button("Run stabilityDump").on_hover_text("Needs the com.github.skydoves.compose.stability.analyzer plugin in the module").clicked() {
                dump = true;
            }

            ui.separator();
            theme::section(ui, ic::IMAGE, "Previews");
            let previews: Vec<String> = tab.syms.decls.iter().filter(|d| d.is_preview()).map(|d| d.name.clone()).collect();
            if previews.is_empty() {
                ui.label(RichText::new("No @Preview functions in this file.").weak());
            }
            for p in &previews {
                if ui.link(RichText::new(p).monospace()).clicked() {
                    jump = tab.syms.decls.iter().find(|d| &d.name == p).map(|d| d.offset);
                }
            }
            match &view.tool {
                Some((m, tool)) => {
                    ui.horizontal(|ui| {
                        render = ui.add_enabled(self.gradle.is_none(), egui::Button::new(format!("{}  Render previews", ic::IMAGE))).on_hover_text(tool.task(m)).clicked();
                        ui.label(RichText::new(format!("{tool:?}")).weak());
                    });
                    // drop textures of images no longer shown, so memory stays flat
                    self.textures.retain(|p, _| view.images.iter().any(|(_, q)| q == p));
                    for (name, path) in &view.images {
                        ui.label(RichText::new(name).small());
                        if let Some(tex) = texture(ui.ctx(), &mut self.textures, path) {
                            let size = tex.size_vec2();
                            let w = ui.available_width().min(size.x);
                            ui.image((tex.id(), egui::vec2(w, size.y * w / size.x)));
                        }
                    }
                }
                None if !previews.is_empty() => {
                    ui.label(RichText::new(gradle::SETUP_HELP).small());
                }
                _ => {}
            }

            ui.separator();
            theme::section(ui, ic::CHECK_CIRCLE, "Compose checks");
            for l in tab.lints.iter() {
                let line = tab.text[..l.offset].matches('\n').count() + 1;
                let c = if l.severity == Severity::Warning { WARN } else { INFO };
                if ui.link(RichText::new(format!("{line}: [{}] {}", l.rule, l.message)).color(c).small()).clicked() {
                    jump = Some(l.offset);
                }
            }
        });
        let path = tab.path.clone();
        if let Some(n) = pick {
            self.selected_composable = Some(n);
        }
        if let Some(o) = jump {
            self.open_file(&path, Some(o));
        }
        let task_module = self
            .project
            .as_ref()
            .and_then(|p| p.module_of(&path))
            .cloned();
        if render
            && let Some(m) = &task_module
            && let Some(tool) = PreviewTool::detect(m)
        {
            let ctx = ui.ctx().clone();
            self.run_gradle(tool.task(m), &ctx);
        }
        if dump && let Some(m) = &task_module {
            let ctx = ui.ctx().clone();
            let prefix = if m.name == ":" {
                String::new()
            } else {
                m.name.clone()
            };
            self.run_gradle(format!("{prefix}:stabilityDump"), &ctx);
        }
    }

    fn bottom_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let problems = self
                .tabs
                .get(self.active)
                .map_or(0, |t| t.lints.len() + t.diagnostics.len());
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Problems,
                format!("{}  Problems  {problems}", ic::WARNING),
            );
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Build,
                if self.gradle.is_some() {
                    format!("{}  Building…", ic::CLOCK_COUNTER_CLOCKWISE)
                } else {
                    format!("{}  Build", ic::STACK)
                },
            );
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Find,
                format!("{}  Find  {}", ic::MAGNIFYING_GLASS, self.found.len()),
            );
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Git,
                format!("{}  Git", ic::GIT_COMMIT),
            );
            if self.bottom == Bottom::Build
                && ui.small_button("Reload stability").clicked()
                && let Some(p) = self.project.take()
            {
                self.reload_stability(&p, &ui.ctx().clone());
                self.project = Some(p);
            }
        });
        ui.separator();
        let mut open: Option<(PathBuf, usize)> = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
        match self.bottom {
            Bottom::Problems => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let Some(t) = self.tabs.get(self.active) else {
                            return;
                        };
                        let line_of =
                            |o: usize| t.text[..o.min(t.text.len())].matches('\n').count() + 1;
                        let rows = t
                            .diagnostics
                            .iter()
                            .map(|(o, s, m)| {
                                (
                                    *o,
                                    if *s == 1 {
                                        (ic::X, ERROR)
                                    } else {
                                        (ic::WARNING, WARN)
                                    },
                                    String::new(),
                                    m.clone(),
                                )
                            })
                            .chain(t.lints.iter().map(|l| {
                                (
                                    l.offset,
                                    if l.severity == Severity::Warning {
                                        (ic::WARNING, WARN)
                                    } else {
                                        (ic::LIGHTNING, INFO)
                                    },
                                    l.rule.to_string(),
                                    l.message.clone(),
                                )
                            }));
                        if t.diagnostics.is_empty() && t.lints.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{}  No problems in this file",
                                    ic::CHECK_CIRCLE
                                ))
                                .color(DARK.weak),
                            );
                        }
                        for (o, (icon, color), rule, msg) in rows {
                            let r = ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).color(color));
                                let r = ui.link(RichText::new(&msg).color(DARK.text));
                                if !rule.is_empty() {
                                    ui.label(RichText::new(&rule).color(DARK.weak).small());
                                }
                                ui.label(
                                    RichText::new(format!("line {}", line_of(o)))
                                        .color(DARK.weak)
                                        .small(),
                                );
                                r
                            });
                            if r.inner.clicked() {
                                open = Some((t.path.clone(), o));
                            }
                        }
                    });
            }
            Bottom::Build => {
                // only visible rows are laid out, so a 20 000-line log stays fast
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_rows(ui, row_h, self.console.len(), |ui, range| {
                        for line in &self.console[range] {
                            match compiler_location(line) {
                                Some((p, l, c)) => {
                                    let color = if line.starts_with("e:") { ERROR } else { WARN };
                                    if ui
                                        .link(RichText::new(line).monospace().color(color))
                                        .clicked()
                                        && let Ok(text) = std::fs::read_to_string(&p)
                                    {
                                        open = Some((
                                            p,
                                            lsp::offset(
                                                &text,
                                                l.saturating_sub(1),
                                                c.saturating_sub(1),
                                            ),
                                        ));
                                    }
                                }
                                None => {
                                    ui.label(RichText::new(line).monospace().color(TEXT));
                                }
                            }
                        }
                    });
            }
            Bottom::Git => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.git_panel(ui));
            }
            Bottom::Find => {
                let root = self
                    .project
                    .as_ref()
                    .map(|p| p.root.clone())
                    .unwrap_or_default();
                if !self.found_title.is_empty() {
                    let mut files: Vec<&Path> =
                        self.found.iter().map(|f| f.path.as_path()).collect();
                    files.dedup();
                    let busy = if self.find_rx.is_some() {
                        "  searching…"
                    } else {
                        ""
                    };
                    ui.label(
                        RichText::new(format!(
                            "{}: {} in {} files{busy}",
                            self.found_title,
                            self.found.len(),
                            files.len()
                        ))
                        .strong(),
                    );
                }
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, self.found.len(), |ui, range| {
                        for f in &self.found[range] {
                            let rel = f
                                .path
                                .strip_prefix(&root)
                                .unwrap_or(&f.path)
                                .display()
                                .to_string();
                            let r = ui.horizontal(|ui| {
                                let r =
                                    ui.link(RichText::new(&f.text).monospace().color(DARK.text));
                                ui.label(
                                    RichText::new(format!("{rel}:{}", f.line))
                                        .color(DARK.weak)
                                        .small(),
                                );
                                r
                            });
                            if r.inner.clicked() {
                                open = Some((f.path.clone(), f.offset));
                            }
                        }
                    });
            }
        }
        if let Some((p, o)) = open {
            self.open_file(&p, Some(o));
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.glass {
            [0.0; 4]
        } else {
            visuals.panel_fill.to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut watch = Watchdog::start();
        watch.stage("background results", || self.poll(&ctx));
        watch.stage("disk changes", || self.poll_disk(&ctx));
        watch.stage("analysis", || self.settle(false));
        watch.stage("git blame", || self.poll_git(&ctx));
        watch.stage("shortcuts", || self.shortcuts(&ctx));
        if let Some(dropped) =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
            && dropped.is_dir()
        {
            self.open_project(&dropped, &ctx);
        }
        watch.stage("toolbar", || {
            egui::Panel::top("top").show(ui, |ui| {
                if self.fullsize {
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    if self.fullsize {
                        ui.add_space(72.0); // traffic lights
                    }
                    self.top_bar(ui);
                });
            })
        });
        watch.stage("status bar", || {
            egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui))
        });
        watch.stage("bottom panel", || {
            egui::Panel::bottom("bottom")
                .resizable(true)
                .default_size(180.0)
                .show(ui, |ui| self.bottom_panel(ui))
        });
        watch.stage("project tree", || {
            egui::Panel::left("project")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| self.project_panel(ui))
        });
        watch.stage("compose panel", || {
            egui::Panel::right("compose")
                .resizable(true)
                .default_size(340.0)
                .show(ui, |ui| self.compose_panel(ui))
        });
        let editor_frame =
            egui::Frame::central_panel(ui.style()).fill(crate::glass::editor_fill(self.glass));
        watch.stage("editor", || {
            egui::CentralPanel::default()
                .frame(editor_frame)
                .show(ui, |ui| self.editor(ui))
        });
        watch.stage("popups", || {
            self.search_window(&ctx);
            self.refactor_window(&ctx);
            self.commit_window(&ctx);
            self.shortcuts_window(&ctx);
        });
        self.frames += 1;
        if let Some(report) = watch.finish(self.frames) {
            eprintln!("droid-lite: {report}");
            self.slow = Some((report, Instant::now()));
        }
        if let Some((path, frames)) = self.shot.as_mut().filter(|_| self.indexing.is_none()) {
            *frames += 1;
            let wait: u32 = std::env::var("DROID_LITE_SHOT_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            if *frames == wait {
                let path = path.clone();
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::ScreenshotCallback::new(move |img| {
                        let buf = image::RgbaImage::from_raw(
                            img.size[0] as u32,
                            img.size[1] as u32,
                            img.as_raw().to_vec(),
                        );
                        if let Some(b) = buf {
                            let _ = b.save(&path);
                        }
                        std::process::exit(0);
                    }),
                ));
            }
            ctx.request_repaint();
        }
    }
}

// ---------- helpers ----------

/// Paints the numbers of the visible rows only, right-aligned; the cursor's line is brighter.
fn paint_line_numbers(
    ui: &Ui,
    galley: &egui::Galley,
    galley_pos: egui::Pos2,
    gutter: egui::Rect,
    current: usize,
    font: &FontId,
    theme: &crate::theme::Theme,
) {
    let clip = ui.clip_rect();
    let painter = ui.painter();
    for (i, row) in galley.rows.iter().enumerate() {
        let y = galley_pos.y + row.pos.y;
        if y > clip.bottom() {
            break;
        }
        if y + row.rect().height() < clip.top() {
            continue;
        }
        let color = if i == current {
            theme.text
        } else {
            theme.gutter
        };
        painter.text(
            egui::pos2(gutter.right() - 12.0, y),
            Align2::RIGHT_TOP,
            (i + 1).to_string(),
            font.clone(),
            color,
        );
    }
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn byte_of_char(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map_or(s.len(), |(b, _)| b)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte range of the identifier around `at`.
pub fn word_bounds(s: &str, at: usize) -> (usize, usize) {
    let at = at.min(s.len());
    let a = s[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(at, |(i, _)| i);
    let b = s[at..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(s.len(), |(i, _)| at + i);
    (a, b)
}

/// Where a new import line goes: after the last import, else after the package line, else the top.
fn import_position(s: &str) -> usize {
    let mut at = 0;
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("import ") || (at == 0 && t.starts_with("package ")) {
            at = pos + line.len();
            if t.starts_with("package ") && !s[at..].starts_with("import") {
                // keep one blank line after the package
                if s[at..].starts_with('\n') {
                    at += 1;
                }
            }
        }
        pos += line.len();
    }
    at
}

/// `e: file:///path/File.kt:12:5 message` from the Kotlin compiler.
fn compiler_location(line: &str) -> Option<(PathBuf, u32, u32)> {
    if !(line.starts_with("e:") || line.starts_with("w:")) {
        return None;
    }
    let rest = line.split("file://").nth(1)?;
    let mut parts = rest.splitn(4, ':');
    let p = parts.next()?;
    let l = parts.next()?.trim().parse().ok()?;
    let c = parts.next()?.split_whitespace().next()?.parse().ok()?;
    Some((PathBuf::from(p), l, c))
}

fn texture(
    ctx: &egui::Context,
    cache: &mut HashMap<PathBuf, (SystemTime, egui::TextureHandle)>,
    path: &Path,
) -> Option<egui::TextureHandle> {
    let mtime = path.metadata().and_then(|m| m.modified()).ok()?;
    if let Some((t, h)) = cache.get(path)
        && *t == mtime
    {
        return Some(h.clone());
    }
    let img = image::open(path).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    let h = ctx.load_texture(path.to_string_lossy(), color, Default::default());
    cache.insert(path.to_path_buf(), (mtime, h.clone()));
    Some(h)
}

fn node_ui(ui: &mut Ui, n: &compose::Node, jump: &mut Option<usize>) {
    if n.children.is_empty() {
        if ui.link(RichText::new(&n.name).monospace()).clicked() {
            *jump = Some(n.offset);
        }
    } else {
        let r = egui::CollapsingHeader::new(RichText::new(&n.name).monospace())
            .id_salt(n.offset)
            .default_open(true)
            .show(ui, |ui| {
                for c in &n.children {
                    node_ui(ui, c, jump);
                }
            });
        if r.header_response.double_clicked() || r.header_response.secondary_clicked() {
            *jump = Some(n.offset);
        }
    }
}

/// Module → directory tree. Files that belong to a nested module are left out of the parent.
fn build_tree(p: &Project) -> Vec<(String, DirNode)> {
    let mut out: Vec<(String, DirNode)> = p
        .modules
        .iter()
        .map(|m| (m.name.clone(), DirNode::default()))
        .collect();
    for f in &p.files {
        let Some(m) = p.module_of(f) else { continue };
        let Some(slot) = out.iter_mut().find(|(n, _)| *n == m.name) else {
            continue;
        };
        let rel = f.strip_prefix(&m.dir).unwrap_or(f);
        let mut node = &mut slot.1;
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        for c in &comps[..comps.len().saturating_sub(1)] {
            node = node.dirs.entry(c.clone()).or_default();
        }
        node.files.push(f.clone());
    }
    out
}

fn dir_ui(ui: &mut Ui, node: &DirNode, id: &str, open: &mut Option<PathBuf>) {
    for (name, child) in &node.dirs {
        // compact single-child chains, like Android Studio's package view: src/main/java/com/app
        let mut label = name.clone();
        let mut c = child;
        while c.files.is_empty() && c.dirs.len() == 1 {
            let (n, next) = c.dirs.iter().next().unwrap();
            label = format!("{label}/{n}");
            c = next;
        }
        let key = format!("{id}/{label}");
        egui::CollapsingHeader::new(format!("{}  {label}", ic::FOLDER_SIMPLE))
            .id_salt(&key)
            .show(ui, |ui| dir_ui(ui, c, &key, open));
    }
    for f in &node.files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (icon, color) = file_icon(f);
        let mut job = egui::text::LayoutJob::default();
        let fmt = |color| egui::TextFormat {
            font_id: FontId::proportional(13.0),
            color,
            ..Default::default()
        };
        job.append(icon, 0.0, fmt(color));
        job.append(&format!("  {name}"), 0.0, fmt(DARK.text));
        if ui.selectable_label(false, job).clicked() {
            *open = Some(f.clone());
        }
    }
}

/// Icon and tint for a file in the tree and on tabs.
fn file_icon(path: &Path) -> (&'static str, Color32) {
    match path.extension().and_then(|e| e.to_str()) {
        Some("kt") => (ic::FILE_CODE, Color32::from_rgb(0xA9, 0x7B, 0xFF)),
        Some("java") => (ic::FILE_CODE, Color32::from_rgb(0xE7, 0x8A, 0x4E)),
        Some("xml") => (ic::FILE_TEXT, Color32::from_rgb(0x5F, 0xB8, 0x65)),
        Some("kts" | "gradle") => (ic::WRENCH, Color32::from_rgb(0x58, 0x9D, 0xF6)),
        _ => (ic::FILE, DARK.weak),
    }
}

/// One editor tab: file icon, name (● when unsaved), close button, accent line when active.
/// Returns (clicked, close clicked). Middle-click closes too.
fn tab_widget(ui: &mut Ui, t: &Tab, active: bool) -> (bool, bool) {
    let (icon, tint) = file_icon(&t.path);
    let name = t
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fill = if active {
        DARK.editor_bg
    } else {
        Color32::TRANSPARENT
    };
    let mut close = false;
    let inner = egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius {
            nw: 6,
            ne: 6,
            sw: 0,
            se: 0,
        })
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icon).color(tint));
                let color = if active { DARK.text } else { DARK.weak };
                ui.label(
                    RichText::new(if t.dirty() {
                        format!("{name} ●")
                    } else {
                        name.clone()
                    })
                    .color(color),
                );
                close = ui
                    .add(
                        egui::Button::new(RichText::new(ic::X).size(11.0).color(DARK.weak))
                            .frame(false),
                    )
                    .on_hover_text("Close  ⌘W")
                    .clicked();
            });
        });
    let r = inner
        .response
        .interact(egui::Sense::click())
        .on_hover_text(t.path.display().to_string());
    if active {
        let rect = r.rect;
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            egui::Stroke::new(2.0, DARK.accent),
        );
    }
    (r.clicked() && !close, close || r.middle_clicked())
}

fn config_file() -> Option<PathBuf> {
    setting_file("last-project")
}

/// `~/.config/droid-lite/<name>`. Tests never read or write the user's settings.
fn setting_file(name: &str) -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/droid-lite").join(name))
}

fn load_setting(name: &str) -> Option<String> {
    std::fs::read_to_string(setting_file(name)?).ok()
}

fn save_setting(name: &str, value: &str) {
    if let Some(f) = setting_file(name) {
        let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(f, value);
    }
}

fn last_project() -> Option<PathBuf> {
    let p = PathBuf::from(std::fs::read_to_string(config_file()?).ok()?.trim());
    p.is_dir().then_some(p)
}

fn remember_project(root: &Path) {
    if let Some(f) = config_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(f, root.to_string_lossy().as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_bounds_and_imports() {
        assert_eq!(word_bounds("val fooBar = 1", 6), (4, 10));
        assert_eq!(word_bounds("a.b", 2), (2, 3));
        let s = "package a.b\n\nimport x.Y\nimport x.Z\n\nfun f() {}\n";
        assert_eq!(&s[import_position(s)..], "\nfun f() {}\n");
        let s = "package a.b\n\nfun f() {}\n";
        assert_eq!(&s[import_position(s)..], "fun f() {}\n");
        assert_eq!(import_position("fun f() {}"), 0);
    }

    #[test]
    fn parses_kotlin_compiler_locations() {
        let (p, l, c) = compiler_location(
            "e: file:///Users/me/app/src/Main.kt:12:5 Unresolved reference 'foo'.",
        )
        .unwrap();
        assert_eq!(
            (p.to_str().unwrap(), l, c),
            ("/Users/me/app/src/Main.kt", 12, 5)
        );
        assert!(compiler_location("BUILD SUCCESSFUL").is_none());
    }
}
