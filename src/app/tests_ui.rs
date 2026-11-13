//! The Tests tab: each plugin's cases as a tree with ▶, live pass / fail, the plugin's live
//! state, its views (failures, …), actions and links. A ▶ also shows in the editor's gutter on
//! lines that a case names. The plugin contract is in docs/PLUGINS.md.

use super::{App, Bottom, config_dir};
use crate::gradle::{Line, Run};
use crate::icons as ic;
use crate::plugins::{self, Case, Entry, Live, Manifest, Status};
use egui::{Align, Color32, Layout, RichText, Ui};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

/// Log lines kept per plugin.
const KEEP_LOG: usize = 5000;

type Fetch = Receiver<Result<String, String>>;

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
    errors: Vec<String>,
    active: usize,
    filter: String,
    /// Marks a gutter ▶ picked when its line has several cases; the list shows only these.
    pub(super) only: Vec<usize>,
    pub(super) marks: Vec<Mark>,
}

#[derive(Default)]
pub(super) struct Plugin {
    m: Manifest,
    cases: Vec<Case>,
    loading: Option<Fetch>,
    error: Option<String>,
    pub(super) status: HashMap<String, Status>,
    /// The running case or action: (its id, or "" for an action; title; process).
    pub(super) run: Option<(String, String, Run)>,
    pub(super) results: Vec<Entry>,
    pub(super) log: Vec<String>,
    pub(super) watch: Option<Run>,
    pub(super) live: Option<Live>,
    /// None: the cases; Some(i): view i.
    view: Option<usize>,
    pub(super) rows: Vec<Entry>,
    rows_rx: Option<Fetch>,
    rows_error: Option<String>,
}

impl Plugin {
    fn new(m: Manifest) -> Plugin {
        Plugin {
            m,
            ..Default::default()
        }
    }

    fn log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > KEEP_LOG {
            self.log.drain(..KEEP_LOG / 5);
        }
    }
}

/// Runs `cmd` to the end on a thread: its stdout, or an error with the end of its stderr.
fn fetch(mut cmd: std::process::Command, ctx: &egui::Context) -> Fetch {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let r = match cmd.output() {
            Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let tail: Vec<&str> = err.lines().rev().take(3).collect();
                Err(format!(
                    "exit {}: {}",
                    o.status.code().unwrap_or(-1),
                    tail.into_iter().rev().collect::<Vec<_>>().join(" ")
                ))
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(r);
        ctx.request_repaint();
    });
    rx
}

/// SIGTERM to the run's process group, so the tools it started stop too.
fn stop(run: &mut Run) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &format!("-{}", run.id())])
            .status();
    }
    #[cfg(not(unix))]
    run.stop();
}

/// Opens a link in the browser named in the `browser` setting file, else the system default.
pub(super) fn open_link(url: &str) {
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
}

impl Tests {
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
            Ok(name) => {
                self.load_plugins(ctx);
                if let Some(i) = self.tests.plugins.iter().position(|p| p.m.name == name) {
                    self.tests.active = i;
                }
                self.bottom = Bottom::Tests;
                self.status = format!("Added the plugin {name}");
            }
            Err(e) => self.status = e,
        }
    }

    fn plugin_dirs(&self) -> Vec<PathBuf> {
        self.plugins_home()
            .into_iter()
            .chain(
                self.project
                    .as_ref()
                    .map(|p| p.root.join(".droid-lite/plugins")),
            )
            .collect()
    }

    /// Finds the plugins (config folder and the project's `.droid-lite/plugins`) and lists their
    /// cases in the background. Running cases of the old set stop (Drop).
    pub(super) fn load_plugins(&mut self, ctx: &egui::Context) {
        let mut t = Tests::default();
        for m in plugins::discover(&self.plugin_dirs()) {
            match m {
                Ok(m) => t.plugins.push(Plugin::new(m)),
                Err(e) => t.errors.push(e),
            }
        }
        self.tests = t;
        for i in 0..self.tests.plugins.len() {
            self.list_cases(i, ctx);
        }
    }

    fn list_cases(&mut self, i: usize, ctx: &egui::Context) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let p = &mut self.tests.plugins[i];
        if let Some(list) = &p.m.list {
            p.loading = Some(fetch(p.m.command(list, root.as_deref(), ""), ctx));
        }
    }

    fn load_view(&mut self, i: usize, v: usize, ctx: &egui::Context) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let p = &mut self.tests.plugins[i];
        let cmd = p.m.command(&p.m.views[v].argv, root.as_deref(), "");
        p.rows_rx = Some(fetch(cmd, ctx));
    }

    /// Runs case `id` of plugin `i`, or action `action` when given.
    pub(super) fn run_case(
        &mut self,
        i: usize,
        id: &str,
        action: Option<usize>,
        ctx: &egui::Context,
    ) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let Some(p) = self.tests.plugins.get_mut(i) else {
            return;
        };
        if p.run.is_some() {
            return;
        }
        let (argv, title) = match action {
            Some(a) => (p.m.actions[a].argv.clone(), p.m.actions[a].name.clone()),
            None => match &p.m.run {
                Some(r) => (r.clone(), id.to_owned()),
                None => return,
            },
        };
        p.results.clear();
        p.log.clear();
        match Run::spawn(p.m.command(&argv, root.as_deref(), id), ctx.clone()) {
            Ok(run) => {
                if action.is_none() {
                    p.status.insert(id.to_owned(), Status::Running);
                }
                let key = if action.is_none() { id } else { "" };
                p.run = Some((key.to_owned(), title.clone(), run));
                self.status = format!("{}: running {title}", p.m.name);
            }
            Err(e) => p.log(format!("Could not start {}: {e}", argv[0])),
        }
        self.tests.active = i;
        self.bottom = Bottom::Tests;
    }

    /// Reads every plugin's finished lists and new run, watch and view output. Called each frame.
    pub(super) fn poll_tests(&mut self) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let mut lists_changed = false;
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
                        Line::Done(_) => p.watch = None,
                    }
                }
            }
            let Some((id, title, run)) = p.run.as_mut() else {
                continue;
            };
            let (id, title) = (id.clone(), title.clone());
            let mut ended = None;
            let mut events = Vec::new();
            for l in run.poll() {
                match l {
                    Line::Out(s) => events.push(plugins::parse_event(&s, root.as_deref())),
                    Line::Done(code) => ended = Some(code),
                }
            }
            for e in events {
                match e {
                    plugins::Event::Log(s) => p.log(s),
                    plugins::Event::Result(e) => {
                        if let (Some(c), Some(s)) = (&e.case, e.status) {
                            p.status.insert(c.clone(), s);
                        }
                        if e.status != Some(Status::Running) {
                            p.results.push(e);
                        }
                    }
                }
            }
            if let Some(code) = ended {
                p.run = None;
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
                p.log(line.clone());
                self.status = if failed > 0 {
                    format!("{}: {title}: {failed} failed", p.m.name)
                } else {
                    format!("{}: {line}", p.m.name)
                };
            }
        }
        if lists_changed {
            self.tests.rebuild_marks();
        }
    }

    /// The live state runs only while the Tests tab shows.
    pub(super) fn keep_watch(&mut self, ctx: &egui::Context) {
        let root = self.project.as_ref().map(|p| p.root.clone());
        let shown = self.bottom == Bottom::Tests;
        for (i, p) in self.tests.plugins.iter_mut().enumerate() {
            let want = shown && i == self.tests.active;
            match (&p.watch, &p.m.watch) {
                (None, Some(w)) if want => {
                    p.watch = Run::spawn(p.m.command(w, root.as_deref(), ""), ctx.clone()).ok();
                }
                (Some(_), _) if !want => {
                    if let Some(mut w) = p.watch.take() {
                        stop(&mut w);
                    }
                    p.live = None;
                }
                _ => {}
            }
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
                for (name, url) in p.m.links.iter().rev() {
                    if ui.link(name).on_hover_text(url).clicked() {
                        click = Some(Click::Link(url.clone()));
                    }
                }
                for (a, n) in p.m.actions.iter().enumerate().rev() {
                    if ui
                        .add_enabled(p.run.is_none(), egui::Button::new(&n.name).small())
                        .clicked()
                    {
                        click = Some(Click::Action(a));
                    }
                }
            });
        });
        ui.separator();
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
        match click {
            Some(Click::Run(id)) => self.run_case(active, &id, None, &ctx),
            Some(Click::Action(a)) => self.run_case(active, "", Some(a), &ctx),
            Some(Click::Stop) => {
                if let Some((_, _, run)) = self.tests.plugins[active].run.as_mut() {
                    stop(run);
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
            Some(Click::Link(u)) => open_link(&u),
            Some(Click::Add) => self.pick_plugin(&ctx),
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

    /// The case tree, a filtered flat list, or the cases a gutter ▶ picked.
    fn cases_list(&mut self, ui: &mut Ui, active: usize) -> Option<Click> {
        let mut click = None;
        let t = &mut self.tests;
        let p = &t.plugins[active];
        if let Some(e) = &p.error {
            ui.colored_label(crate::highlight::ERROR, e);
        }
        if p.loading.is_some() {
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
