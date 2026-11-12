//! GitLens-style views: inline blame on the current line, a blame column, file history and commit diffs.

use super::{App, FONT_SIZE};
use crate::git::{self, Blame, Commit};
use crate::highlight::{ERROR, INFO, TEXT};
use egui::{Align2, Color32, FontId, Galley, Pos2, RichText, Sense, Ui};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const DIM: Color32 = Color32::from_gray(110);
/// Wait this long after the last keystroke before blaming the edited text again.
const DEBOUNCE: Duration = Duration::from_millis(450);

/// Blame state of one tab.
#[derive(Default)]
pub(super) struct TabBlame {
    pub blame: Option<Arc<Blame>>,
    rx: Option<Receiver<Option<Blame>>>,
    /// Edit counter (`Tab::rev`) of the text the current (or running) blame is for.
    for_hash: u64,
    seen_hash: u64,
    seen_at: Option<Instant>,
}

#[derive(Default)]
pub(super) struct GitState {
    pub show_column: bool,
    history_for: Option<PathBuf>,
    history: Vec<Commit>,
    history_rx: Option<Receiver<Vec<Commit>>>,
    /// Open commit window: title and `git show` output.
    commit: Option<(String, Vec<String>)>,
    commit_rx: Option<Receiver<(String, String)>>,
    /// Uncommitted files and the branch's changes (refreshed in the background).
    pub changes: Option<git::Changes>,
    changes_rx: Option<Receiver<git::Changes>>,
    /// Set by file-system events and git commands; the next Git tab frame refreshes.
    pub changes_stale: bool,
    commit_message: String,
}

pub(super) fn commit_line(c: &Commit, now: i64) -> String {
    if c.is_uncommitted() {
        "You • Not committed yet".into()
    } else {
        format!("{}, {} • {}", c.author, git::ago(c.time, now), c.summary)
    }
}

fn commit_tooltip(ui: &mut Ui, c: &Commit, now: i64) {
    if c.is_uncommitted() {
        ui.label("Not committed yet");
        return;
    }
    ui.label(RichText::new(&c.summary).strong());
    ui.label(format!("{} <{}>", c.author, c.email));
    ui.label(RichText::new(format!("{}  •  {}", git::ago(c.time, now), c.short())).weak());
    ui.label(RichText::new("Click to see the commit").weak().small());
}

impl App {
    #[cfg(test)]
    pub(super) fn git_history_len(&self) -> usize {
        self.git.history.len()
    }

    #[cfg(test)]
    pub(super) fn commit_lines(&self) -> Option<&[String]> {
        self.git.commit.as_ref().map(|(_, l)| l.as_slice())
    }

    /// Starts or refreshes blame for the active tab when its text settled after an edit.
    pub(super) fn poll_git(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            let h = t.rev; // bumped on every edit, cheaper than hashing the text each frame
            if h != t.git.seen_hash {
                t.git.seen_hash = h;
                // first look at a file: blame now; after a keystroke: wait for a pause
                t.git.seen_at = if t.git.for_hash == 0 {
                    Some(Instant::now() - DEBOUNCE)
                } else {
                    Some(Instant::now())
                };
            }
            let settled = t.git.seen_at.is_some_and(|at| at.elapsed() >= DEBOUNCE);
            if settled && t.git.rx.is_none() && t.git.for_hash != h {
                let (tx, rx) = channel();
                let (path, text, c) = (t.path.clone(), t.text.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(git::blame(&path, &text));
                    c.request_repaint();
                });
                t.git.rx = Some(rx);
                t.git.for_hash = h;
            } else if t.git.seen_at.is_some() && !settled {
                ctx.request_repaint_after(DEBOUNCE);
            }
            if let Some(rx) = &t.git.rx
                && let Ok(b) = rx.try_recv()
            {
                t.git.blame = b.map(Arc::new);
                t.git.rx = None;
            }
        }
        let g = &mut self.git;
        if let Some(rx) = &g.history_rx
            && let Ok(h) = rx.try_recv()
        {
            g.history = h;
            g.history_rx = None;
        }
        if let Some(rx) = &g.commit_rx
            && let Ok((title, text)) = rx.try_recv()
        {
            g.commit = Some((title, text.lines().map(String::from).collect()));
            g.commit_rx = None;
        }
    }

    pub(super) fn open_commit(&mut self, sha: &str, path: &Path, ctx: &egui::Context) {
        if sha == git::UNCOMMITTED {
            return;
        }
        let (tx, rx) = channel();
        let (sha, path, c) = (sha.to_string(), path.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            let text = git::show(&dir, &sha, Some(&path));
            let _ = tx.send((format!("Commit {}", &sha[..8.min(sha.len())]), text));
            c.request_repaint();
        });
        self.git.commit_rx = Some(rx);
    }

    /// The Git tab: history of the active file.
    /// Shows a diff (of a commit or a file) in the diff window.
    fn show_diff(&mut self, title: String, text: String) {
        self.git.commit = Some((title, text.lines().map(String::from).collect()));
    }

    fn refresh_changes(&mut self, ctx: &egui::Context) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        if self.git.changes_rx.is_none() && (self.git.changes.is_none() || self.git.changes_stale) {
            self.git.changes_stale = false;
            let (tx, rx) = channel();
            let c = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(git::changes(&root)); // gh may use the network: never on the UI thread
                c.request_repaint();
            });
            self.git.changes_rx = Some(rx);
        }
        if let Some(rx) = &self.git.changes_rx
            && let Ok(ch) = rx.try_recv()
        {
            self.git.changes = Some(ch);
            self.git.changes_rx = None;
        }
    }

    fn git_command(&mut self, args: &[&str]) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        match git::run(&root, args) {
            Ok(()) => self.status = format!("git {} done", args.join(" ")),
            Err(e) => self.status = format!("git {}: {e}", args[0]),
        }
        self.git.changes_stale = true;
        if args[0] == "checkout" {
            self.branch = git::branch(&root);
        }
    }

    /// The Git tab: uncommitted changes (stage, commit), the branch / pull request's files,
    /// branch switch, and the open file's history.
    pub(super) fn git_panel(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.refresh_changes(&ctx);
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        let mut command: Option<Vec<String>> = None;
        let mut diff: Option<(PathBuf, Option<String>, bool)> = None;
        let mut open_file: Option<PathBuf> = None;
        let code_color = |c: char| match c {
            'A' => Color32::from_rgb(0x5F, 0xB8, 0x65),
            'D' => ERROR,
            'R' => Color32::from_rgb(0xA9, 0x7B, 0xFF),
            '?' => crate::theme::DARK.weak,
            _ => Color32::from_rgb(0x58, 0x9D, 0xF6),
        };
        ui.horizontal(|ui| {
            let branch = self.branch.clone().unwrap_or_else(|| "(no branch)".into());
            ui.menu_button(format!("{}  {branch}", crate::icons::GIT_BRANCH), |ui| {
                for b in self
                    .git
                    .changes
                    .as_ref()
                    .map(|c| c.branches.clone())
                    .unwrap_or_default()
                {
                    if ui
                        .selectable_label(Some(&b) == self.branch.as_ref(), &b)
                        .clicked()
                    {
                        command = Some(vec!["checkout".into(), b]);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Switch branch");
            if ui.button("Refresh").clicked() {
                self.git.changes_stale = true;
            }
            if self.git.changes_rx.is_some() {
                ui.spinner();
            }
            ui.checkbox(&mut self.git.show_column, "Blame column");
        });
        if let Some(ch) = self.git.changes.clone() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Uncommitted changes ({})", ch.working.len())).strong(),
                );
                if ch.working.iter().any(|c| !c.staged) && ui.small_button("Stage all").clicked() {
                    command = Some(vec!["add".into(), "-A".into()]);
                }
            });
            if ch.working.is_empty() {
                ui.label(RichText::new("Working tree clean").weak());
            }
            for c in &ch.working {
                let rel = c
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&c.path)
                    .display()
                    .to_string();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(c.code.to_string())
                            .monospace()
                            .strong()
                            .color(code_color(c.code)),
                    );
                    if ui.link(&rel).on_hover_text("Show the diff").clicked() {
                        diff = Some((c.path.clone(), None, c.staged));
                    }
                    ui.label(
                        RichText::new(if c.staged { "staged" } else { "" })
                            .weak()
                            .small(),
                    );
                    if c.code != 'D' && ui.small_button("Open").clicked() {
                        open_file = Some(c.path.clone());
                    }
                    let p = c.path.to_string_lossy().into_owned();
                    if c.staged {
                        if ui.small_button("Unstage").clicked() {
                            command =
                                Some(vec!["restore".into(), "--staged".into(), "--".into(), p]);
                        }
                    } else if ui.small_button("Stage").clicked() {
                        command = Some(vec!["add".into(), "--".into(), p]);
                    }
                });
            }
            if ch.working.iter().any(|c| c.staged) {
                ui.horizontal(|ui| {
                    let label = ui.label("Commit message");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.git.commit_message)
                            .hint_text("what changed")
                            .desired_width(420.0),
                    )
                    .labelled_by(label.id);
                    let ok = !self.git.commit_message.trim().is_empty();
                    if ui
                        .add_enabled(ok, egui::Button::new("Commit staged"))
                        .clicked()
                    {
                        command = Some(vec![
                            "commit".into(),
                            "-m".into(),
                            self.git.commit_message.trim().to_string(),
                        ]);
                        self.git.commit_message.clear();
                    }
                });
            }
            ui.separator();
            let base = ch.base.clone().unwrap_or_else(|| "?".into());
            let title = match &ch.pr {
                Some(pr) => format!("Pull request {pr}: {} files vs {base}", ch.branch.len()),
                None => format!("This branch: {} files changed vs {base}", ch.branch.len()),
            };
            ui.label(RichText::new(title).strong());
            if ch.base.is_none() {
                ui.label(
                    RichText::new(
                        "No base branch found (no origin/HEAD, develop, main or master).",
                    )
                    .weak(),
                );
            }
            for c in &ch.branch {
                let rel = c
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&c.path)
                    .display()
                    .to_string();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(c.code.to_string())
                            .monospace()
                            .strong()
                            .color(code_color(c.code)),
                    );
                    if ui
                        .link(&rel)
                        .on_hover_text("Show the diff against the base")
                        .clicked()
                    {
                        diff = Some((c.path.clone(), ch.base.clone(), false));
                    }
                    if c.code != 'D' && ui.small_button("Open").clicked() {
                        open_file = Some(c.path.clone());
                    }
                });
            }
        }
        ui.separator();
        if let Some(args) = command {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.git_command(&args);
        }
        if let Some((p, base, staged)) = diff {
            let text = git::file_diff(&root, &p, base.as_deref(), staged);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.show_diff(
                format!(
                    "Diff: {name}{}",
                    base.map(|b| format!(" vs {b}")).unwrap_or_default()
                ),
                text,
            );
        }
        if let Some(p) = open_file {
            self.open_file(&p, None);
        }
        let Some(path) = self.tabs.get(self.active).map(|t| t.path.clone()) else {
            ui.label("Open a file to see its history.");
            return;
        };
        if self.git.history_for.as_ref() != Some(&path) && self.git.history_rx.is_none() {
            let (tx, rx) = channel();
            let (p, c) = (path.clone(), ui.ctx().clone());
            std::thread::spawn(move || {
                let _ = tx.send(git::history(&p, 300));
                c.request_repaint();
            });
            self.git.history_rx = Some(rx);
            self.git.history_for = Some(path.clone());
        }
        ui.horizontal(|ui| {
            if self.git.history_rx.is_some() {
                ui.spinner();
            }
            ui.label(
                RichText::new(format!(
                    "{} commits changed {}",
                    self.git.history.len(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
                .weak(),
            );
        });
        let now = git::now();
        let mut open = None;
        egui::Grid::new("history")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for c in &self.git.history {
                    if ui.link(RichText::new(c.short()).monospace()).clicked() {
                        open = Some(c.sha.clone());
                    }
                    ui.label(&c.author);
                    ui.label(RichText::new(git::ago(c.time, now)).weak());
                    ui.label(&c.summary);
                    ui.end_row();
                }
            });
        if let Some(sha) = open {
            self.open_commit(&sha, &path, &ui.ctx().clone());
        }
    }

    pub(super) fn commit_window(&mut self, ctx: &egui::Context) {
        let Some((title, lines)) = &self.git.commit else {
            return;
        };
        let mut open = !ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        egui::Window::new(title.clone())
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .default_size([900.0, 620.0])
            .resizable(true)
            .show(ctx, |ui| {
                let row_h = ui.fonts_mut(|f| f.row_height(&FontId::monospace(FONT_SIZE - 1.0)));
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, lines.len(), |ui, range| {
                        for l in &lines[range] {
                            let color = if l.starts_with("+++") || l.starts_with("---") {
                                TEXT
                            } else if l.starts_with('+') {
                                Color32::from_rgb(0x6A, 0xB0, 0x6A)
                            } else if l.starts_with('-') {
                                ERROR
                            } else if l.starts_with("@@") {
                                INFO
                            } else {
                                TEXT
                            };
                            ui.label(
                                RichText::new(l)
                                    .monospace()
                                    .size(FONT_SIZE - 1.0)
                                    .color(color),
                            );
                        }
                    });
            });
        if !open {
            self.git.commit = None;
        }
    }
}

/// Grey "author, time • summary" after the end of the cursor's line, with the full commit on hover.
/// Returns the commit to open when it is clicked.
pub(super) fn paint_inline_blame(
    ui: &mut Ui,
    blame: &Blame,
    galley: &Galley,
    galley_pos: Pos2,
    line: usize,
    min_x: Option<f32>,
) -> Option<String> {
    let c = blame.line(line)?;
    let row = galley.rows.get(line)?;
    let mut pos = galley_pos + row.rect().right_top().to_vec2() + egui::vec2(48.0, 0.0);
    if let Some(x) = min_x {
        pos.x = pos.x.max(x + 24.0); // after a stability badge on the same line
    }
    let now = git::now();
    let rect = ui.painter().text(
        pos,
        Align2::LEFT_TOP,
        commit_line(c, now),
        FontId::proportional(FONT_SIZE - 1.0),
        DIM,
    );
    let r = ui.interact(rect, ui.scope_id().with("inline-blame"), Sense::click());
    let clicked = r.clicked();
    r.on_hover_ui(|ui| commit_tooltip(ui, c, now));
    clicked.then(|| c.sha.clone())
}

pub(super) const COLUMN_WIDTH: f32 = 210.0;

/// The blame column in `rect`: author and age at the first line of each block from the same commit,
/// painted for visible rows only. Returns the commit to open when a line is clicked.
pub(super) fn blame_column(
    ui: &mut Ui,
    blame: Option<&Blame>,
    rect: egui::Rect,
    galley: &Galley,
    galley_pos: Pos2,
    row_h: f32,
) -> Option<String> {
    let b = blame?;
    let now = git::now();
    let clip = ui.clip_rect();
    let font = FontId::proportional(FONT_SIZE - 1.0);
    let painter = ui.painter();
    for (i, row) in galley.rows.iter().enumerate() {
        let y = galley_pos.y + row.pos.y;
        if y > clip.bottom() {
            break;
        }
        if y + row_h < clip.top() {
            continue;
        }
        let (Some(c), first) = (b.line(i), i == 0 || b.lines.get(i - 1) != b.lines.get(i)) else {
            continue;
        };
        if !first {
            continue;
        }
        let who = if c.is_uncommitted() {
            "You".to_string()
        } else {
            c.author.chars().take(18).collect()
        };
        let when = if c.is_uncommitted() {
            "now".to_string()
        } else {
            git::ago(c.time, now)
        };
        painter.text(
            Pos2::new(rect.left() + 6.0, y),
            Align2::LEFT_TOP,
            who,
            font.clone(),
            DIM,
        );
        painter.text(
            Pos2::new(rect.right() - 10.0, y),
            Align2::RIGHT_TOP,
            when,
            font.clone(),
            DIM,
        );
    }
    let r = ui.interact(rect, ui.scope_id().with("blame-column"), Sense::click());
    let line_at = |p: Pos2| {
        galley
            .rows
            .iter()
            .position(|row| p.y < galley_pos.y + row.pos.y + row_h)
    };
    let mut open = None;
    if r.clicked()
        && let Some(p) = r.interact_pointer_pos()
    {
        open = line_at(p).and_then(|i| b.line(i)).map(|c| c.sha.clone());
    }
    if let Some(p) = r.hover_pos()
        && let Some(c) = line_at(p).and_then(|i| b.line(i))
    {
        r.on_hover_ui_at_pointer(|ui| commit_tooltip(ui, c, now));
    }
    open
}
