//! The IDE window: project tree, editor tabs, search popups, completion, Compose panel, Gradle console.

mod disk;
mod refactor_ui;

use crate::compose::{self, Lint, Severity, Stability, Verdict};
use crate::gradle::{self, PreviewTool};
use crate::highlight::{self, ERROR, INFO, TEXT, WARN};
use crate::index::{self, Hit, Index};
use crate::kotlin::{Decl, FileSyms, scan};
use crate::lsp::{self, Lsp};
use crate::project::{Project, is_code};
use egui::text::{CCursor, CCursorRange};
use egui::{
    Align, Align2, Color32, FontId, Key, KeyboardShortcut, Modifiers, RichText, TextEdit, Ui,
};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Instant, SystemTime};

const FONT_SIZE: f32 = 13.0;

struct Tab {
    path: PathBuf,
    text: String,
    saved: String,
    syms: FileSyms,
    lints: Vec<Lint>,
    diagnostics: Vec<(usize, u8, String)>,
    cache: Option<(u64, egui::text::LayoutJob)>,
    /// Byte offset to put the cursor at (and scroll to) on the next frame.
    goto: Option<usize>,
    cursor: usize,
    /// Selected byte range (empty when nothing is selected).
    selection: std::ops::Range<usize>,
    /// Changed on disk while this tab had unsaved edits.
    disk_changed: bool,
    deleted: bool,
}

impl Tab {
    fn dirty(&self) -> bool {
        self.text != self.saved
    }
    fn title(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if self.dirty() {
            format!("● {name}")
        } else {
            name
        }
    }
    fn reanalyse(&mut self) {
        self.syms = scan(&self.text);
        self.lints = if is_code(&self.path) {
            compose::lint(&self.text, &self.syms)
        } else {
            Vec::new()
        };
        self.cache = None;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SearchKind {
    Class,
    File,
    Symbol,
    Everywhere,
    Recent,
    Structure,
    FindInFiles,
}

impl SearchKind {
    fn title(self) -> &'static str {
        match self {
            SearchKind::Class => "Go to class (⌘O)",
            SearchKind::File => "Go to file (⇧⌘O)",
            SearchKind::Symbol => "Go to symbol (⌥⌘O)",
            SearchKind::Everywhere => "Search everywhere (⇧⇧)",
            SearchKind::Recent => "Recent files (⌘E)",
            SearchKind::Structure => "File structure (⌘F12)",
            SearchKind::FindInFiles => "Find in files (⇧⌘F), press Enter",
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
    indexing: Option<Receiver<Index>>,
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
    /// Liquid Glass is behind the window.
    glass: bool,
    /// Content runs under the title bar: leave room for the window buttons.
    fullsize: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>, glass_wanted: bool) -> App {
        let glass = glass_wanted && crate::glass::apply(cc);
        if glass_wanted && !glass {
            eprintln!(
                "droid-lite: Liquid Glass not available (macOS 26+ needed); using solid colours"
            );
        }
        cc.egui_ctx.set_visuals(crate::glass::visuals(glass));
        let mut app = App {
            project: None,
            tree: Vec::new(),
            index: Index::default(),
            indexing: None,
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
            glass,
            fullsize: glass_wanted,
        };
        if let Some(dir) = open.or_else(last_project) {
            app.open_project(&dir, &cc.egui_ctx);
        }
        if let Some(f) = std::env::var_os("DROID_LITE_OPEN") {
            app.open_file(Path::new(&f), None);
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

    fn open_project(&mut self, dir: &Path, ctx: &egui::Context) {
        let project = Project::open(dir);
        remember_project(&project.root);
        self.tree = build_tree(&project);
        self.tabs.clear();
        self.history.clear();
        self.recent.clear();
        self.index = Index::default();
        let (tx, rx) = channel();
        let files = project.files.clone();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Index::build(&files));
            c.request_repaint();
        });
        self.indexing = Some(rx);
        self.reload_stability(&project, ctx);
        self.lsp = Lsp::start(&project.root, ctx.clone());
        self.watcher = crate::watch::Watcher::start(&project.root, ctx.clone());
        self.refactor = None;
        self.undo = None;
        self.status = format!(
            "Opened {} ({} modules, {} files)",
            project.name,
            project.modules.len(),
            project.files.len()
        );
        self.project = Some(project);
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
            self.history.push((t.path.clone(), t.cursor));
            self.forward.clear();
        }
        self.show_file(path, offset);
    }

    /// Opens or focuses a tab without touching the back/forward history.
    fn show_file(&mut self, path: &Path, offset: Option<usize>) {
        self.completion = None;
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
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
                goto: None,
                cursor: 0,
                selection: 0..0,
                disk_changed: false,
                deleted: false,
            };
            tab.reanalyse();
            if let Some(l) = self.lsp.as_mut().filter(|_| is_code(path)) {
                l.did_open(path, &tab.text);
            }
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
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
                self.history.push((t.path.clone(), t.cursor));
            }
            self.show_file(&p, Some(o));
        }
    }

    fn save(&mut self) {
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
            self.tabs.remove(i);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
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

    fn find_usages(&mut self, word: String, whole_word: bool, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let files: Vec<PathBuf> = p.files.clone();
        let (tx, rx) = channel();
        let c = ctx.clone();
        self.status = format!("Searching for `{word}`…");
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for f in files {
                let Ok(text) = std::fs::read_to_string(&f) else {
                    continue;
                };
                for (o, _) in text.match_indices(&word) {
                    let (a, b) = word_bounds(&text, o);
                    if whole_word && (a != o || b != o + word.len()) {
                        continue;
                    }
                    let ls = text[..o].rfind('\n').map_or(0, |i| i + 1);
                    let le = text[o..].find('\n').map_or(text.len(), |i| o + i);
                    out.push(FoundLine {
                        path: f.clone(),
                        offset: o,
                        line: text[..o].matches('\n').count() + 1,
                        text: text[ls..le].trim().chars().take(160).collect(),
                    });
                    if out.len() >= 2000 {
                        break;
                    }
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
            && let Ok(idx) = rx.try_recv()
        {
            self.index = idx;
            for t in &self.tabs {
                self.index.update(&t.path, &t.text);
            }
            self.indexing = None;
            self.status = format!("Indexed {} symbols", self.index.symbol_count());
        }
        if let Some(rx) = &self.stability_rx
            && let Ok(s) = rx.try_recv()
        {
            self.stability = s;
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
                self.textures.clear();
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
                lsp::Event::Diagnostics(path, diags) => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.diagnostics = diags
                            .into_iter()
                            .map(|(l, c, s, m)| (lsp::offset(&t.text, l, c), s, m))
                            .collect();
                        t.cache = None;
                    }
                }
            }
        }
    }

    // ---------- keyboard ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let pressed = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)))
        };
        let open = |kind| {
            Some(Search {
                kind,
                query: String::new(),
                hits: Vec::new(),
                selected: 0,
                stale: true,
            })
        };
        if pressed(cmd | Modifiers::SHIFT, Key::O) {
            self.search = open(SearchKind::File);
        } else if pressed(cmd | Modifiers::ALT, Key::O) {
            self.search = open(SearchKind::Symbol);
        } else if pressed(cmd, Key::O) {
            self.search = open(SearchKind::Class);
        } else if pressed(cmd, Key::E) {
            self.search = open(SearchKind::Recent);
        } else if pressed(cmd, Key::F12) {
            self.search = open(SearchKind::Structure);
        } else if pressed(cmd | Modifiers::SHIFT, Key::F) {
            self.search = open(SearchKind::FindInFiles);
        }
        if pressed(Modifiers::SHIFT, Key::F6) {
            self.start_rename();
        }
        if pressed(Modifiers::CTRL | Modifiers::ALT, Key::O) {
            self.optimize_imports();
        }
        if pressed(cmd | Modifiers::ALT, Key::V) {
            self.start_extract();
        }
        if pressed(cmd, Key::B) {
            self.goto_definition();
        }
        if pressed(cmd, Key::OpenBracket) {
            self.back();
        }
        if pressed(cmd, Key::CloseBracket) {
            self.fwd();
        }
        if pressed(cmd, Key::S) {
            self.save();
        }
        if pressed(cmd, Key::W) {
            self.close_tab(self.active);
        }
        if pressed(Modifiers::ALT, Key::F7)
            && let Some((w, _)) = self.word_at_cursor()
        {
            self.find_usages(w, true, ctx);
        }
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
            if s.kind == SearchKind::FindInFiles {
                if !s.query.is_empty() {
                    self.find_usages(s.query.clone(), false, ctx);
                }
                close = true;
            } else if let Some(h) = s.hits.get(s.selected) {
                chosen = Some((h.path.clone(), h.offset));
            }
        }
        egui::Window::new(s.kind.title())
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 60.0])
            .default_width(640.0)
            .show(ctx, |ui| {
                let r = ui.add(
                    TextEdit::singleline(&mut s.query)
                        .desired_width(f32::INFINITY)
                        .hint_text("Type to search; camel humps work: HSV → HomeScreenViewModel"),
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
        let files = |q: &str| {
            index::rank(
                q,
                self.project
                    .iter()
                    .flat_map(|p| p.files.iter())
                    .map(file_hit),
                |h| h.name.clone(),
                60,
            )
        };
        match kind {
            SearchKind::Class => self.index.search(q, true, 60),
            SearchKind::Symbol => self.index.search(q, false, 60),
            SearchKind::File => files(q),
            SearchKind::Recent => {
                index::rank(q, self.recent.iter().map(file_hit), |h| h.name.clone(), 60)
            }
            SearchKind::Everywhere => {
                let mut v = self.index.search(q, false, 30);
                v.extend(files(q).into_iter().take(15));
                v.sort_by_key(|h| std::cmp::Reverse(h.score));
                v
            }
            SearchKind::Structure => {
                let Some(t) = self.tabs.get(self.active) else {
                    return Vec::new();
                };
                let items = t.syms.decls.iter().map(|d| Hit {
                    name: match &d.container {
                        Some(c) => format!("{c}.{}", d.name),
                        None => d.name.clone(),
                    },
                    detail: index::describe(&t.syms, d),
                    path: t.path.clone(),
                    offset: d.offset,
                    score: 0,
                });
                index::rank(q, items, |h| h.name.clone(), 200)
            }
            SearchKind::FindInFiles => Vec::new(),
        }
    }

    // ---------- panels ----------

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("📂 Open project…").clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("Open an Android project folder")
                    .pick_folder()
            {
                self.open_project(&dir, &ui.ctx().clone());
            }
            if let Some(p) = &self.project {
                ui.label(RichText::new(&p.name).strong());
            }
            ui.separator();
            ui.add_enabled(!self.history.is_empty(), egui::Button::new("◀"))
                .on_hover_text("Back (⌘[)")
                .clicked()
                .then(|| self.back());
            ui.add_enabled(!self.forward.is_empty(), egui::Button::new("▶"))
                .on_hover_text("Forward (⌘])")
                .clicked()
                .then(|| self.fwd());
            if ui.button("🔍 Class").on_hover_text("⌘O").clicked() {
                self.search = Some(Search {
                    kind: SearchKind::Class,
                    query: String::new(),
                    hits: Vec::new(),
                    selected: 0,
                    stale: true,
                });
            }
            ui.separator();
            self.refactor_menu(ui);
            ui.separator();
            ui.label("Gradle:");
            ui.add(TextEdit::singleline(&mut self.gradle_task).desired_width(220.0));
            let running = self.gradle.is_some();
            if ui
                .add_enabled(
                    self.project.is_some() && !running,
                    egui::Button::new("▶ Run"),
                )
                .clicked()
            {
                let t = self.gradle_task.clone();
                self.run_gradle(t, &ui.ctx().clone());
            }
            for t in ["assembleDebug", "installDebug"] {
                if ui
                    .add_enabled(
                        self.project.is_some() && !running,
                        egui::Button::new(t).small(),
                    )
                    .clicked()
                {
                    self.gradle_task = t.into();
                    self.run_gradle(t.into(), &ui.ctx().clone());
                }
            }
            if running
                && ui.button("■ Stop").clicked()
                && let Some(r) = self.gradle.as_mut()
            {
                r.stop();
            }
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if self.indexing.is_some() {
                ui.spinner();
                ui.label("Indexing…");
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
            ui.label(&self.status);
        });
    }

    fn project_panel(&mut self, ui: &mut Ui) {
        if self.project.is_none() {
            ui.label("Open an Android project folder (the one with settings.gradle).");
            return;
        }
        let mut open = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (module, node) in &self.tree {
                    egui::CollapsingHeader::new(RichText::new(format!("📦 {module}")).strong())
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
                ui.label(RichText::new("⌘O class   ⇧⌘O file   ⇧⇧ everywhere   ⌘E recent   ⌘B go to declaration   ⌥F7 usages").weak());
            });
            return;
        }
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            for (i, t) in self.tabs.iter().enumerate() {
                let r = ui
                    .selectable_label(i == self.active, t.title())
                    .on_hover_text(t.path.display().to_string());
                if r.clicked() {
                    self.active = i;
                    self.completion = None;
                }
                if r.middle_clicked() || ui.small_button("×").clicked() {
                    close = Some(i);
                }
            }
        });
        if let Some(i) = close {
            self.close_tab(i);
            return;
        }
        self.disk_banner(ui);
        if self.tabs[self.active].dirty() {
            ui.horizontal(|ui| {
                if ui.small_button("Save ⌘S").clicked() {
                    self.save();
                }
                if ui.small_button("Revert").clicked() {
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
        let ctrl_space = ui
            .input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::Space)));
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
        let digits = lines.to_string().len();
        let numbers: String = (1..=lines).map(|n| format!("{n:>digits$}\n")).collect();
        let Tab {
            text,
            cache,
            goto,
            cursor,
            selection,
            lints,
            diagnostics,
            ..
        } = tab;
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
            let s = buf.as_str();
            let key = hash(s) ^ marks.len() as u64;
            if cache.as_ref().is_none_or(|(k, _)| *k != key) {
                *cache = Some((key, highlight::job(s, font.clone(), &marks)));
            }
            ui.fonts_mut(|f| f.layout_job(cache.as_ref().unwrap().1.clone()))
        };
        let mut changed = false;
        let mut cmd_click = false;
        let mut popup_pos = None;
        egui::ScrollArea::both()
            .id_salt(id)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        RichText::new(&numbers)
                            .font(FontId::monospace(FONT_SIZE))
                            .color(Color32::from_gray(90)),
                    );
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
            tab.reanalyse();
            self.index.update(&path, &self.tabs[self.active].text);
            if let Some(l) = self.lsp.as_mut() {
                l.did_change(&path, &self.tabs[self.active].text);
            }
            self.update_completion(false);
        }
        if ctrl_space {
            self.update_completion(true);
        }
        if cmd_click {
            self.goto_definition();
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
            egui::CollapsingHeader::new(RichText::new(format!("Composables ({})", composables.len())).heading()).default_open(composables.len() <= 8).show(ui, |ui| {
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
            ui.separator();
            ui.heading(format!("UI tree: {}", d.name));
            let is_ui = |n: &str| compose::is_ui_call(n, &self.index);
            let tree = compose::tree(&tab.text, d, &is_ui);
            if tree.is_empty() {
                ui.label(RichText::new("No UI calls found.").weak());
            }
            for n in &tree {
                node_ui(ui, n, &mut jump);
            }

            ui.separator();
            ui.heading("Stability");
            let (st, guessed) = match self.stability.get(&d.name) {
                Some(s) => (s.clone(), false),
                None => (compose::guess(d, &self.index), true),
            };
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
            ui.heading("Previews");
            let previews: Vec<String> = tab.syms.decls.iter().filter(|d| d.is_preview()).map(|d| d.name.clone()).collect();
            if previews.is_empty() {
                ui.label(RichText::new("No @Preview functions in this file.").weak());
            }
            for p in &previews {
                if ui.link(RichText::new(p).monospace()).clicked() {
                    jump = tab.syms.decls.iter().find(|d| &d.name == p).map(|d| d.offset);
                }
            }
            let module = self.project.as_ref().and_then(|pr| pr.module_of(&tab.path)).cloned();
            match module.as_ref().map(|m| (m, PreviewTool::detect(m))) {
                Some((m, Some(tool))) => {
                    ui.horizontal(|ui| {
                        render = ui.add_enabled(self.gradle.is_none(), egui::Button::new("🖼 Render previews")).on_hover_text(tool.task(m)).clicked();
                        ui.label(RichText::new(format!("{tool:?}")).weak());
                    });
                    for (name, path) in tool.images(m, &previews) {
                        ui.label(RichText::new(&name).small());
                        if let Some(tex) = texture(ui.ctx(), &mut self.textures, &path) {
                            let size = tex.size_vec2();
                            let w = ui.available_width().min(size.x);
                            ui.image((tex.id(), egui::vec2(w, size.y * w / size.x)));
                        }
                    }
                }
                Some((_, None)) if !previews.is_empty() => {
                    ui.label(RichText::new(gradle::SETUP_HELP).small());
                }
                _ => {}
            }

            ui.separator();
            ui.heading("Compose checks");
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
                format!("Problems ({problems})"),
            );
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Build,
                if self.gradle.is_some() {
                    "Build ⏳"
                } else {
                    "Build"
                },
            );
            ui.selectable_value(
                &mut self.bottom,
                Bottom::Find,
                format!("Find ({})", self.found.len()),
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
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(self.bottom == Bottom::Build)
            .show(ui, |ui| match self.bottom {
                Bottom::Problems => {
                    if let Some(t) = self.tabs.get(self.active) {
                        for (o, s, m) in &t.diagnostics {
                            let line = t.text[..(*o).min(t.text.len())].matches('\n').count() + 1;
                            if ui
                                .link(RichText::new(format!("{line}: {m}")).color(if *s == 1 {
                                    ERROR
                                } else {
                                    WARN
                                }))
                                .clicked()
                            {
                                open = Some((t.path.clone(), *o));
                            }
                        }
                        for l in &t.lints {
                            let line = t.text[..l.offset].matches('\n').count() + 1;
                            if ui
                                .link(
                                    RichText::new(format!("{line}: [{}] {}", l.rule, l.message))
                                        .color(if l.severity == Severity::Warning {
                                            WARN
                                        } else {
                                            INFO
                                        }),
                                )
                                .clicked()
                            {
                                open = Some((t.path.clone(), l.offset));
                            }
                        }
                    }
                }
                Bottom::Build => {
                    for line in &self.console {
                        if let Some((p, l, c)) = compiler_location(line) {
                            if ui
                                .link(
                                    RichText::new(line)
                                        .monospace()
                                        .color(if line.starts_with("e:") { ERROR } else { WARN }),
                                )
                                .clicked()
                                && let Ok(text) = std::fs::read_to_string(&p)
                            {
                                open = Some((
                                    p,
                                    lsp::offset(&text, l.saturating_sub(1), c.saturating_sub(1)),
                                ));
                            }
                        } else {
                            ui.label(RichText::new(line).monospace().color(TEXT));
                        }
                    }
                }
                Bottom::Find => {
                    let root = self
                        .project
                        .as_ref()
                        .map(|p| p.root.clone())
                        .unwrap_or_default();
                    for f in &self.found {
                        let rel = f.path.strip_prefix(&root).unwrap_or(&f.path).display();
                        if ui
                            .link(
                                RichText::new(format!("{rel}:{}  {}", f.line, f.text)).monospace(),
                            )
                            .clicked()
                        {
                            open = Some((f.path.clone(), f.offset));
                        }
                    }
                }
            });
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
        self.poll(&ctx);
        self.poll_disk(&ctx);
        self.shortcuts(&ctx);
        if let Some(dropped) =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
            && dropped.is_dir()
        {
            self.open_project(&dropped, &ctx);
        }
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
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("bottom")
            .resizable(true)
            .default_size(180.0)
            .show(ui, |ui| self.bottom_panel(ui));
        egui::Panel::left("project")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| self.project_panel(ui));
        egui::Panel::right("compose")
            .resizable(true)
            .default_size(340.0)
            .show(ui, |ui| self.compose_panel(ui));
        let editor_frame =
            egui::Frame::central_panel(ui.style()).fill(crate::glass::editor_fill(self.glass));
        egui::CentralPanel::default()
            .frame(editor_frame)
            .show(ui, |ui| self.editor(ui));
        self.search_window(&ctx);
        self.refactor_window(&ctx);
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
        egui::CollapsingHeader::new(format!("📁 {label}"))
            .id_salt(&key)
            .show(ui, |ui| dir_ui(ui, c, &key, open));
    }
    for f in &node.files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let icon = match f.extension().and_then(|e| e.to_str()) {
            Some("kt") => "🟣",
            Some("java") => "☕",
            Some("xml") => "📄",
            Some("kts" | "gradle") => "🐘",
            _ => "·",
        };
        if ui
            .selectable_label(false, format!("{icon} {name}"))
            .clicked()
        {
            *open = Some(f.clone());
        }
    }
}

fn config_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/droid-lite/last-project"))
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
