//! The IDE window: project tree, editor tabs, search popups, completion, Compose panel, Gradle console.

mod android;
mod assist;
mod capture_ui;
mod comfort;
mod disk;
#[cfg(test)]
mod e2e;
mod find;
mod git_ui;
mod gutter;
mod libs_ui;
mod logcat_ui;
mod perf;
mod refactor_ui;
mod run_ui;
mod tests_ui;
mod toasts;
mod translations;
mod tree;
mod typing;
mod usage_filter;

use crate::compose::{self, Lint, Severity, Stability};
use crate::edit;
use crate::gradle::{self, PreviewTool};
use crate::highlight::{self, ERROR, INFO, WARN};
use crate::icons as ic;
use crate::index::{self, Hit, Index};
use crate::keymap::{self, Action, Keymap};
use crate::kotlin::{Decl, FileSyms, scan};
use crate::lsp::{self, Lsp};
use crate::project::{Project, is_code};
use crate::theme;
use egui::text::{CCursor, CCursorRange};
use egui::{Align, Align2, Color32, FontId, Key, Modifiers, RichText, TextEdit, Ui};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Instant, SystemTime};

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
    /// A library source opened from a jar: navigate and copy, never edit or save.
    readonly: bool,
    git: git_ui::TabBlame,
    annotations: Option<gutter::Annotations>,
    /// Selection to set with the next `goto` (a find match).
    select: Option<std::ops::Range<usize>>,
    /// Cursor to set without scrolling the view (after an edit command).
    caret: Option<usize>,
    /// Matching-bracket pair for (rev, cursor).
    bracket: (u64, usize, Option<(usize, usize)>),
    /// Bumped on every text change; caches key on it.
    rev: u64,
}

impl Tab {
    fn dirty(&self) -> bool {
        self.text != self.saved
    }
    /// The text changed: caches key on `rev`; analysis follows after a typing pause.
    fn mark_edited(&mut self) {
        self.rev += 1;
        self.dirty_since = Some(Instant::now());
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

    /// The frame's total time and parts, and the report for a slow frame. The first frames
    /// (fonts, first layout) are never reported.
    fn finish(self, frame: u64) -> (f32, Vec<(&'static str, f32)>, Option<String>) {
        let total = self.start.elapsed().as_secs_f32() * 1000.0;
        let report = self.report(frame, total);
        (total, self.stages, report)
    }

    fn report(&self, frame: u64, total: f32) -> Option<String> {
        if frame <= 3 || total < perf::SLOW_MS {
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
    /// `galley` with room for the link rows, and the key of the rows it was made for.
    shown: Option<(u64, std::sync::Arc<egui::Galley>)>,
}

/// Analysis waits this long after the last keystroke, so typing in big files stays smooth.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Debug)]
enum SearchKind {
    Class,
    File,
    Symbol,
    Everywhere,
    Recent,
    Structure,
    FindInFiles,
    GotoLine,
    /// A fixed list from `Search::pinned`.
    Pick,
    /// Every command, by name (⇧⌘A).
    Actions,
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
            SearchKind::Pick => Action::Implementations,
            SearchKind::Actions => Action::Palette,
        }
    }
    fn hint(self) -> &'static str {
        match self {
            SearchKind::FindInFiles => "Text to find in every file, then Enter",
            SearchKind::GotoLine => "line or line:column, then Enter",
            SearchKind::Actions => "Type a command: rename, logcat, run app, focus…",
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
    /// For a picker (implementations, several declarations): the fixed list to filter, and its title.
    pinned: Vec<Hit>,
    title: Option<String>,
}

/// The usages dropdown: the name, where it opens, the selected row and the declaration to leave out.
struct UsagesPopup {
    word: String,
    pos: egui::Pos2,
    selected: usize,
    decl: (PathBuf, usize),
    /// The frame it opened in: that ⌘-click is not a click outside it.
    frame: u64,
    /// The results were shown once: a filter change that leaves one usage does not jump.
    settled: bool,
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
    /// A live template (`comp`, `vm`…) to expand instead of inserting `insert`.
    template: Option<&'static crate::android_kit::Template>,
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

#[derive(PartialEq, Clone, Copy, Debug)]
enum Bottom {
    Problems,
    Build,
    Find,
    Logcat,
    Tests,
    Performance,
}

/// The right panel's tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Right {
    Compose,
    PullRequest,
}

/// What the Compose panel shows for one composable; rebuilt only when its inputs change.
struct ComposeView {
    key: (PathBuf, u64, u64, u64, String),
    tree: Vec<compose::Node>,
    tool: Option<(crate::project::Module, PreviewTool)>,
    images: Vec<(String, PathBuf)>,
}

/// The ⌘-hovered name, a resource reference it may be (kind, name), and its response.
type HoverWord = (String, usize, Option<(String, String)>, egui::Response);
type HoverTarget = ((PathBuf, usize, u64, u64), Option<crate::usage::Target>);

/// A project opened on a worker thread: files, index, git branch and the tree.
type Opened = (
    Project,
    Index,
    Option<String>,
    Vec<tree::Node>,
    crate::resources::Resources,
);

pub struct App {
    project: Option<Project>,
    tree: Vec<tree::Node>,
    tree_view: tree::View,
    /// The Project panel's file filter.
    tree_filter: String,
    /// File to show in the Project tree, and the frames left to keep it scrolled into view.
    locate: Option<(PathBuf, u8)>,
    index: Index,
    indexing: Option<Receiver<Opened>>,
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
    /// kotlin-lsp references that came back before the project search ended.
    lsp_found: Option<Vec<FoundLine>>,
    /// Android Studio style usages dropdown at the caret (⌘-click on a declaration).
    usages_popup: Option<UsagesPopup>,
    logcat: logcat_ui::LogcatView,
    run: run_ui::RunState,
    capture: capture_ui::CaptureState,
    /// The caret's screen position last frame, where popups open.
    caret_pos: Option<egui::Pos2>,
    bottom: Bottom,
    right: Right,
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
    git: git_ui::GitState,
    /// Current git branch of the project, shown in the toolbar.
    branch: Option<String>,
    /// Bumped when a build or a report reload may have changed previews and stability.
    outputs_gen: u64,
    /// Android resources of the project (values, files, ids).
    res: crate::resources::Resources,
    res_rx: Option<Receiver<crate::resources::Resources>>,
    /// A file under `res/` changed: rescan resources.
    res_stale: bool,
    /// Library symbols from the Gradle cache.
    libs: crate::libs::Libraries,
    libs_rx: Option<Receiver<crate::libs::Libraries>>,
    compose_view: Option<ComposeView>,
    ctx: Option<egui::Context>,
    keymap: Keymap,
    show_shortcuts: bool,
    /// What the Find tab lists, e.g. "References to `User`".
    found_title: String,
    frames: u64,
    find: Option<find::FindBar>,
    /// ⌘-hover usage lists by name: (index generation, result once counted).
    usages: HashMap<String, (u64, Option<Vec<FoundLine>>)>,
    usages_rx: Vec<Receiver<(String, u64, Vec<FoundLine>)>>,
    /// Compile errors and warnings from the last Gradle run: file → (line, column, severity, message).
    build_problems: HashMap<PathBuf, Vec<(u32, u32, u8, String)>>,
    settings: comfort::Settings,
    /// The window had focus last frame (auto-save runs when it loses it).
    window_focused: bool,
    show_settings: bool,
    /// Hash of the last saved session, and when it was written.
    session_saved: (u64, Instant),
    /// File whose local history window is open.
    history_for: Option<PathBuf>,
    /// Last slow-frame report from the watchdog, shown in the status bar for a while.
    slow: Option<(String, Instant)>,
    perf: perf::Perf,
    tests: tests_ui::Tests,
    /// Which side panels are open (⌘1, ⌘9, ⌘4); focus mode hides all three.
    show_project: bool,
    show_right: bool,
    show_bottom: bool,
    focus: bool,
    toasts: Vec<toasts::Toast>,
    toast_seq: u64,
    /// The running Gradle task and when it started, for the note when it ends.
    gradle_started: Option<(String, Instant)>,
    fix_popup: Option<assist::FixPopup>,
    new_file_window: Option<assist::NewFile>,
    /// The picked build variant (`debug`, `freeRelease`…) and the app module's variants.
    variant: String,
    variants: Vec<String>,
    translations: translations::Translations,
    /// The declaration a rename started from, for the text search fallback.
    rename_target: Option<crate::usage::Target>,
    /// The ⌘-hover card's declaration: (file, offset, text version, index version) → target.
    hover_target: Option<HoverTarget>,
    /// Virtual devices for the device picker, read in the background on open and when it opens.
    avds: Vec<String>,
    avds_rx: Option<Receiver<Vec<String>>>,
    /// The Deep link window's text while it is open.
    deep_link: Option<String>,
    /// Bumped when the resources are scanned again, so views built from them refresh.
    res_gen: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>) -> App {
        crate::theme::install_fonts(&cc.egui_ctx);
        let settings = comfort::Settings::load();
        settings.apply(&cc.egui_ctx);
        crate::theme::style(&cc.egui_ctx);
        let mut app = App {
            project: None,
            tree: Vec::new(),
            tree_view: tree::View::default(),
            tree_filter: String::new(),
            locate: None,
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
            lsp_found: None,
            usages_popup: None,
            logcat: Default::default(),
            run: Default::default(),
            capture: Default::default(),
            caret_pos: None,
            bottom: Bottom::Problems,
            right: Right::Compose,
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
            git: Default::default(),
            branch: None,
            outputs_gen: 0,
            res: Default::default(),
            res_rx: None,
            res_stale: false,
            libs: Default::default(),
            libs_rx: None,
            compose_view: None,
            ctx: Some(cc.egui_ctx.clone()),
            keymap: load_setting("keymap").map_or(Keymap::Eclipse, |s| Keymap::from_name(&s)),
            show_shortcuts: false,
            found_title: String::new(),
            frames: 0,
            find: None,
            usages: HashMap::new(),
            usages_rx: Vec::new(),
            build_problems: HashMap::new(),
            settings,
            window_focused: true,
            show_settings: false,
            session_saved: (0, Instant::now()),
            history_for: None,
            slow: None,
            perf: Default::default(),
            tests: Default::default(),
            show_project: true,
            show_right: true,
            show_bottom: true,
            focus: false,
            toasts: Vec::new(),
            toast_seq: 0,
            gradle_started: None,
            fix_popup: None,
            new_file_window: None,
            variant: "debug".into(),
            variants: Vec::new(),
            translations: Default::default(),
            rename_target: None,
            hover_target: None,
            avds: Vec::new(),
            avds_rx: None,
            deep_link: None,
            res_gen: 0,
        };
        if let Some(dir) = open.or_else(last_project) {
            app.open_project(&dir, &cc.egui_ctx);
        }
        if let Some(f) = std::env::var_os("DROID_LITE_OPEN") {
            app.open_file(Path::new(&f), None);
        }
        if std::env::var_os("DROID_LITE_BLAME").is_some() {
            app.git.show_column = true;
            app.right = Right::PullRequest;
        }
        if let Ok(q) = std::env::var("DROID_LITE_SEARCH") {
            app.search = Some(Search {
                kind: SearchKind::Class,
                query: q,
                hits: Vec::new(),
                selected: 0,
                stale: true,
                pinned: Vec::new(),
                title: None,
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
        self.perf.opening = Some(Instant::now());
        self.status = format!("Opening {}…", dir.display());
        let (dir, c, view) = (dir.to_path_buf(), ctx.clone(), self.tree_view);
        std::thread::spawn(move || {
            let project = Project::open(&dir);
            let index = Index::build(&project.files);
            let branch = crate::git::branch(&project.root);
            let tree = tree::build(&project, view);
            let res = crate::resources::Resources::scan(&project.root);
            let _ = tx.send((project, index, branch, tree, res));
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
        self.index = index;
        for t in &self.tabs {
            self.index.update(&t.path, &t.text);
        }
        self.branch = branch;
        self.reload_stability(&project, ctx);
        self.watcher = crate::watch::Watcher::start(&project.root, ctx.clone());
        self.status = format!(
            "Opened {} ({} modules, {} files, {} symbols, {} resources)",
            project.name,
            project.modules.len(),
            project.files.len(),
            self.index.symbol_count(),
            self.res.len()
        );
        self.project = Some(project);
        self.load_variants();
        self.load_avds(ctx);
        self.load_plugins(ctx);
        self.start_lib_index(ctx);
        if self.tabs.is_empty() {
            self.restore_session();
        }
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
        self.lsp = Lsp::start(&p.root, ctx.clone(), &self.settings.lsp_heap);
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
            if libs_ui::is_binary_archive(path) {
                self.status = format!("{} has no sources to show (binary only)", path.display());
                return;
            }
            let text = if crate::libs::is_archive_path(path) {
                crate::libs::read_entry(path)
            } else {
                std::fs::read_to_string(path).ok()
            };
            let Some(text) = text else {
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
                readonly: crate::libs::is_archive_path(path),
                git: Default::default(),
                rev: 0,
                annotations: None,
                select: None,
                caret: None,
                bracket: (u64::MAX, 0, None),
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
        if tab.readonly {
            self.status = "Library sources are read-only".into();
            return;
        }
        if tab.text != tab.saved || !tab.path.exists() {
            comfort::record_history(&tab.path);
        }
        match std::fs::write(&tab.path, &tab.text) {
            Ok(()) => {
                self.git.changes_stale = true;
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
        if self.goto_android_reference(&word, at) {
            return;
        }
        let defs = self.index.definitions(&word, &path);
        // ⌘-click on the declaration itself lists where it is used, like Android Studio
        if defs.iter().any(|d| d.path == path && d.offset == at)
            && let Some(ctx) = self.ctx.clone()
        {
            let bottom = self.bottom;
            self.find_references(&ctx);
            self.bottom = bottom; // the dropdown shows them; the Find panel stays one click away
            self.usages_popup = Some(UsagesPopup {
                word,
                pos: self.caret_pos.unwrap_or(egui::pos2(200.0, 200.0)),
                selected: 0,
                decl: (path, at),
                frame: self.frames,
                settled: false,
            });
            return;
        }
        if let Some(l) = self.lsp.as_mut().filter(|l| l.ready) {
            let (line, col) = lsp::position(&self.tabs[self.active].text, at);
            l.definition(&path, line, col);
        }
        match defs.len() {
            // not in the project: a library class or function
            0 => {
                let found: Vec<Hit> = self
                    .libs
                    .named(&word)
                    .map(|s| libs_ui::lib_hit(s, 0))
                    .collect();
                match found.len() {
                    0 => {
                        self.status = format!(
                            "No declaration found for `{word}` in the project or its libraries"
                        )
                    }
                    1 if libs_ui::is_binary_archive(&found[0].path) => {
                        self.status =
                            format!("`{word}`: {} (no sources downloaded)", found[0].detail)
                    }
                    1 => self.open_file(&found[0].path.clone(), None),
                    n => self.pick(format!("`{word}` in {n} libraries"), found),
                }
            }
            1 => self.open_file(&defs[0].path.clone(), Some(defs[0].offset)),
            _ => {
                self.pick(format!("Declarations of `{word}`"), defs);
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
        self.lsp_found = None;
        // The project search always runs: kotlin-lsp answers empty or with one file while it
        // still imports a big Gradle project. Its answer is merged in when it comes.
        let target = self.usage_target(at);
        self.search_project(word, true, target, ctx);
        if let Some(l) = self.lsp.as_mut().filter(|l| l.ready) {
            let t = &self.tabs[self.active];
            let (line, col) = lsp::position(&t.text, at);
            l.references(&t.path, line, col);
        }
    }

    /// Searches every project file (open tabs use their unsaved text) in the background.
    /// `code_refs`: whole identifiers only, outside strings and comments in code files.
    /// What the name at `at` in the active file refers to: its declaration when that is known
    /// for sure, so same-named members of other classes are not counted as its uses.
    fn usage_target(&self, at: usize) -> Option<crate::usage::Target> {
        let t = self.tabs.get(self.active)?;
        let (a, b) = word_bounds(&t.text, at);
        let word = t.text.get(a..b)?;
        if let Some(d) = t
            .syms
            .decls
            .iter()
            .find(|d| d.offset == a && d.name == word)
        {
            return Some(crate::usage::Target::new(
                &self.index,
                &t.path,
                &t.syms,
                d,
                &t.text,
            ));
        }
        // from a use: the one declaration this place can mean
        let mut fits = self
            .index
            .definitions(word, &t.path)
            .into_iter()
            .take(12)
            .filter_map(|h| {
                let f = self.index.files.get(&h.path)?;
                let d = f.decls.iter().find(|d| d.offset == h.offset)?;
                let text = self.current_text(&h.path)?;
                Some(crate::usage::Target::new(&self.index, &h.path, f, d, &text))
            })
            .filter(|target| target.is_use(&t.path, &t.text, &t.syms, a));
        let one = fits.next()?;
        fits.next().is_none().then_some(one)
    }

    fn search_project(
        &mut self,
        word: String,
        code_refs: bool,
        target: Option<crate::usage::Target>,
        ctx: &egui::Context,
    ) {
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
                    let hits = crate::refactor::occurrences(&text, &word).into_iter();
                    match &target {
                        Some(t) => {
                            let syms = scan(&text);
                            hits.map(|r| r.start)
                                .filter(|&o| t.is_use(&f, &text, &syms, o))
                                .collect()
                        }
                        None => hits.map(|r| r.start).collect(),
                    }
                } else if target.as_ref().is_some_and(|t| !t.is_type) {
                    // a function or property is not named in layouts and manifests
                    Vec::new()
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

    /// Adds kotlin-lsp references the project search missed (each place once, in file order).
    fn merge_found(&mut self, extra: Vec<FoundLine>) {
        let seen: std::collections::HashSet<(PathBuf, usize)> = self
            .found
            .iter()
            .map(|f| (f.path.clone(), f.offset))
            .collect();
        let before = self.found.len();
        self.found.extend(
            extra
                .into_iter()
                .filter(|f| !seen.contains(&(f.path.clone(), f.offset))),
        );
        self.found
            .sort_by(|a, b| a.path.cmp(&b.path).then(a.offset.cmp(&b.offset)));
        self.status = format!(
            "{} references ({} more from kotlin-lsp)",
            self.found.len(),
            self.found.len() - before
        );
    }

    /// Compiles the active file's module, so compile errors show in the editor and Problems.
    fn build_module(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let file = self.tabs.get(self.active).map(|t| t.path.clone());
        let module = file
            .as_ref()
            .and_then(|f| p.module_of(f))
            .or_else(|| p.modules.iter().find(|m| m.name != ":"))
            .cloned();
        let Some(m) = module else { return };
        let build = ["build.gradle.kts", "build.gradle"]
            .iter()
            .find_map(|f| std::fs::read_to_string(m.dir.join(f)).ok())
            .unwrap_or_default();
        let prefix = if m.name == ":" {
            String::new()
        } else {
            m.name.clone()
        };
        let task = if build.contains("com.android") || build.contains("android {") {
            "compileDebugKotlin"
        } else {
            "compileKotlin"
        };
        self.run_gradle(format!("{prefix}:{task}"), ctx);
    }

    /// After a Gradle run: compiler errors and warnings become editor marks and Problems rows.
    /// Compiler errors from the console, and Android lint / detekt findings from the reports the
    /// run wrote (`since` is when it started).
    fn collect_build_problems(&mut self, since: Option<SystemTime>) {
        self.build_problems.clear();
        for line in &self.console {
            if let Some((p, l, c, sev, msg)) = compiler_problem(line) {
                self.build_problems
                    .entry(p)
                    .or_default()
                    .push((l, c, sev, msg));
            }
        }
        let mut found = 0;
        if let (Some(p), Some(since)) = (&self.project, since) {
            let dirs: Vec<PathBuf> = p.modules.iter().map(|m| m.dir.clone()).collect();
            for (f, l, c, error, msg) in crate::reports::collect(&dirs, &p.root, since) {
                let sev = if error { 1 } else { 2 };
                self.build_problems
                    .entry(f)
                    .or_default()
                    .push((l, c, sev, msg));
                found += 1;
            }
        }
        if found > 0 {
            self.status = format!(
                "Lint: {}, in Problems",
                translations::count(found, "finding")
            );
            self.bottom = Bottom::Problems;
        }
        let errors: usize = self
            .build_problems
            .values()
            .flatten()
            .filter(|p| p.2 == 1)
            .count();
        if errors > 0 {
            self.status = format!(
                "Build: {errors} compile errors in {} files",
                self.build_problems.len()
            );
            self.bottom = Bottom::Problems;
        }
        for t in self.tabs.iter_mut() {
            t.recolor = true;
        }
    }

    fn run_gradle(&mut self, task: String, ctx: &egui::Context) {
        self.run.pending = None; // a plain task never starts the app afterwards
        self.run_gradle_on(task, None, ctx);
    }

    /// Runs a Gradle task; `serial` makes install tasks target that device.
    fn run_gradle_on(&mut self, task: String, serial: Option<&str>, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        if let Some(r) = self.gradle.as_mut() {
            r.stop();
        }
        self.console.clear();
        self.console.push(format!("> ./gradlew {task}"));
        match gradle::Run::start_on(p, &task, serial, ctx.clone()) {
            Ok(r) => {
                self.gradle = Some(r);
                self.gradle_started = Some((task.clone(), Instant::now()));
            }
            Err(e) => self.console.push(format!("Could not start Gradle: {e}")),
        }
        self.bottom = Bottom::Build;
    }

    // ---------- per-frame background work ----------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.indexing
            && let Ok((project, index, branch, tree, res)) = rx.try_recv()
        {
            self.indexing = None;
            self.perf.opened_in = self.perf.opening.take().map(|t| t.elapsed());
            self.tree = tree;
            self.res = res;
            self.res_gen += 1;
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
            if let Some(l) = self.lsp_found.take() {
                self.merge_found(l);
            }
        }
        self.poll_tests();
        self.keep_watch(ctx);
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
                let since = self
                    .gradle_started
                    .as_ref()
                    .map(|(_, at)| SystemTime::now() - at.elapsed());
                if let Some((task, at)) = self.gradle_started.take() {
                    let secs = at.elapsed().as_secs();
                    let tests = task.contains("UnitTest") || task.contains("AndroidTest");
                    let summary = tests
                        .then(|| crate::android_kit::test_summary(&self.console))
                        .flatten()
                        .filter(|_| code.is_some());
                    let text = match (code, summary) {
                        (_, Some((s, _))) => format!("{s} in {secs} s"),
                        (Some(0), None) => format!("{task} finished in {secs} s"),
                        (Some(c), None) => format!("{task} failed (exit {c}) after {secs} s"),
                        (None, None) => format!("{task} stopped"),
                    };
                    self.toast(text, code == Some(0), Bottom::Build);
                }
                self.after_install(code == Some(0), ctx);
                self.outputs_gen += 1;
                self.collect_build_problems(since);
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
                                template: None,
                            })
                            .collect();
                        lsp_items.extend(
                            c.items
                                .drain(..)
                                .filter(|i| i.import.is_some() || i.template.is_some()),
                        );
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
                    if self.find_rx.is_some() {
                        self.lsp_found = Some(found);
                    } else {
                        self.merge_found(found);
                    }
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
                pinned: Vec::new(),
                title: None,
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

    /// The path of the open file and the declaration around the cursor, like Android Studio's
    /// navigation bar: `app › src › … › UserCard.kt › UserCard`. Click a declaration to jump to it.
    fn breadcrumb(&mut self, ui: &mut Ui) -> egui::Id {
        let Some(t) = self.tabs.get(self.active) else {
            return ui.next_auto_id();
        };
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        let rel = t.path.strip_prefix(&root).unwrap_or(&t.path);
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // long paths keep the module, "…", and the last two folders
        let shown: Vec<String> = if parts.len() > 5 {
            [
                parts[..1].to_vec(),
                vec!["…".into()],
                parts[parts.len() - 3..].to_vec(),
            ]
            .concat()
        } else {
            parts
        };
        let mut text = shown.join(" › ");
        let inside: Vec<&crate::kotlin::Decl> = t
            .syms
            .decls
            .iter()
            .filter(|d| d.body.as_ref().is_some_and(|b| b.contains(&t.cursor)))
            .collect();
        let mut jump = None;
        let r = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for d in inside.iter().filter_map(|d| d.container.as_ref()).take(1) {
                text.push_str(&format!(" › {d}"));
            }
            let label = ui.label(RichText::new(&text).small().color(theme::t().weak));
            for d in &inside {
                if ui
                    .link(RichText::new(format!("› {}", d.name)).small())
                    .clicked()
                {
                    jump = Some(d.offset);
                }
            }
            label.id
        });
        if let Some(o) = jump {
            let p = t.path.clone();
            self.open_file(&p, Some(o));
        }
        r.inner
    }

    /// Puts one uncommitted change back to its HEAD lines (the click on a change bar).
    fn revert_hunk(&mut self, h: &crate::git::Hunk) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let line_start =
            |n: usize| -> usize { t.text.split_inclusive('\n').take(n).map(str::len).sum() };
        let (a, b) = (line_start(h.new_start), line_start(h.new_start + h.new_len));
        let mut old: String = h.old.iter().map(|l| format!("{l}\n")).collect();
        if b == t.text.len() && !t.text.ends_with('\n') && old.ends_with('\n') {
            old.pop(); // the file's last line had no newline
        }
        t.text.replace_range(a..b, &old);
        t.caret = Some(a);
        t.mark_edited();
        self.status = format!("Reverted {} line(s) to HEAD", h.old.len().max(h.new_len));
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
                pinned: Vec::new(),
                title: None,
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
            Action::Implementations => self.goto_implementations(),
            Action::Find | Action::Replace => self.open_find(),
            Action::FindNext | Action::FindPrev if self.find.is_none() => self.open_find(),
            Action::FindNext => self.find_step(true),
            Action::FindPrev => self.find_step(false),
            Action::Build => self.build_module(ctx),
            Action::Settings => self.show_settings = true,
            Action::LocalHistory => {
                self.history_for = self.tabs.get(self.active).map(|t| t.path.clone())
            }
            Action::LocateFile => self.locate_file(),
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
            Action::NewWindow => self.new_window(),
            Action::Palette => self.search = open(SearchKind::Actions),
            Action::ToggleProject => self.toggle_panel(|a| &mut a.show_project),
            Action::ToggleRight => self.toggle_panel(|a| &mut a.show_right),
            Action::ToggleBottom => self.toggle_panel(|a| &mut a.show_bottom),
            Action::FocusMode => self.focus = !self.focus,
            Action::RunApp => {
                if self.gradle.is_none() && self.run.launching.is_none() {
                    self.run_on_device(crate::device::Launch::App, ctx);
                }
            }
            Action::RunTask => {
                let t = self.gradle_task.clone();
                self.run_gradle(t, ctx);
            }
            Action::QuickFix => self.quick_fix(),
            Action::Translations => self.translations.open = true,
            Action::DeepLink => {
                self.deep_link = Some(self.res.links.first().cloned().unwrap_or_default())
            }
            Action::NewFile => self.new_file(None),
        }
    }

    /// Opens or closes one panel. In focus mode it leaves focus mode with only that panel open.
    fn toggle_panel(&mut self, panel: fn(&mut App) -> &mut bool) {
        if self.focus {
            self.focus = false;
            self.show_project = false;
            self.show_right = false;
            self.show_bottom = false;
            *panel(self) = true;
        } else {
            let open = panel(self);
            *open = !*open;
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
        t.caret = Some(cursor.min(t.text.len()));
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

    /// Asks for a project and opens it in a new window: another Droid Lite process, so each
    /// window has its own project, tabs and Gradle runs.
    fn new_window(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Open a project in a new window")
            .pick_folder()
        else {
            return;
        };
        let started = window_command(&dir).and_then(|mut c| c.spawn());
        self.status = match started {
            Ok(mut child) => {
                // reap it when that window closes
                std::thread::spawn(move || child.wait());
                format!("Opened {} in a new window", dir.display())
            }
            Err(e) => format!("Could not open a new window: {e}"),
        };
    }

    fn help_menu(&mut self, ui: &mut Ui) {
        ui.menu_button("Help", |ui| {
            if ui
                .add(
                    egui::Button::new("New window…")
                        .shortcut_text(keymap::text(self.keymap, Action::NewWindow)),
                )
                .clicked()
            {
                ui.close();
                self.new_window();
            }
            if ui
                .add(
                    egui::Button::new("Settings…")
                        .shortcut_text(keymap::text(self.keymap, Action::Settings)),
                )
                .clicked()
            {
                self.show_settings = true;
                ui.close();
            }
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
            ui.label(
                RichText::new("Keymap")
                    .color(crate::theme::t().weak)
                    .small(),
            );
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
                                    .color(crate::theme::t().accent),
                            );
                            ui.end_row();
                        }
                        ui.label("Go to declaration");
                        ui.label(
                            RichText::new("⌘-click")
                                .monospace()
                                .color(crate::theme::t().accent),
                        );
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
            s.hits = if s.kind == SearchKind::Pick {
                index::rank_by(&s.query, s.pinned.iter(), |h| &h.name, 200)
                    .into_iter()
                    .map(|(score, h)| Hit { score, ..h.clone() })
                    .collect()
            } else {
                self.search_hits(s.kind, &s.query)
            };
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
                    self.search_project(s.query.clone(), false, None, ctx);
                }
                close = true;
            } else if let Some(h) = s.hits.get(s.selected) {
                chosen = Some((h.path.clone(), h.offset));
            }
        }
        let title = s.title.clone().unwrap_or_else(|| {
            format!(
                "{}   {}",
                s.kind.action().label(),
                keymap::text(self.keymap, s.kind.action())
            )
        });
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
            if s.kind == SearchKind::Actions {
                if let Some(a) = keymap::ALL.get(o) {
                    self.run(*a, ctx);
                }
            } else {
                self.open_file(&p, Some(o));
            }
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
            SearchKind::Class => {
                let mut v = self.index.search(q, true, 60);
                v.extend(self.lib_hits(q, true, 30));
                v.sort_by_key(|h| std::cmp::Reverse(h.score));
                v.truncate(60);
                v
            }
            SearchKind::Symbol => {
                let mut v = self.index.search(q, false, 60);
                v.extend(self.lib_hits(q, false, 30));
                v.sort_by_key(|h| std::cmp::Reverse(h.score));
                v.truncate(60);
                v
            }
            SearchKind::File => files(q, &mut self.project.iter().flat_map(|p| p.files.iter()), 60),
            SearchKind::Recent => files(q, &mut self.recent.iter(), 60),
            SearchKind::Everywhere => {
                let mut v = self.index.search(q, false, 30);
                v.extend(self.lib_hits(q, false, 10));
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
            SearchKind::Pick => Vec::new(), // filtered in search_window from `pinned`
            SearchKind::Actions => {
                index::rank_by(q, keymap::ALL.iter().enumerate(), |(_, a)| a.label(), 100)
                    .into_iter()
                    .map(|(score, (i, a))| Hit {
                        name: a.label().into(),
                        detail: keymap::text(self.keymap, *a),
                        path: PathBuf::new(),
                        offset: i,
                        score,
                    })
                    .collect()
            }
        }
    }

    // ---------- panels ----------

    /// Three groups, like Android Studio's new UI: project and navigation on the left, search in
    /// the middle, Gradle task and device runs on the right.
    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let name = self
                .project
                .as_ref()
                .map_or("Open project…".to_string(), |p| p.name.clone());
            ui.menu_button(
                RichText::new(format!("{}  {name}  ▾", ic::FOLDER_OPEN)).strong(),
                |ui| {
                    if ui.button("Translations…").clicked() {
                        ui.close();
                        self.translations.open = true;
                    }
                    if ui.button("Open project…").clicked() {
                        ui.close();
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Open an Android project folder")
                            .pick_folder()
                        {
                            self.open_project(&dir, &ui.ctx().clone());
                        }
                    }
                    if ui
                        .add(
                            egui::Button::new("New window…")
                                .shortcut_text(keymap::text(self.keymap, Action::NewWindow)),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.new_window();
                    }
                },
            )
            .response
            .on_hover_text("Open a project, or a second one in a new window");
            if let Some(b) = self.branch.clone()
                && theme::button(ui, true, format!("{}  {b}", ic::GIT_BRANCH))
                    .on_hover_text("Show this branch's changes")
                    .clicked()
            {
                self.right = Right::PullRequest;
                self.show_right = true;
                self.focus = false;
            }
            ui.add_space(6.0);
            if theme::button(ui, !self.history.is_empty(), ic::ARROW_LEFT)
                .on_hover_text("Back  ⌘[")
                .clicked()
            {
                self.back();
            }
            if theme::button(ui, !self.forward.is_empty(), ic::ARROW_RIGHT)
                .on_hover_text("Forward  ⌘]")
                .clicked()
            {
                self.fwd();
            }
            self.refactor_menu(ui);
            self.help_menu(ui);
            // search box in the middle of the window, moved left when the run group needs the room
            // (its width is known from the last frame)
            let w = 260.0;
            let right_id = egui::Id::unique("top_bar_right_width");
            let right_w: f32 = ui.data(|d| d.get_temp(right_id)).unwrap_or(0.0);
            let left = (ui.max_rect().center().x - w / 2.0)
                .min(ui.max_rect().right() - right_w - 16.0 - w);
            ui.add_space((left - ui.cursor().left()).max(8.0));
            if search_box(ui, w)
                .on_hover_text("Go to class  ⌘O\nGo to file  ⇧⌘O\nSearch everywhere  ⇧⇧")
                .clicked()
            {
                self.search = Some(Search {
                    kind: SearchKind::Everywhere,
                    query: String::new(),
                    hits: Vec::new(),
                    selected: 0,
                    stale: true,
                    pinned: Vec::new(),
                    title: None,
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.run_controls(ui);
                ui.add_space(10.0);
                if self.gradle.is_some() {
                    if theme::button(
                        ui,
                        true,
                        RichText::new(format!("{}  Stop", ic::STOP)).color(ERROR),
                    )
                    .clicked()
                        && let Some(r) = self.gradle.as_mut()
                    {
                        r.stop();
                    }
                    ui.spinner();
                } else if theme::button(
                    ui,
                    self.project.is_some(),
                    RichText::new(format!("{}  Run task", ic::PLAY)).color(RUN_GREEN),
                )
                .on_hover_text("Run the Gradle task")
                .clicked()
                {
                    let t = self.gradle_task.clone();
                    self.run_gradle(t, &ui.ctx().clone());
                }
                let r = egui::ComboBox::from_id_salt("gradle_task")
                    .selected_text(format!("{}  {}", ic::WRENCH, self.gradle_task))
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for t in [
                            "assembleDebug",
                            "installDebug",
                            "testDebugUnitTest",
                            "lint",
                            "detekt",
                            "clean",
                            "stabilityDump",
                        ] {
                            if ui.selectable_label(self.gradle_task == t, t).clicked() {
                                self.gradle_task = t.into();
                            }
                        }
                        ui.separator();
                        ui.add(
                            TextEdit::singleline(&mut self.gradle_task)
                                .desired_width(180.0)
                                .hint_text("Any Gradle task"),
                        );
                    });
                run_ui::name_combo(&r.response, "Gradle task");
                let used = ui.min_rect().width();
                ui.data_mut(|d| d.insert_temp(right_id, used));
            });
        });
    }

    /// Indexing and messages on the left; symbols, the language server and the cursor on the right.
    fn status_bar(&mut self, ui: &mut Ui) {
        let weak = theme::t().weak;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (action, icon, open) in [
                (Action::ToggleProject, ic::TREE_STRUCTURE, self.show_project),
                (Action::ToggleBottom, ic::TERMINAL, self.show_bottom),
                (Action::ToggleRight, ic::CUBE, self.show_right),
            ] {
                let open = open && !self.focus && self.project.is_some();
                let text = RichText::new(icon).color(if open { theme::t().accent } else { weak });
                let tip = format!("{}  {}", action.label(), keymap::text(self.keymap, action));
                if theme::button(ui, true, text).on_hover_text(tip).clicked() {
                    self.run(action, &ui.ctx().clone());
                }
            }
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            if self.indexing.is_some() {
                ui.spinner();
                ui.label(RichText::new("Indexing…").color(weak));
            }
            if let Some((report, at)) = &self.slow
                && at.elapsed().as_secs() < 15
            {
                ui.label(RichText::new(format!("{}  {report}", ic::WARNING)).color(WARN))
                    .on_hover_text(
                        "The window was blocked this long. The same line is printed to stderr.",
                    );
            }
            ui.label(RichText::new(&self.status).color(weak));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                if let Some(t) = self.tabs.get(self.active) {
                    let before = &t.text[..t.cursor.min(t.text.len())];
                    let line = before.matches('\n').count() + 1;
                    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
                    ui.label(RichText::new(format!("{line}:{col}")).color(weak));
                }
                match &self.lsp {
                    Some(l) if l.ready => ui.label(
                        RichText::new(format!("{}  {} ready", ic::CHECK_CIRCLE, l.name))
                            .color(RUN_GREEN),
                    ),
                    Some(l) => ui.label(RichText::new(format!("{} starting…", l.name)).color(weak)),
                    None => ui
                        .label(RichText::new("Built-in index").color(weak))
                        .on_hover_text(
                            "No Kotlin language server. Completion and go-to use the built-in \
                             index only. Install kotlin-lsp to get more (see README).",
                        ),
                };
                if self.indexing.is_none() {
                    ui.label(
                        RichText::new(format!("{} symbols", self.index.symbol_count())).color(weak),
                    );
                }
            });
        });
    }

    /// Switches the Project tree between the Android and the folder view (saved per project).
    fn set_tree_view(&mut self, v: tree::View) {
        self.tree_view = v;
        if let Some(p) = &self.project {
            self.tree = tree::build(p, v);
        }
        self.save_session(true);
    }

    /// Opens the folders down to the active file in the Project tree, selects it and scrolls to it.
    fn locate_file(&mut self) {
        let Some(f) = self.tabs.get(self.active).map(|t| t.path.clone()) else {
            return;
        };
        if self.tree.iter().any(|n| n.contains(&f)) {
            self.locate = Some((f, 20));
        } else {
            self.status = format!("{} is not in this project's tree", f.display());
        }
    }

    fn project_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            theme::section(ui, ic::TREE_STRUCTURE, "Project");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let tip = format!(
                    "{}  {}",
                    Action::LocateFile.label(),
                    keymap::text(self.keymap, Action::LocateFile)
                );
                if ui
                    .add_enabled(
                        !self.tabs.is_empty(),
                        egui::Button::new(ic::CROSSHAIR).frame(false),
                    )
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.locate_file();
                }
                let tip = format!(
                    "{}  {}",
                    Action::NewFile.label(),
                    keymap::text(self.keymap, Action::NewFile)
                );
                if ui
                    .add_enabled(
                        self.project.is_some(),
                        egui::Button::new(ic::FILE_PLUS).frame(false),
                    )
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.new_file(None);
                }
            });
        });
        if self.project.is_none() {
            ui.label("Open an Android project folder (the one with settings.gradle).");
            return;
        }
        let status: tree::StatusMap = self
            .git
            .changes
            .as_ref()
            .map(|c| c.working.iter().map(|w| (w.path.clone(), w.code)).collect())
            .unwrap_or_default();
        // view switch and file filter
        ui.horizontal(|ui| {
            let mut tabs = theme::Tabs::new(ui, "tree-view");
            for v in [tree::View::Android, tree::View::Project] {
                if tabs.tab(ui, self.tree_view == v, v.name()).clicked() && self.tree_view != v {
                    self.set_tree_view(v);
                }
            }
            tabs.finish(ui);
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.tree_filter)
                .desired_width(f32::INFINITY)
                .hint_text(format!("{}  Filter files", ic::MAGNIFYING_GLASS)),
        );
        let mut open = None;
        let active = self.tabs.get(self.active).map(|t| t.path.clone());
        let locate = self.locate.as_ref().map(|(f, _)| f.clone());
        let new_in = std::cell::Cell::new(None);
        let sel = tree::Sel {
            active: active.as_deref(),
            locate: locate.as_deref(),
            status: &status,
            syms: &self.index.files,
            new_in: &new_in,
        };
        let root = self.root();
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.tree_filter.trim().is_empty() {
                    tree::show(ui, &self.tree, sel, &mut open);
                    return;
                }
                // a flat list of matching files, like Android Studio's speed search
                let files = self
                    .project
                    .as_ref()
                    .map_or(&[][..], |p| p.files.as_slice());
                let hits = tree::filter(files, self.tree_filter.trim(), 300);
                if hits.is_empty() {
                    ui.label(RichText::new("No file matches").color(theme::t().weak));
                }
                for f in hits {
                    let dir = f
                        .parent()
                        .and_then(|d| d.strip_prefix(&root).ok())
                        .map(|d| d.display().to_string())
                        .unwrap_or_default();
                    let name = f
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let (icon, tint) = assist::kind_icon(f, self.index.files.get(f));
                    let mut job = egui::text::LayoutJob::default();
                    let fmt = |color| egui::TextFormat {
                        font_id: FontId::proportional(13.0),
                        color,
                        ..Default::default()
                    };
                    job.append(icon, 0.0, fmt(tint));
                    job.append(&format!("  {name}"), 0.0, fmt(theme::t().text));
                    let r = ui.horizontal(|ui| {
                        let r = ui.selectable_label(sel.active == Some(f.as_path()), job);
                        ui.label(RichText::new(dir).color(theme::t().weak).small());
                        r
                    });
                    if r.inner.clicked() {
                        open = Some(f.clone());
                    }
                }
            });
        // the folders open with an animation, so keep scrolling to the file until it ends
        if let Some((_, frames)) = &mut self.locate {
            *frames = frames.saturating_sub(1);
            if *frames == 0 {
                self.locate = None;
            }
            ui.ctx().request_repaint();
        }
        if let Some(f) = open {
            self.open_file(&f, None);
        }
        if let Some(dir) = new_in.take() {
            self.new_file(Some(dir));
        }
    }

    /// No project open: a big Open button and the recent projects.
    fn welcome(&mut self, ui: &mut Ui) {
        let t = theme::t();
        let mut open = None;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(24.0));
            ui.label(
                RichText::new("Droid Lite")
                    .size(28.0)
                    .strong()
                    .color(t.text),
            );
            ui.label(RichText::new("A light editor for Android and Kotlin").color(t.weak));
            ui.add_space(18.0);
            let button = egui::Button::new(
                RichText::new(format!("{}  Open project…", ic::FOLDER_OPEN))
                    .size(14.0)
                    .color(Color32::WHITE),
            )
            .fill(t.accent)
            .min_size(egui::vec2(200.0, 34.0));
            if ui.add(button).clicked() {
                open = rfd::FileDialog::new()
                    .set_title("Open an Android project folder")
                    .pick_folder();
            }
            ui.label(
                RichText::new("or drop a project folder on this window")
                    .small()
                    .color(t.weak),
            );
            let recent = recent_projects();
            if !recent.is_empty() {
                ui.add_space(26.0);
                ui.allocate_ui(egui::vec2(460.0_f32.min(ui.available_width()), 0.0), |ui| {
                    theme::section(ui, ic::CLOCK_COUNTER_CLOCKWISE, "Recent projects");
                    for p in recent {
                        let name = p
                            .file_name()
                            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
                        let home = std::env::var("HOME").unwrap_or_default();
                        let shown = p.display().to_string().replacen(&home, "~", 1);
                        if theme::problem_row(ui, ic::FOLDER_SIMPLE, t.weak, &name, &shown)
                            .clicked()
                        {
                            open = Some(p);
                        }
                    }
                });
            }
        });
        if let Some(dir) = open {
            self.open_project(&dir, &ui.ctx().clone());
        }
    }

    fn editor(&mut self, ui: &mut Ui) {
        self.refresh_annotations();
        let mut request: Option<gutter::Request> = None;
        let km = self.keymap;
        if self.project.is_none() && self.indexing.is_none() {
            self.welcome(ui);
            return;
        }
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
                ui.label(RichText::new(line).color(crate::theme::t().weak));
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
        let crumb_id = self.breadcrumb(ui);
        self.disk_banner(ui);
        if self.tabs[self.active].dirty() {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Unsaved changes")
                        .color(crate::theme::t().weak)
                        .small(),
                );
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
        self.editor_keys(ui);
        self.find_bar(ui);

        let font = FontId::monospace(crate::theme::font_size());
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
        let build: Vec<(usize, u8, String)> = self
            .build_problems
            .get(&tab.path)
            .into_iter()
            .flatten()
            .map(|(l, c, sev, m)| {
                (
                    lsp::offset(&tab.text, l.saturating_sub(1), c.saturating_sub(1)),
                    *sev,
                    m.clone(),
                )
            })
            .collect();
        marks.extend(build.iter().map(|(o, s, _)| {
            (
                *o..word_bounds(&tab.text, *o).1.max(o + 1),
                if *s == 1 { ERROR } else { WARN },
            )
        }));
        marks.extend(tab.diagnostics.iter().map(|(o, s, _)| {
            (
                *o..*o + word_bounds(&tab.text, *o).1.saturating_sub(*o).max(1),
                if *s == 1 { ERROR } else { WARN },
            )
        }));
        // background highlights: find matches and the bracket pair at the cursor
        let mut bg: Vec<(std::ops::Range<usize>, Color32)> = Vec::new();
        if let Some(bar) = &self.find {
            for (i, m) in bar.matches.iter().enumerate() {
                bg.push((
                    m.clone(),
                    if i == bar.current {
                        Color32::from_rgba_unmultiplied(0xE0, 0xA0, 0x30, 120)
                    } else {
                        Color32::from_rgba_unmultiplied(0xE0, 0xA0, 0x30, 50)
                    },
                ));
            }
        }
        if tab.bracket.0 != tab.rev || tab.bracket.1 != tab.cursor {
            tab.bracket = (
                tab.rev,
                tab.cursor,
                edit::matching_bracket(&tab.text, tab.cursor),
            );
        }
        if let Some((a, b)) = tab.bracket.2 {
            bg.push((
                a..a + 1,
                Color32::from_rgba_unmultiplied(0x58, 0x9D, 0xF6, 70),
            ));
            bg.push((
                b..b + 1,
                Color32::from_rgba_unmultiplied(0x58, 0x9D, 0xF6, 70),
            ));
        }
        bg.sort_by_key(|(r, _)| r.start);
        let lines = tab.text.matches('\n').count() + 1;
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let gutter_w = char_w * lines.to_string().len().max(3) as f32 + 18.0;
        let blame = tab.git.blame.clone();
        let hunks = tab.git.hunks.clone();
        let branch_hunks = tab.git.branch_hunks.clone();
        let mut revert_hunk: Option<usize> = None;
        let show_column = self.git.show_column;
        // plugin test cases named on a line get a ▶ link like JUnit tests
        let plugin_lenses: Vec<gutter::Lens> = {
            let lines: Vec<&str> = tab.text.split('\n').collect();
            self.tests
                .marks_at(&tab.path)
                .into_iter()
                .filter(|(l, _)| *l < lines.len())
                .map(|(l, m)| {
                    let names: Vec<&str> = m
                        .iter()
                        .map(|&i| self.tests.marks[i].label.as_str())
                        .collect();
                    let text = match names[..] {
                        [one] => format!("{}  Run {one}", ic::PLAY),
                        _ => format!("{}  Run a case: {}", ic::PLAY, names.join(", ")),
                    };
                    gutter::Lens::new(l, lines[l], vec![(text, gutter::Request::Tests(m))])
                })
                .collect()
        };
        let mut lens_lines: Vec<usize> = tab
            .annotations
            .iter()
            .flat_map(|a| a.lenses.iter())
            .chain(&plugin_lenses)
            .map(|l| l.line)
            .collect();
        lens_lines.sort_unstable();
        lens_lines.dedup();
        let lens_h = gutter::lens_height(row_h);
        let lens_key = lens_lines
            .iter()
            .fold(0x51u64, |h, l| h.rotate_left(5) ^ *l as u64);
        let mut open_commit: Option<String> = None;
        let Tab {
            text,
            cache,
            recolor,
            goto,
            caret,
            select,
            cursor,
            selection,
            lints,
            diagnostics,
            annotations,
            readonly,
            ..
        } = tab;
        let marks_key = marks.iter().chain(bg.iter()).fold(0x9e37u64, |h, (r, c)| {
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
                    let job = highlight::job(s, font.clone(), &marks, &bg);
                    let galley = ui.fonts_mut(|f| f.layout_job(job.clone()));
                    *cache = Some(EditorCache {
                        key,
                        job,
                        galley,
                        shown: None,
                    });
                    *recolor = false;
                }
            }
            let Some(c) = cache.as_mut() else {
                return ui.fonts_mut(|f| f.layout_job(Default::default()));
            };
            if lens_lines.is_empty() {
                return c.galley.clone();
            }
            // room for the link rows, made again only when the text or the rows change
            let k = c.key ^ lens_key;
            if c.shown.as_ref().is_none_or(|(sk, _)| *sk != k) {
                let g = gutter::with_lens_room(&c.galley, &lens_lines, lens_h);
                c.shown = Some((k, std::sync::Arc::new(g)));
            }
            c.shown.as_ref().map(|(_, g)| g.clone()).expect("set above")
        };
        let mut changed = false;
        let mut hover_word: Option<HoverWord> = None;
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
                    // library sources: a read-only copy keeps selection and navigation, drops edits
                    let ro_copy = readonly.then(|| text.clone());
                    let mut ro_str: &str = ro_copy.as_deref().unwrap_or("");
                    let buffer: &mut dyn egui::TextBuffer = if ro_copy.is_some() {
                        &mut ro_str
                    } else {
                        &mut *text
                    };
                    let out = TextEdit::multiline(buffer)
                        .id(id)
                        .code_editor()
                        .lock_focus(true)
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(f32::INFINITY)
                        .desired_rows(lines.max(30))
                        .layouter(&mut layouter)
                        .show(ui);
                    // the breadcrumb names the editor for screen readers
                    let _ = out.response.response.clone().labelled_by(crumb_id);
                    if let Some(c) = caret.take() {
                        // after an edit command: move the cursor, let the editor keep it in view
                        let c = c.min(text.len());
                        let mut st = out.state.clone();
                        st.cursor
                            .set_char_range(Some(CCursorRange::one(CCursor::new(
                                text[..c].chars().count(),
                            ))));
                        st.store(ui.ctx(), id);
                        out.response.request_focus();
                        *cursor = c;
                        *selection = c..c;
                    } else if let Some(target) = goto.take() {
                        let ci = text[..target.min(text.len())].chars().count();
                        let mut st = out.state.clone();
                        let picked = select.take();
                        let range = match &picked {
                            Some(r) => CCursorRange::two(
                                CCursor::new(text[..r.start.min(text.len())].chars().count()),
                                CCursor::new(ci),
                            ),
                            None => CCursorRange::one(CCursor::new(ci)),
                        };
                        st.cursor.set_char_range(Some(range));
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
                        // a find match stays selected; a plain jump has no selection
                        *selection =
                            picked.map_or(*cursor..*cursor, |r| r.start.min(*cursor)..*cursor);
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
                    let theme = crate::theme::t();
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
                    let aids = ScopeView {
                        text,
                        galley: &out.galley,
                        galley_pos: out.galley_pos,
                        gutter,
                        row_h,
                        char_w,
                        font: &font,
                    };
                    if let Some(o) = aids.paint(ui) {
                        *goto = Some(o);
                    }
                    // dim branch bars first: the bright uncommitted ones paint over them
                    if let Some(h) = &branch_hunks {
                        git_ui::paint_changes(ui, h, true, gutter, &out.galley, out.galley_pos);
                    }
                    if let Some(h) = &hunks {
                        revert_hunk = git_ui::paint_changes(
                            ui,
                            h,
                            false,
                            gutter,
                            &out.galley,
                            out.galley_pos,
                        );
                    }
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
                    let mut badge_right = None;
                    let lenses = annotations
                        .iter()
                        .flat_map(|a| a.lenses.iter())
                        .chain(&plugin_lenses);
                    if let Some(r) = gutter::paint_lenses(
                        ui,
                        lenses,
                        &out.galley,
                        out.galley_pos,
                        char_w,
                        lens_h,
                    ) {
                        request = Some(r);
                    }
                    if let Some(a) = annotations.as_ref() {
                        let swatch =
                            gutter::paint_swatches(ui, a, &out.galley, out.galley_pos, cursor_line);
                        badge_right =
                            gutter::paint_badges(ui, a, &out.galley, out.galley_pos, cursor_line)
                                .or(swatch);
                    }
                    if let Some(b) = &blame
                        && let Some(sha) = git_ui::paint_inline_blame(
                            ui,
                            b,
                            &out.galley,
                            out.galley_pos,
                            cursor_line,
                            badge_right,
                        )
                    {
                        open_commit = Some(sha);
                    }
                    // right-click: move the cursor there (unless inside the selection), then the menu
                    let resp = &out.response.response;
                    if resp.secondary_clicked()
                        && let Some(p) = resp.interact_pointer_pos()
                    {
                        let b = byte_of_char(
                            text,
                            out.galley.cursor_from_pos(p - out.galley_pos).index.0,
                        );
                        if !(selection.start..=selection.end).contains(&b)
                            || selection.start == selection.end
                        {
                            *goto = Some(b);
                        }
                    }
                    let has_selection = selection.start != selection.end;
                    resp.context_menu(|ui| {
                        if let Some(r) = gutter::context_menu(ui, km, has_selection) {
                            request = Some(r);
                        }
                    });
                    changed = out.response.changed();
                    // ⌘-click (Mac) or Ctrl-click (Eclipse / Windows habit) opens the declaration,
                    // matching the ⌘ / Ctrl hover underline
                    cmd_click = out.response.clicked()
                        && ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
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
                        } else if ui.input(|i| i.modifiers.command || i.modifiers.ctrl)
                            && let Some((a, w)) = typing::word_at(text, b)
                        {
                            // ⌘ / Ctrl held: the name becomes a link with a card about it
                            let chars = |o: usize| CCursor::new(text[..o].chars().count());
                            let r0 = out
                                .galley
                                .pos_from_cursor(chars(a))
                                .translate(out.galley_pos.to_vec2());
                            let r1 = out
                                .galley
                                .pos_from_cursor(chars(w))
                                .translate(out.galley_pos.to_vec2());
                            ui.painter().hline(
                                r0.left()..=r1.left(),
                                r0.bottom() - 1.0,
                                egui::Stroke::new(1.5, crate::theme::t().accent),
                            );
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            hover_word = Some((
                                text[a..w].to_string(),
                                a,
                                crate::resources::reference_at(text, a, w),
                                out.response.response.clone(),
                            ));
                        }
                    }
                });
            });
        let path = tab.path.clone();
        if changed {
            // analysis, the index and kotlin-lsp catch up after a typing pause (see `settle`)
            tab.mark_edited();
            self.update_completion(false);
        }
        if let Some((word, at, res_ref, resp)) = hover_word {
            resp.on_hover_ui_at_pointer(|ui| match res_ref {
                Some((kind, name)) => self.resource_card(ui, &kind, &name),
                None => self.hover_card(ui, &word, at),
            });
        }
        if cmd_click {
            self.goto_definition();
        }
        if let (Some(i), Some(h)) = (revert_hunk, hunks) {
            self.revert_hunk(&h[i]);
        }
        if let Some(sha) = open_commit {
            self.open_commit(&sha, &path, &ui.ctx().clone());
        }
        if let Some(r) = request {
            self.handle(r, &ui.ctx().clone());
        }
        if popup_pos.is_some() {
            self.caret_pos = popup_pos;
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
            let imported: Vec<&str> = t.syms.imports.iter().map(|s| &**s).collect();
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
                    template: None,
                });
            }
            // library classes and top-level functions (Text, Column, ViewModel…), with their import
            let have: std::collections::HashSet<String> =
                items.iter().map(|i| i.label.clone()).collect();
            for (_, name) in index::rank_by(prefix, self.libs.names(), |n| n, 20) {
                let Some(sym) = self.libs.named(name).next() else {
                    continue;
                };
                if have.contains(name) {
                    continue;
                }
                let fq = sym.qualified();
                let needs = (!imported.contains(&fq.as_str())
                    && *sym.package != *t.syms.package
                    && !imported
                        .iter()
                        .any(|i| i.ends_with(".*") && &*sym.package == i.trim_end_matches(".*")))
                .then_some(fq);
                items.push(Item {
                    label: name.to_string(),
                    detail: libs_ui::lib_hit(sym, 0).detail,
                    insert: name.to_string(),
                    import: needs,
                    template: None,
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
                    template: None,
                });
            }
            // live templates: an exact name goes first, so Tab expands it
            if t.path.extension().is_some_and(|e| e == "kt") {
                for tpl in crate::android_kit::TEMPLATES
                    .iter()
                    .filter(|x| x.abbr.starts_with(prefix))
                {
                    let item = Item {
                        label: tpl.abbr.to_string(),
                        detail: format!("template · {}", tpl.about),
                        insert: tpl.abbr.to_string(),
                        import: None,
                        template: Some(tpl),
                    };
                    if tpl.abbr == prefix {
                        items.insert(0, item);
                    } else {
                        items.push(item);
                    }
                }
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
        if let Some(tpl) = item.template {
            self.expand_template(c.start, end, tpl);
            return;
        }
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

    /// The usages the dropdown lists: every found line except the declaration itself.
    fn popup_usages(&self) -> Vec<&FoundLine> {
        let Some(p) = &self.usages_popup else {
            return Vec::new();
        };
        usage_filter::apply(&self.settings, &self.root(), &self.found)
            .into_iter()
            .map(|i| &self.found[i])
            .filter(|f| (&f.path, f.offset) != (&p.decl.0, p.decl.1))
            .collect()
    }

    fn root(&self) -> PathBuf {
        self.project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default()
    }

    /// ↑ ↓ Enter Esc for the usages dropdown, taken before the editor sees them.
    fn usages_popup_keys(&mut self, ctx: &egui::Context) {
        if self.usages_popup.is_none() {
            return;
        }
        let key = |k| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        let (esc, down, up, enter) = (
            key(Key::Escape),
            key(Key::ArrowDown),
            key(Key::ArrowUp),
            key(Key::Enter),
        );
        let n = self.popup_usages().len();
        let chosen = enter
            .then(|| self.usages_popup.as_ref().map(|p| p.selected))
            .flatten()
            .and_then(|i| {
                self.popup_usages()
                    .get(i)
                    .map(|f| (f.path.clone(), f.offset))
            });
        if let Some(p) = self.usages_popup.as_mut() {
            if down {
                p.selected = (p.selected + 1).min(n.saturating_sub(1));
            }
            if up {
                p.selected = p.selected.saturating_sub(1);
            }
        }
        if esc || chosen.is_some() {
            self.usages_popup = None;
        }
        if let Some((f, o)) = chosen {
            self.open_file(&f, Some(o));
        }
    }

    /// Android Studio's "Show usages" dropdown: one row per usage (file:line and the code),
    /// at the caret. One usage jumps straight to it.
    fn usages_popup_ui(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.usages_popup else { return };
        let (word, pos, selected, settled) = (p.word.clone(), p.pos, p.selected, p.settled);
        let searching = self.find_rx.is_some();
        let list: Vec<FoundLine> = self.popup_usages().into_iter().cloned().collect();
        let all = self
            .found
            .iter()
            .filter(|f| (&f.path, f.offset) != (&p.decl.0, p.decl.1))
            .count();
        if !searching && !settled && list.len() == 1 {
            let (f, o) = (list[0].path.clone(), list[0].offset);
            self.usages_popup = None;
            self.open_file(&f, Some(o));
            return;
        }
        let (mut chosen, mut to_panel) = (None, false);
        let title = if searching {
            format!("Usages of `{word}`  searching…")
        } else {
            let mut files: Vec<&Path> = list.iter().map(|f| f.path.as_path()).collect();
            files.dedup();
            format!(
                "Usages of `{word}`  {} in {} files",
                list.len(),
                files.len()
            )
        };
        let area = egui::Area::new(egui::Id::unique("usages_popup"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(640.0);
                    ui.label(RichText::new(title).strong());
                    usage_filter::bar(ui, &mut self.settings, all - list.len());
                    ui.separator();
                    if !searching && list.is_empty() {
                        let why = if all > 0 {
                            "All usages are hidden by the filters"
                        } else {
                            "No usages found"
                        };
                        ui.label(RichText::new(why).color(theme::t().weak));
                    }
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for (i, f) in list.iter().enumerate() {
                                let (icon, tint) = file_icon(&f.path);
                                let name = f
                                    .path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let mut job = egui::text::LayoutJob::default();
                                let fmt = |color, size| egui::TextFormat {
                                    font_id: FontId::monospace(size),
                                    color,
                                    ..Default::default()
                                };
                                job.append(icon, 0.0, fmt(tint, 12.0));
                                job.append(
                                    &format!(" {name}:{}  ", f.line),
                                    0.0,
                                    fmt(theme::t().weak, 12.0),
                                );
                                job.append(f.text.trim(), 0.0, fmt(theme::t().text, 12.5));
                                let r = ui.selectable_label(i == selected, job);
                                if i == selected {
                                    r.scroll_to_me(None);
                                }
                                if r.clicked() {
                                    chosen = Some((f.path.clone(), f.offset));
                                }
                            }
                        });
                    ui.separator();
                    let more = format!(
                        "Open in Find panel  {}",
                        keymap::text(self.keymap, Action::References)
                    );
                    if ui.link(RichText::new(more).small()).clicked() {
                        to_panel = true;
                    }
                });
            });
        if let Some(p) = self.usages_popup.as_mut() {
            p.settled |= !searching;
            p.selected = p.selected.min(list.len().saturating_sub(1));
        }
        let opened_now = self
            .usages_popup
            .as_ref()
            .is_some_and(|p| p.frame == self.frames);
        let clicked_away = !opened_now && area.response.clicked_elsewhere();
        if chosen.is_some() || to_panel || clicked_away {
            self.usages_popup = None;
        }
        if to_panel {
            self.bottom = Bottom::Find;
        }
        if let Some((f, o)) = chosen {
            self.open_file(&f, Some(o));
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
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::CollapsingHeader::new(
                    RichText::new(format!("{}  Composables ({})", ic::CUBE, composables.len()))
                        .strong(),
                )
                .default_open(composables.len() <= 8)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("composables")
                        .max_height(220.0)
                        .show(ui, |ui| {
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
                let Some(d) = composables.iter().find(|d| d.name == current) else {
                    return;
                };
                let key = (
                    tab.path.clone(),
                    tab.rev,
                    self.index.generation,
                    self.outputs_gen,
                    d.name.clone(),
                );
                if self.compose_view.as_ref().is_none_or(|v| v.key != key) {
                    let is_ui = |n: &str| compose::is_ui_call(n, &self.index, &self.libs);
                    let previews: Vec<String> = tab
                        .syms
                        .decls
                        .iter()
                        .filter(|d| d.is_preview())
                        .map(|d| d.name.clone())
                        .collect();
                    let tool = self
                        .project
                        .as_ref()
                        .and_then(|pr| pr.module_of(&tab.path))
                        .and_then(|m| PreviewTool::detect(m).map(|t| (m.clone(), t)));
                    let images = tool
                        .as_ref()
                        .map(|(m, t)| t.images(m, &previews))
                        .unwrap_or_default();
                    self.compose_view = Some(ComposeView {
                        key,
                        tree: compose::tree(&tab.text, d, &is_ui),
                        tool,
                        images,
                    });
                }
                let Some(view) = &self.compose_view else {
                    return;
                };
                ui.separator();
                theme::section(ui, ic::TREE_STRUCTURE, &format!("UI tree · {}", d.name));
                if view.tree.is_empty() {
                    ui.label(RichText::new("No UI calls found.").weak());
                }
                for n in &view.tree {
                    node_ui(ui, n, &mut jump);
                }

                ui.separator();
                theme::section(ui, ic::IMAGE, "Previews");
                let previews: Vec<String> = tab
                    .syms
                    .decls
                    .iter()
                    .filter(|d| d.is_preview())
                    .map(|d| d.name.clone())
                    .collect();
                if previews.is_empty() {
                    ui.label(RichText::new("No @Preview functions in this file.").weak());
                }
                for p in &previews {
                    if ui.link(RichText::new(p).monospace()).clicked() {
                        jump = tab
                            .syms
                            .decls
                            .iter()
                            .find(|d| &d.name == p)
                            .map(|d| d.offset);
                    }
                }
                match &view.tool {
                    Some((m, tool)) => {
                        ui.horizontal(|ui| {
                            render = ui
                                .add_enabled(
                                    self.gradle.is_none(),
                                    egui::Button::new(format!("{}  Render previews", ic::IMAGE)),
                                )
                                .on_hover_text(tool.task(m))
                                .clicked();
                            ui.label(RichText::new(format!("{tool:?}")).weak());
                        });
                        // drop textures of images no longer shown, so memory stays flat
                        self.textures
                            .retain(|p, _| view.images.iter().any(|(_, q)| q == p));
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
                    let c = if l.severity == Severity::Warning {
                        WARN
                    } else {
                        INFO
                    };
                    let icon = if c == WARN {
                        ic::WARNING
                    } else {
                        ic::LIGHTNING
                    };
                    if theme::problem_row(ui, icon, c, &l.message, &format!("line {line}"))
                        .on_hover_text(l.rule)
                        .clicked()
                    {
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
    }

    fn bottom_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let problems = self
                .tabs
                .get(self.active)
                .map_or(0, |t| t.lints.len() + t.diagnostics.len())
                + self.build_problems.values().map(Vec::len).sum::<usize>();
            let labels = [
                (
                    Bottom::Problems,
                    format!("{}  Problems  {problems}", ic::WARNING),
                ),
                (
                    Bottom::Build,
                    if self.gradle.is_some() {
                        format!("{}  Building…", ic::CLOCK_COUNTER_CLOCKWISE)
                    } else {
                        format!("{}  Build", ic::STACK)
                    },
                ),
                (
                    Bottom::Find,
                    format!("{}  Find  {}", ic::MAGNIFYING_GLASS, self.found.len()),
                ),
                (Bottom::Logcat, format!("{}  Logcat", ic::TERMINAL)),
                (
                    Bottom::Tests,
                    if self.tests.running() {
                        format!("{}  Testing…", ic::CLOCK_COUNTER_CLOCKWISE)
                    } else {
                        format!("{}  Tests", ic::CHECK_CIRCLE)
                    },
                ),
                (Bottom::Performance, format!("{}  Performance", ic::GAUGE)),
            ];
            let mut tabs = theme::Tabs::new(ui, "bottom");
            for (b, label) in labels {
                if tabs.tab(ui, self.bottom == b, label).clicked() {
                    self.bottom = b;
                }
            }
            tabs.finish(ui);
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
        // `show_rows` adds the item spacing itself; every row is drawn exactly this high
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        match self.bottom {
            Bottom::Problems => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // compile errors from the last build, every file
                        let root = self
                            .project
                            .as_ref()
                            .map(|p| p.root.clone())
                            .unwrap_or_default();
                        let mut files: Vec<&PathBuf> = self.build_problems.keys().collect();
                        files.sort();
                        for f in files {
                            let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
                            for (l, c, sev, msg) in &self.build_problems[f] {
                                let (icon, tint) = if *sev == 1 {
                                    (ic::X, ERROR)
                                } else {
                                    (ic::WARNING, WARN)
                                };
                                let r = theme::problem_row(
                                    ui,
                                    icon,
                                    tint,
                                    msg,
                                    &format!("{rel}:{l}:{c}"),
                                );
                                if r.clicked()
                                    && let Ok(text) = std::fs::read_to_string(f)
                                {
                                    open = Some((
                                        f.clone(),
                                        lsp::offset(
                                            &text,
                                            l.saturating_sub(1),
                                            c.saturating_sub(1),
                                        ),
                                    ));
                                }
                            }
                        }
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
                        if t.diagnostics.is_empty()
                            && t.lints.is_empty()
                            && self.build_problems.is_empty()
                        {
                            ui.label(
                                RichText::new(format!(
                                    "{}  No problems in this file",
                                    ic::CHECK_CIRCLE
                                ))
                                .color(crate::theme::t().weak),
                            );
                        }
                        for (o, (icon, color), rule, msg) in rows {
                            let detail = if rule.is_empty() {
                                format!("line {}", line_of(o))
                            } else {
                                format!("{rule} · line {}", line_of(o))
                            };
                            if theme::problem_row(ui, icon, color, &msg, &detail).clicked() {
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
                            fixed_row(ui, row_h, |ui| match compiler_location(line) {
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
                                    ui.label(
                                        RichText::new(line)
                                            .monospace()
                                            .color(crate::theme::t().code),
                                    );
                                }
                            });
                        }
                    });
            }
            Bottom::Logcat => self.logcat_panel(ui),
            Bottom::Tests => self.tests_panel(ui),
            Bottom::Performance => self.perf_panel(ui),
            Bottom::Find => {
                let root = self
                    .project
                    .as_ref()
                    .map(|p| p.root.clone())
                    .unwrap_or_default();
                let shown = usage_filter::apply(&self.settings, &root, &self.found);
                if !self.found_title.is_empty() {
                    let mut files: Vec<&Path> = shown
                        .iter()
                        .map(|&i| self.found[i].path.as_path())
                        .collect();
                    files.dedup();
                    let busy = if self.find_rx.is_some() {
                        "  searching…"
                    } else {
                        ""
                    };
                    let title = format!(
                        "{}: {} in {} files{busy}",
                        self.found_title,
                        shown.len(),
                        files.len()
                    );
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).strong());
                        ui.separator();
                        usage_filter::bar(ui, &mut self.settings, self.found.len() - shown.len());
                    });
                }
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, shown.len(), |ui, range| {
                        for f in shown[range].iter().map(|&i| &self.found[i]) {
                            let rel = f
                                .path
                                .strip_prefix(&root)
                                .unwrap_or(&f.path)
                                .display()
                                .to_string();
                            let r = fixed_row(ui, row_h, |ui| {
                                let r = ui.link(
                                    RichText::new(&f.text)
                                        .monospace()
                                        .color(crate::theme::t().text),
                                );
                                ui.label(
                                    RichText::new(format!("{rel}:{}", f.line))
                                        .color(crate::theme::t().weak)
                                        .small(),
                                );
                                r
                            });
                            if r.clicked() {
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
    fn on_exit(&mut self) {
        self.save_session(true);
        if self.settings.autosave {
            self.autosave();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut watch = Watchdog::start();
        watch.stage("background results", || self.poll(&ctx));
        watch.stage("disk changes", || self.poll_disk(&ctx));
        watch.stage("analysis", || self.settle(false));
        watch.stage("git blame", || self.poll_git(&ctx));
        self.poll_usages();
        self.poll_logcat(&ctx);
        self.poll_launch();
        self.poll_capture(&ctx);
        self.rescan_resources(&ctx);
        self.poll_libs();
        self.refresh_changes(&ctx); // git status for tree colours, not only the Pull request tab
        self.watch_focus(&ctx);
        self.save_session(false);
        watch.stage("shortcuts", || self.shortcuts(&ctx));
        self.usages_popup_keys(&ctx);
        self.fix_popup_keys(&ctx);
        if let Some(dropped) =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
            && dropped.is_dir()
        {
            self.open_project(&dropped, &ctx);
        }
        watch.stage("toolbar", || {
            egui::Panel::top("top")
                .frame(
                    egui::Frame::side_top_panel(ui.style())
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| self.top_bar(ui))
        });
        // focus mode, and the welcome screen before a project opens, show no side panels
        let hidden = self.focus || (self.project.is_none() && self.indexing.is_none());
        watch.stage("status bar", || {
            egui::Panel::bottom("status")
                .frame(
                    egui::Frame::side_top_panel(ui.style())
                        .inner_margin(egui::Margin::symmetric(10, 3)),
                )
                .show(ui, |ui| self.status_bar(ui))
        });
        watch.stage("bottom panel", || {
            let mut open = self.show_bottom && !hidden;
            egui::Panel::bottom("bottom")
                .resizable(true)
                .default_size(160.0)
                .show_collapsible(ui, &mut open, |ui| self.bottom_panel(ui));
            if !open && !hidden {
                self.show_bottom = false; // dragged closed
            }
        });
        watch.stage("project tree", || {
            let mut open = self.show_project && !hidden;
            egui::Panel::left("project")
                .resizable(true)
                .default_size(260.0)
                .show_collapsible(ui, &mut open, |ui| self.project_panel(ui));
            if !open && !hidden {
                self.show_project = false;
            }
        });
        watch.stage("compose panel", || {
            let mut open = self.show_right && !hidden;
            egui::Panel::right("compose")
                .resizable(true)
                .default_size(340.0)
                .show_collapsible(ui, &mut open, |ui| {
                    ui.horizontal(|ui| {
                        let mut tabs = theme::Tabs::new(ui, "right");
                        for (r, label) in [
                            (Right::Compose, format!("{}  Compose", ic::CUBE)),
                            (
                                Right::PullRequest,
                                format!("{}  Pull request", ic::GIT_PULL_REQUEST),
                            ),
                        ] {
                            if tabs.tab(ui, self.right == r, label).clicked() {
                                self.right = r;
                            }
                        }
                        tabs.finish(ui);
                    });
                    ui.separator();
                    match self.right {
                        Right::Compose => self.compose_panel(ui),
                        Right::PullRequest => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.git_panel(ui));
                        }
                    }
                });
            if !open && !hidden {
                self.show_right = false;
            }
        });
        let editor_frame = egui::Frame::central_panel(ui.style()).fill(theme::t().editor_bg);
        watch.stage("editor", || {
            egui::CentralPanel::default()
                .frame(editor_frame)
                .show(ui, |ui| self.editor(ui))
        });
        watch.stage("popups", || {
            self.search_window(&ctx);
            self.usages_popup_ui(&ctx);
            self.fix_popup_ui(&ctx);
            self.new_file_ui(&ctx);
            self.translations_window(&ctx);
            self.deep_link_window(&ctx);
            self.refactor_window(&ctx);
            self.commit_window(&ctx);
            self.shortcuts_window(&ctx);
            self.settings_window(&ctx);
            self.capture_window(&ctx);
            self.history_window(&ctx);
            self.toasts_ui(&ctx);
        });
        self.frames += 1;
        let (total, stages, report) = watch.finish(self.frames);
        if self.frames > 3 {
            // the first frames load fonts and lay out everything once
            self.perf.record(stages, total);
        }
        if let Some(report) = report {
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

/// Indent guides and the sticky scope header, both read from indentation, so they work for Kotlin,
/// Java, Gradle scripts and XML alike. Only the lines down to the bottom of the view are looked at.
struct ScopeView<'a> {
    text: &'a str,
    /// Rows can sit lower than `line × row_h` (link rows above declarations), so positions come
    /// from the laid-out rows.
    galley: &'a egui::Galley,
    galley_pos: egui::Pos2,
    gutter: egui::Rect,
    row_h: f32,
    char_w: f32,
    font: &'a FontId,
}

impl ScopeView<'_> {
    /// Paints both; returns the offset of a sticky line that was clicked.
    fn paint(&self, ui: &Ui) -> Option<usize> {
        let clip = ui.clip_rect();
        let row_top = |i: usize| {
            self.galley_pos.y + self.galley.rows.get(i).map_or(f32::INFINITY, |r| r.pos.y)
        };
        let row = |y: f32| {
            self.galley
                .rows
                .partition_point(|r| self.galley_pos.y + r.pos.y + self.row_h <= y)
        };
        let (top, bottom) = (row(clip.top()), row(clip.bottom()) + 1);
        let lines: Vec<&str> = self.text.split('\n').take(bottom + 1).collect();
        let t = theme::t();
        let p = ui.painter();
        // guides: one thin line per indent level, through blank lines inside a block
        let guide = t.raised;
        for i in top..bottom.min(lines.len()) {
            let y = row_top(i);
            for level in 1..indent_level(&lines, i) {
                let x = self.galley_pos.x + ((level - 1) * INDENT) as f32 * self.char_w;
                p.vline(
                    x.round() + 0.5,
                    y..=y + self.row_h,
                    egui::Stroke::new(1.0, guide),
                );
            }
        }
        // sticky header: the lines that open the scopes around the top line, outermost first
        let mut heads = scope_heads(&lines, top);
        heads.retain(|h| *h < top);
        let heads = &heads[heads.len().saturating_sub(3)..];
        if heads.is_empty() {
            return None;
        }
        let area = egui::Rect::from_min_size(
            egui::pos2(self.gutter.left(), clip.top()),
            egui::vec2(
                clip.right() - self.gutter.left(),
                heads.len() as f32 * self.row_h + 2.0,
            ),
        );
        p.rect_filled(area, 0.0, t.editor_bg);
        p.hline(
            area.x_range(),
            area.bottom(),
            egui::Stroke::new(1.0, t.raised),
        );
        let mut clicked = None;
        for (i, &h) in heads.iter().enumerate() {
            let y = clip.top() + i as f32 * self.row_h;
            let r = egui::Rect::from_min_size(
                egui::pos2(area.left(), y),
                egui::vec2(area.width(), self.row_h),
            );
            let resp = ui.interact(r, ui.scope_id().with(("sticky", i)), egui::Sense::click());
            if resp.hovered() {
                p.rect_filled(r, 0.0, t.current_line);
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            p.text(
                egui::pos2(self.gutter.right() - 12.0, y),
                Align2::RIGHT_TOP,
                (h + 1).to_string(),
                self.font.clone(),
                t.gutter,
            );
            p.text(
                egui::pos2(self.galley_pos.x, y),
                Align2::LEFT_TOP,
                lines[h],
                self.font.clone(),
                t.code,
            );
            if resp.clicked() {
                clicked = Some(lines[..h].iter().map(|l| l.len() + 1).sum::<usize>());
            }
        }
        clicked
    }
}

const INDENT: usize = 4;

/// Leading spaces of a line (a tab counts as one indent), or None when the line is blank.
fn indent_of(line: &str) -> Option<usize> {
    let mut n = 0;
    for c in line.chars() {
        match c {
            ' ' => n += 1,
            '\t' => n += INDENT,
            _ => return Some(n),
        }
    }
    None
}

/// How many guides line `i` shows: its indent levels, plus one for the guide at column 0.
/// A blank line takes the smaller indent of the lines around it, so guides run through gaps.
fn indent_level(lines: &[&str], i: usize) -> usize {
    let near = |it: &mut dyn Iterator<Item = &&str>| it.take(200).find_map(|l| indent_of(l));
    let n = indent_of(lines[i]).unwrap_or_else(|| {
        let up = near(&mut lines[..i].iter().rev()).unwrap_or(0);
        let down = near(&mut lines[i + 1..].iter()).unwrap_or(0);
        up.min(down)
    });
    n / INDENT + 1
}

/// The lines that open the scopes around line `at`: walking up, each non-blank line with a smaller
/// indent than the last one found, skipping closers, comments and annotations. Outermost first.
fn scope_heads(lines: &[&str], at: usize) -> Vec<usize> {
    // a blank line belongs to the deeper of its neighbours (the inside of the block it splits)
    let near = |it: &mut dyn Iterator<Item = &&str>| it.take(200).find_map(|l| indent_of(l));
    let at = at.min(lines.len().saturating_sub(1));
    let mut want = indent_of(lines.get(at).unwrap_or(&"")).unwrap_or_else(|| {
        let up = near(&mut lines[..at].iter().rev()).unwrap_or(0);
        let down = near(&mut lines[at + 1..].iter()).unwrap_or(0);
        up.max(down)
    });
    let mut heads = Vec::new();
    for i in (0..at).rev() {
        if want == 0 {
            break;
        }
        let Some(n) = indent_of(lines[i]) else {
            continue;
        };
        let code = lines[i].trim_start();
        let closer = code.starts_with("</") || code.starts_with(['}', ')', ']', '/', '*', '@']);
        if n < want && !closer {
            heads.push(i);
            want = n;
        }
    }
    heads.reverse();
    heads
}

const RUN_GREEN: Color32 = Color32::from_rgb(0x5F, 0xB8, 0x65);

/// The toolbar's search field: looks like a text box, opens Search everywhere when clicked.
fn search_box(ui: &mut Ui, width: f32) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Search"));
    let t = theme::t();
    let hover = theme::ease(ui.ctx(), r.id.with("hover"), r.hovered() as u8 as f32, 0.12);
    let p = ui.painter();
    p.rect(
        rect,
        egui::CornerRadius::same(7),
        t.editor_bg,
        egui::Stroke::new(1.0, t.raised.lerp_to_gamma(t.accent, hover * 0.6)),
        egui::StrokeKind::Inside,
    );
    let font = FontId::proportional(13.0);
    let mid = rect.left_center() + egui::vec2(10.0, 0.0);
    p.text(
        mid,
        Align2::LEFT_CENTER,
        format!("{}  Search", ic::MAGNIFYING_GLASS),
        font.clone(),
        t.weak,
    );
    p.text(
        rect.right_center() - egui::vec2(10.0, 0.0),
        Align2::RIGHT_CENTER,
        "⇧⇧",
        font,
        t.gutter,
    );
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

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
/// A Kotlin compiler line: `e: file:///path/File.kt:12:5 message` → (path, line, column, severity, message).
fn compiler_problem(line: &str) -> Option<(PathBuf, u32, u32, u8, String)> {
    let (p, l, c) = compiler_location(line)?;
    let msg = line
        .split_once(&format!(":{l}:{c} "))
        .map_or("", |(_, m)| m)
        .trim()
        .to_string();
    Some((p, l, c, if line.starts_with("e:") { 1 } else { 2 }, msg))
}

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

/// For `--bench`: builds the Project tree (Android view, as the app does), returns its top-level rows.
pub fn bench_tree(p: &Project) -> usize {
    tree::build(p, tree::View::Android).len()
}

/// One row of a `show_rows` list, exactly `h` high (plus the spacing `show_rows` adds itself).
/// `show_rows` places rows by that height alone, so a taller row (a link's minimum size) made the
/// list jump while scrolling or dragging. Wider content still widens the list for side scrolling.
fn fixed_row<R>(ui: &mut Ui, h: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), h));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    child.spacing_mut().interact_size.y = 0.0;
    let r = add(&mut child);
    let width = child.min_rect().width().max(rect.width());
    ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    r
}

/// Icon and tint for a file in the tree and on tabs.
fn file_icon(path: &Path) -> (&'static str, Color32) {
    match path.extension().and_then(|e| e.to_str()) {
        Some("kt") => (ic::FILE_CODE, Color32::from_rgb(0xA9, 0x7B, 0xFF)),
        Some("java") => (ic::FILE_CODE, Color32::from_rgb(0xE7, 0x8A, 0x4E)),
        Some("xml") => (ic::FILE_TEXT, Color32::from_rgb(0x5F, 0xB8, 0x65)),
        Some("kts" | "gradle") => (ic::WRENCH, Color32::from_rgb(0x58, 0x9D, 0xF6)),
        _ => (ic::FILE, crate::theme::t().weak),
    }
}

/// One editor tab: file icon, name (● when unsaved), close button, accent line when active.
/// Returns (clicked, close clicked). Middle-click closes too.
fn tab_widget(ui: &mut Ui, t: &Tab, active: bool) -> (bool, bool) {
    let (icon, tint) = assist::kind_icon(&t.path, is_code(&t.path).then_some(&t.syms));
    let name = t
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fill = if active {
        crate::theme::t().editor_bg
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
                let color = if active {
                    crate::theme::t().text
                } else {
                    crate::theme::t().weak
                };
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
                        egui::Button::new(
                            RichText::new(ic::X)
                                .size(11.0)
                                .color(crate::theme::t().weak),
                        )
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
            egui::Stroke::new(2.0, crate::theme::t().accent),
        );
    }
    (r.clicked() && !close, close || r.middle_clicked())
}

/// This app again, opening `dir`, in its own process group so it outlives this window.
fn window_command(dir: &Path) -> std::io::Result<std::process::Command> {
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.arg(dir);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    Ok(c)
}

fn config_file() -> Option<PathBuf> {
    setting_file("last-project")
}

/// `~/.config/droid-lite` (or `$DROID_LITE_CONFIG_DIR`). Tests never touch the user's folder:
/// without the variable they get no config folder at all.
/// Serialises tests that change process-wide state (environment, theme) with the e2e tests.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    e2e::SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DROID_LITE_CONFIG_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/droid-lite"))
}

fn setting_file(name: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(name))
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
    let list = with_recent(recent_projects(), root);
    save_setting(
        "recent-projects",
        &list
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Projects opened before, newest first (folders that still exist), for the welcome screen.
fn recent_projects() -> Vec<PathBuf> {
    load_setting("recent-projects")
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

/// `root` moved to the front, without a duplicate, at most 8.
fn with_recent(mut list: Vec<PathBuf>, root: &Path) -> Vec<PathBuf> {
    list.retain(|p| p != root);
    list.insert(0, root.to_path_buf());
    list.truncate(8);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_heads_follow_indentation_and_skip_closers_and_annotations() {
        let src = "class Home {\n    @Composable\n    fun Screen() {\n        Column {\n\n            Text()\n        }\n    }\n}";
        let lines: Vec<&str> = src.split('\n').collect();
        assert_eq!(
            scope_heads(&lines, 5),
            vec![0, 2, 3],
            "class, fun, Column for Text()"
        );
        assert_eq!(
            scope_heads(&lines, 4),
            vec![0, 2, 3],
            "a blank line belongs to the block it splits"
        );
        assert_eq!(
            scope_heads(&lines, 7),
            vec![0],
            "the fun's closing brace sits in the class"
        );
        assert_eq!(scope_heads(&lines, 0), Vec::<usize>::new());
        let xml = "<LinearLayout>\n    <TextView\n        android:text=\"a\" />\n</LinearLayout>";
        let lines: Vec<&str> = xml.split('\n').collect();
        assert_eq!(
            scope_heads(&lines, 2),
            vec![0, 1],
            "XML tags open scopes too"
        );
        assert_eq!(indent_level(&lines, 2), 3);
        let gap = ["a {", "        x", "", "    y", "}"];
        assert_eq!(
            indent_level(&gap, 2),
            2,
            "a blank line takes the smaller neighbour"
        );
    }

    #[test]
    fn recent_projects_move_to_the_front_without_duplicates_and_keep_eight() {
        let p = |s: &str| PathBuf::from(s);
        let list: Vec<PathBuf> = (0..8).map(|i| p(&format!("/p{i}"))).collect();
        let out = with_recent(list, &p("/p3"));
        assert_eq!(out[0], p("/p3"));
        assert_eq!(out.len(), 8);
        assert_eq!(out.iter().filter(|x| **x == p("/p3")).count(), 1);
        let out = with_recent(out, &p("/new"));
        assert_eq!((out[0].clone(), out.len()), (p("/new"), 8));
        assert!(!out.contains(&p("/p7")), "the oldest drops off");
    }

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

    #[test]
    fn new_window_runs_this_app_on_the_picked_project() {
        let c = window_command(Path::new("/some/project")).unwrap();
        assert_eq!(
            Path::new(c.get_program()),
            std::env::current_exe().unwrap().as_path()
        );
        assert_eq!(c.get_args().collect::<Vec<_>>(), ["/some/project"]);
        for km in [Keymap::Eclipse, Keymap::AndroidStudio] {
            assert_eq!(keymap::text(km, Action::NewWindow), "⇧⌘N");
        }
    }
}
