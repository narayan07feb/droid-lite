//! GitLens-style views: inline blame on the current line, a blame column, file history and commit diffs.

use super::App;
use crate::git::{self, Blame, Commit};
use crate::highlight::{ERROR, INFO};
use egui::{Align, Align2, Color32, FontId, Galley, Layout, Pos2, RichText, Sense, Ui};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const DIM: Color32 = Color32::from_gray(110);
/// Wait this long after the last keystroke before blaming the edited text again.
const DEBOUNCE: Duration = Duration::from_millis(450);

/// Blame, the uncommitted change bars and the branch change bars of one text.
type BlameAndBars = (Option<Blame>, Vec<git::Hunk>, Vec<git::Hunk>);

/// Blame state of one tab.
#[derive(Default)]
pub(super) struct TabBlame {
    pub blame: Option<Arc<Blame>>,
    rx: Option<Receiver<BlameAndBars>>,
    /// Uncommitted line changes against HEAD, for the gutter bars.
    pub hunks: Option<Arc<Vec<git::Hunk>>>,
    /// What this branch changed against the PR base (committed or not), for the dim bars.
    pub branch_hunks: Option<Arc<Vec<git::Hunk>>>,
    /// The PR base `branch_hunks` were made for.
    for_base: Option<String>,
    /// Edit counter (`Tab::rev`) of the text the current (or running) blame is for.
    for_hash: u64,
    seen_hash: u64,
    seen_at: Option<Instant>,
}

/// The Current changes lists, with the branch and HEAD commit read with them.
type Refreshed = (git::Changes, Option<String>, Option<String>);

#[derive(Default)]
pub(super) struct GitState {
    pub show_column: bool,
    history_for: Option<PathBuf>,
    history: Vec<Commit>,
    history_rx: Option<Receiver<Vec<Commit>>>,
    /// Open diff panel: title and `git show` / `git diff` output.
    pub(super) commit: Option<(String, Vec<String>)>,
    /// The diff panel shows one column (unified) instead of old | new side by side.
    unified: bool,
    commit_rx: Option<Receiver<(String, String)>>,
    /// Uncommitted files and the branch's changes (refreshed in the background).
    pub changes: Option<git::Changes>,
    changes_rx: Option<Receiver<Refreshed>>,
    /// Set by file-system events and git commands; the next Current changes tab frame refreshes.
    pub changes_stale: bool,
    commit_message: String,
    /// Open pull requests for the base PR picker (loaded when the picker opens).
    prs: Option<Vec<git::OpenPr>>,
    prs_rx: Option<Receiver<Vec<git::OpenPr>>>,
    /// Typed base: `#123` or a branch name.
    base_input: String,
    /// The row whose Revert waits for "Discard?" Yes: the file, and true for a PR file.
    confirm_revert: Option<(PathBuf, bool)>,
}

/// Discard one file's changes: the uncommitted ones, or what this branch changed in it.
pub(super) enum Revert {
    Working { path: PathBuf, untracked: bool },
    Branch { path: PathBuf, base: String },
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
        let base = self.git.changes.as_ref().and_then(|c| c.base.clone());
        let root = self.project.as_ref().map(|p| p.root.clone());
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
            if settled && t.git.rx.is_none() && (t.git.for_hash != h || t.git.for_base != base) {
                let (tx, rx) = channel();
                let (path, text, c) = (t.path.clone(), t.text.clone(), ctx.clone());
                let (b, root) = (base.clone(), root.clone());
                std::thread::spawn(move || {
                    let blame = git::blame(&path, &text);
                    let all_new = || {
                        vec![git::Hunk {
                            new_start: 0,
                            new_len: text.lines().count().max(1),
                            old: Vec::new(),
                        }]
                    };
                    // the change bars: this text against the file at HEAD (untracked: all new)
                    let head = git::head_text(&path);
                    let hunks = match &head {
                        Some(head) => git::hunks(head, &text),
                        None if blame.is_some() => all_new(),
                        None => Vec::new(),
                    };
                    // the dim bars: against where the branch left its PR base (added on it: all new)
                    let fork = b.zip(root).and_then(|(b, r)| git::merge_base(&r, &b));
                    let branch = match fork.map(|f| git::file_at(&path, &f)) {
                        Some(Some(old)) => git::hunks(&old, &text),
                        Some(None) if head.is_some() => all_new(),
                        _ => Vec::new(),
                    };
                    let _ = tx.send((blame, hunks, branch));
                    c.request_repaint();
                });
                t.git.rx = Some(rx);
                t.git.for_hash = h;
                t.git.for_base = base;
            } else if t.git.seen_at.is_some() && !settled {
                ctx.request_repaint_after(DEBOUNCE);
            }
            if let Some(rx) = &t.git.rx
                && let Ok((b, hunks, branch)) = rx.try_recv()
            {
                t.git.hunks = b.is_some().then(|| Arc::new(hunks));
                t.git.branch_hunks = b.is_some().then(|| Arc::new(branch));
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

    /// Shows a diff (of a commit or a file) in the diff window.
    pub(super) fn show_diff_text(&mut self, title: String, text: String) {
        self.git.commit = Some((title, text.lines().map(String::from).collect()));
    }

    /// Settings key of the base choice for the current branch.
    fn base_key(&self) -> Option<String> {
        let root = &self.project.as_ref()?.root;
        Some(format!("{}\t{}", root.display(), self.branch.as_deref()?))
    }

    /// Remembers `choice` (`#123`, a branch, or `None` for the automatic base) for this branch.
    fn set_base(&mut self, choice: Option<String>) {
        let Some(key) = self.base_key() else {
            return;
        };
        match choice {
            Some(c) => self.settings.pr_bases.insert(key, c),
            None => self.settings.pr_bases.remove(&key),
        };
        self.settings.save();
        self.git.changes_stale = true;
    }

    pub(super) fn refresh_changes(&mut self, ctx: &egui::Context) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        if self.git.changes_rx.is_none() && (self.git.changes.is_none() || self.git.changes_stale) {
            self.git.changes_stale = false;
            let choice = self
                .base_key()
                .and_then(|k| self.settings.pr_bases.get(&k).cloned());
            let (tx, rx) = channel();
            let c = ctx.clone();
            std::thread::spawn(move || {
                // gh may use the network: never on the UI thread
                let ch = git::changes(&root, choice.as_deref());
                let _ = tx.send((ch, git::branch(&root), git::head_sha(&root)));
                c.request_repaint();
            });
            self.git.changes_rx = Some(rx);
        }
        if let Some(rx) = &self.git.changes_rx
            && let Ok((ch, branch, head)) = rx.try_recv()
        {
            self.git.changes = Some(ch);
            self.git.changes_rx = None;
            // a commit, or a branch switched outside Droid Lite: the branch name and the
            // PR list's "● current" follow
            self.branch = branch;
            self.prs.head = head;
        }
    }

    pub(super) fn revert_file(&mut self, root: &Path, r: Revert) {
        if self.refuse_during_checkout() {
            return;
        }
        if self.saves_pending > 0 {
            let root = root.to_path_buf();
            self.settle_saves("Revert", move |a| a.revert_file(&root, r));
            return;
        }
        let (path, res) = match r {
            Revert::Working {
                path,
                untracked: true,
            } => {
                let res = std::fs::remove_file(&path).map_err(|e| e.to_string());
                (path, res)
            }
            Revert::Working { path, .. } => {
                let res = git::revert_file(root, &path, "HEAD");
                (path, res)
            }
            Revert::Branch { path, base } => {
                let res = git::merge_base(root, &base)
                    .ok_or_else(|| format!("no common commit with {base}"))
                    .and_then(|mb| git::revert_file(root, &path, &mb));
                (path, res)
            }
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.status = match res {
            Ok(()) => format!("Reverted {name}"),
            Err(e) => format!("Revert {name}: {e}"),
        };
        self.git.changes_stale = true;
    }

    pub(super) fn git_command(&mut self, args: &[&str]) {
        if self.refuse_during_checkout() {
            return;
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let again =
            move |a: &mut App| a.git_command(&owned.iter().map(String::as_str).collect::<Vec<_>>());
        if !self.settle_saves(&format!("git {}", args.join(" ")), again) {
            return;
        }
        match git::run(&root, args) {
            Ok(()) => self.status = format!("git {} done", args.join(" ")),
            Err(e) => self.status = format!("git {}: {e}", args[0]),
        }
        self.git.changes_stale = true;
        if args[0] == "checkout" {
            self.branch = git::branch(&root);
        }
    }

    /// The pull request title and the base picker. On a PR branch with no base picked yet it asks
    /// which PR this one is based on (stacked PRs), so the files and diffs show only this PR's work.
    fn pr_header(&mut self, ui: &mut Ui, ch: &git::Changes, choice: &mut Option<Option<String>>) {
        let picked = self
            .base_key()
            .and_then(|k| self.settings.pr_bases.get(&k).cloned());
        let title = match &ch.pr {
            Some(pr) => format!("Pull request {pr}"),
            None => "This branch".to_string(),
        };
        ui.label(RichText::new(title).strong());
        if ch.pr.is_some() && picked.is_none() {
            ui.label(
                RichText::new(
                    "Which PR is this one based on? Pick it to see only this PR's changes.",
                )
                .color(crate::highlight::WARN),
            );
        }
        // the open PRs, once, off the UI thread (gh uses the network)
        if self.git.prs.is_none()
            && self.git.prs_rx.is_none()
            && let Some(root) = self.project.as_ref().map(|p| p.root.clone())
        {
            let (tx, rx) = channel();
            let c = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(git::open_prs(&root));
                c.request_repaint();
            });
            self.git.prs_rx = Some(rx);
        }
        if let Some(rx) = &self.git.prs_rx
            && let Ok(prs) = rx.try_recv()
        {
            self.git.prs = Some(prs);
            self.git.prs_rx = None;
        }
        let auto = format!(
            "Automatic ({})",
            ch.auto_base.as_deref().unwrap_or("none found")
        );
        let pr_label = |p: &git::OpenPr| format!("#{} {}", p.number, p.title);
        let prs = self.git.prs.clone().unwrap_or_default();
        let shown = match picked.as_deref().map(|c| (c, git::pr_number(c))) {
            None => auto.clone(),
            Some((_, Some(n))) => prs
                .iter()
                .find(|p| p.number == n)
                .map_or(format!("#{n}"), pr_label),
            Some((c, None)) => c.to_string(),
        };
        let branch = self.branch.clone().unwrap_or_default();
        // "Base" and "or" are equally wide, so the two fields after them line up
        let label_w = ui.fonts_mut(|f| {
            let font = egui::TextStyle::Body.resolve(ui.style());
            f.layout_no_wrap("Base".into(), font, Color32::WHITE)
                .size()
                .x
        });
        ui.horizontal(|ui| {
            field_label(ui, "Base", label_w);
            egui::ComboBox::from_id_salt("base-pr")
                .selected_text(shown)
                .truncate()
                .width(ui.available_width().min(260.0))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(picked.is_none(), &auto).clicked() {
                        *choice = Some(None);
                    }
                    if self.git.prs_rx.is_some() {
                        ui.label(RichText::new("Loading open PRs…").weak());
                    }
                    for p in prs.iter().filter(|p| p.head != branch) {
                        let c = format!("#{}", p.number);
                        if ui
                            .selectable_label(picked.as_deref() == Some(c.as_str()), pr_label(p))
                            .on_hover_text(&p.head)
                            .clicked()
                        {
                            *choice = Some(Some(c));
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            let label = field_label(ui, "or", label_w);
            let r = ui
                .add(
                    egui::TextEdit::singleline(&mut self.git.base_input)
                        .hint_text("#123 or a branch")
                        // leave room for the "Use" button
                        .desired_width((ui.available_width() - 48.0).max(40.0)),
                )
                .labelled_by(label.id);
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let typed = self.git.base_input.trim().to_string();
            if (ui
                .add_enabled(!typed.is_empty(), egui::Button::new("Use"))
                .clicked()
                || enter)
                && !typed.is_empty()
            {
                *choice = Some(Some(typed));
                self.git.base_input.clear();
            }
        });
        if let Some(e) = &ch.base_error {
            ui.label(RichText::new(format!("{e}: using the automatic base")).color(ERROR));
        }
        ui.label(
            RichText::new(format!(
                "{} files changed vs {}",
                ch.branch.len(),
                ch.base.as_deref().unwrap_or("?")
            ))
            .weak(),
        );
        if ch.base.is_none() {
            ui.label(
                RichText::new("No base branch found (no origin/HEAD, develop, main or master).")
                    .weak(),
            );
        }
    }

    /// The Current changes tab: the branch / pull request's files, uncommitted changes (stage, commit),
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
            '?' => crate::theme::t().weak,
            _ => Color32::from_rgb(0x58, 0x9D, 0xF6),
        };
        right_row(ui, |ui| {
            ui.checkbox(&mut self.git.show_column, "Blame column");
            if ui.button("Refresh").clicked() {
                self.git.changes_stale = true;
            }
            if self.git.changes_rx.is_some() {
                ui.spinner();
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let branch = self.branch.clone().unwrap_or_else(|| "(no branch)".into());
                let button =
                    egui::Button::new(format!("{}  {branch}", crate::icons::GIT_BRANCH)).truncate();
                egui::containers::menu::MenuButton::from_button(button)
                    .ui(ui, |ui| {
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
                    .0
                    .on_hover_text(format!("{branch}\nSwitch branch"));
            });
        });
        let mut base_choice: Option<Option<String>> = None;
        let mut revert: Option<Revert> = None;
        let mut confirm = self.git.confirm_revert.clone();
        // "Revert", then "Discard? Yes / No" in the same row (right to left: "No" first)
        let mut revert_button = |ui: &mut Ui, key: (PathBuf, bool), tip: &str| -> bool {
            if confirm.as_ref() == Some(&key) {
                let no = ui.small_button("No").clicked();
                let yes = ui.small_button("Yes").clicked();
                ui.label(RichText::new("Discard?").color(ERROR));
                if yes || no {
                    confirm = None;
                }
                yes
            } else {
                if ui.small_button("Revert").on_hover_text(tip).clicked() {
                    confirm = Some(key);
                }
                false
            }
        };
        if let Some(ch) = self.git.changes.clone() {
            ui.separator();
            self.pr_header(ui, &ch, &mut base_choice);
            for c in &ch.branch {
                let rel = c
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&c.path)
                    .display()
                    .to_string();
                let name = c
                    .path
                    .file_name()
                    .map_or(rel.clone(), |n| n.to_string_lossy().into_owned());
                let clicked = file_row(ui, c.code, code_color(c.code), &name, &rel, |ui| {
                    if let Some(base) = &ch.base
                        && revert_button(
                            ui,
                            (c.path.clone(), true),
                            "Put this file back as it was on the base (an uncommitted change)",
                        )
                    {
                        revert = Some(Revert::Branch {
                            path: c.path.clone(),
                            base: base.clone(),
                        });
                    }
                    if ui
                        .small_button("Diff")
                        .on_hover_text("Show the diff against the base")
                        .clicked()
                    {
                        diff = Some((c.path.clone(), ch.base.clone(), false));
                    }
                });
                if clicked {
                    if c.code == 'D' {
                        diff = Some((c.path.clone(), ch.base.clone(), false));
                    } else {
                        open_file = Some(c.path.clone());
                    }
                }
            }
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
                let name = c
                    .path
                    .file_name()
                    .map_or(rel.clone(), |n| n.to_string_lossy().into_owned());
                let clicked = file_row(ui, c.code, code_color(c.code), &name, &rel, |ui| {
                    let untracked = c.code == '?';
                    if revert_button(
                        ui,
                        (c.path.clone(), false),
                        if untracked {
                            "Delete this new file"
                        } else {
                            "Discard the changes: back to the last commit"
                        },
                    ) {
                        revert = Some(Revert::Working {
                            path: c.path.clone(),
                            untracked,
                        });
                    }
                    if ui.small_button("Diff").clicked() {
                        diff = Some((c.path.clone(), None, c.staged));
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
                if clicked {
                    if c.code == 'D' {
                        diff = Some((c.path.clone(), None, c.staged));
                    } else {
                        open_file = Some(c.path.clone());
                    }
                }
            }
            if ch.working.iter().any(|c| c.staged) {
                let label = ui.label("Commit message");
                right_row(ui, |ui| {
                    let ok = !self.git.commit_message.trim().is_empty();
                    let commit = ui
                        .add_enabled(ok, egui::Button::new("Commit staged"))
                        .clicked();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.git.commit_message)
                            .hint_text("what changed")
                            .desired_width(ui.available_width()),
                    )
                    .labelled_by(label.id);
                    if commit {
                        command = Some(vec![
                            "commit".into(),
                            "-m".into(),
                            self.git.commit_message.trim().to_string(),
                        ]);
                        self.git.commit_message.clear();
                    }
                });
            }
        }
        ui.separator();
        if let Some(c) = base_choice {
            self.set_base(c);
        }
        self.git.confirm_revert = confirm;
        if let Some(r) = revert {
            self.revert_file(&root, r);
        }
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
            self.show_diff_text(
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
        // one short row per commit (the panel is narrow): sha, age, summary; author on hover
        for c in &self.git.history {
            ui.horizontal(|ui| {
                if ui
                    .link(RichText::new(c.short()).monospace())
                    .on_hover_text(&c.author)
                    .clicked()
                {
                    open = Some(c.sha.clone());
                }
                ui.label(RichText::new(git::ago(c.time, now)).weak());
                ui.add(egui::Label::new(&c.summary).truncate());
            });
        }
        if let Some(sha) = open {
            self.open_commit(&sha, &path, &ui.ctx().clone());
        }
    }

    pub(super) fn has_diff(&self) -> bool {
        self.git.commit.is_some()
    }

    /// The diff of a commit or a file, docked beside the editor like Android Studio's diff:
    /// side by side (old | new, with line numbers) or unified. Esc or × closes it.
    pub(super) fn diff_panel(&mut self, ui: &mut Ui) {
        let Some((title, lines)) = &self.git.commit else {
            return;
        };
        let mut open = !ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        right_row(ui, |ui| {
            if ui
                .add(egui::Button::new(crate::icons::X).frame(false))
                .on_hover_text("Close the diff  Esc")
                .clicked()
            {
                open = false;
            }
            let mut split = !self.git.unified;
            ui.checkbox(&mut split, "Side by side");
            self.git.unified = !split;
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
            });
        });
        ui.separator();
        let size = crate::theme::font_size() - 1.0;
        let font = FontId::monospace(size);
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let code = crate::theme::t().code;
        let added = Color32::from_rgb(0x6A, 0xB0, 0x6A);
        if self.git.unified {
            egui::ScrollArea::both()
                .id_salt("diff-unified")
                .auto_shrink([false, false])
                .show_rows(ui, row_h, lines.len(), |ui, range| {
                    for l in &lines[range] {
                        let color = if l.starts_with("+++") || l.starts_with("---") {
                            code
                        } else if l.starts_with('+') {
                            added
                        } else if l.starts_with('-') {
                            ERROR
                        } else if l.starts_with("@@") {
                            INFO
                        } else {
                            code
                        };
                        ui.label(RichText::new(l).monospace().size(size).color(color));
                    }
                });
        } else {
            let rows = side_rows(lines);
            let num_w = ui.fonts_mut(|f| {
                f.layout_no_wrap("00000".into(), font.clone(), code)
                    .size()
                    .x
            });
            egui::ScrollArea::vertical()
                .id_salt("diff-split")
                .auto_shrink([false, false])
                .show_rows(ui, row_h, rows.len(), |ui, range| {
                    for row in &rows[range] {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), row_h),
                            Sense::hover(),
                        );
                        let p = ui.painter_at(rect);
                        let text = |p: &egui::Painter, x: f32, s: &str, c: Color32| {
                            p.text(
                                Pos2::new(x, rect.top()),
                                Align2::LEFT_TOP,
                                s,
                                font.clone(),
                                c,
                            );
                        };
                        match row {
                            SideRow::Head(h) => {
                                let c = if h.starts_with("@@") { INFO } else { DIM };
                                text(&p, rect.left() + 4.0, h, c);
                            }
                            SideRow::Pair { old, new, changed } => {
                                let half = rect.width() / 2.0;
                                for (i, side, tint, fg) in
                                    [(0.0, old, ERROR, code), (1.0, new, added, code)]
                                {
                                    let r = egui::Rect::from_min_size(
                                        Pos2::new(rect.left() + i * half, rect.top()),
                                        egui::vec2(half - 2.0, row_h),
                                    );
                                    let p = ui.painter_at(r);
                                    match side {
                                        Some((n, s)) => {
                                            if *changed {
                                                p.rect_filled(r, 0.0, tint.gamma_multiply(0.18));
                                            }
                                            let nx = r.left() + num_w;
                                            p.text(
                                                Pos2::new(nx, r.top()),
                                                Align2::RIGHT_TOP,
                                                n.to_string(),
                                                font.clone(),
                                                DIM,
                                            );
                                            text(&p, nx + 8.0, s, fg);
                                        }
                                        // the other side has lines here: hatch-like dim fill
                                        None => {
                                            p.rect_filled(r, 0.0, DIM.gamma_multiply(0.12));
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
        }
        if !open {
            self.git.commit = None;
        }
    }
}

/// One row of the side-by-side diff: old line (number, text), new line, and what happened.
#[derive(Debug, PartialEq)]
pub(super) enum SideRow {
    /// A header line (`diff --git`, `@@ …`, commit message) across both sides.
    Head(String),
    /// Old and new side; `changed` is false for an unchanged context line.
    Pair {
        old: Option<(usize, String)>,
        new: Option<(usize, String)>,
        changed: bool,
    },
}

/// Pairs a unified diff's removed and added lines up, like Android Studio's two-pane diff.
pub(super) fn side_rows(lines: &[String]) -> Vec<SideRow> {
    let mut rows = Vec::new();
    let (mut o, mut n) = (0usize, 0usize);
    let mut dels: Vec<(usize, String)> = Vec::new();
    let mut adds: Vec<(usize, String)> = Vec::new();
    let flush = |rows: &mut Vec<SideRow>, dels: &mut Vec<_>, adds: &mut Vec<_>| {
        let (mut d, mut a) = (dels.drain(..), adds.drain(..));
        loop {
            let (old, new) = (d.next(), a.next());
            if old.is_none() && new.is_none() {
                break;
            }
            rows.push(SideRow::Pair {
                old,
                new,
                changed: true,
            });
        }
    };
    let mut in_hunk = false;
    for l in lines {
        if let Some(h) = l.strip_prefix("@@ -") {
            flush(&mut rows, &mut dels, &mut adds);
            // @@ -12,3 +14,4 @@
            let num = |s: &str| {
                s.split(',')
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(1)
            };
            let mut parts = h.split(' ');
            o = num(parts.next().unwrap_or("1"));
            n = num(parts.next().unwrap_or("+1").trim_start_matches('+'));
            in_hunk = true;
            rows.push(SideRow::Head(l.clone()));
            continue;
        }
        if l.starts_with("diff --git") {
            in_hunk = false;
        }
        match (in_hunk, l.chars().next()) {
            (true, Some('-')) => {
                dels.push((o, l[1..].to_string()));
                o += 1;
            }
            (true, Some('+')) => {
                adds.push((n, l[1..].to_string()));
                n += 1;
            }
            (true, Some(' ')) | (true, None) => {
                flush(&mut rows, &mut dels, &mut adds);
                let t = l.get(1..).unwrap_or("").to_string();
                rows.push(SideRow::Pair {
                    old: Some((o, t.clone())),
                    new: Some((n, t)),
                    changed: false,
                });
                (o, n) = (o + 1, n + 1);
            }
            _ => {
                flush(&mut rows, &mut dels, &mut adds);
                rows.push(SideRow::Head(l.clone()));
            }
        }
    }
    flush(&mut rows, &mut dels, &mut adds);
    rows
}

/// A form label `w` wide with its text at the right end.
fn field_label(ui: &mut Ui, text: &str, w: f32) -> egui::Response {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(w, h),
        Layout::right_to_left(Align::Center),
        |ui| ui.label(text),
    )
    .inner
}

/// A row laid out right to left (the first widget is at the right end), one line high.
fn right_row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), add)
            .inner
    })
    .inner
}

/// One changed file: its status letter, its name (click to open), and `buttons` on the right.
/// `buttons` adds right to left. The name truncates, so a long name never holds the panel wide.
fn file_row(
    ui: &mut Ui,
    code: char,
    color: Color32,
    name: &str,
    rel: &str,
    buttons: impl FnOnce(&mut Ui),
) -> bool {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(code.to_string())
                .monospace()
                .strong()
                .color(color),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            buttons(ui);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let link = ui.visuals().hyperlink_color;
                ui.add(
                    egui::Label::new(RichText::new(name).color(link))
                        .truncate()
                        .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("{rel}\nClick to open"))
                .clicked()
            })
            .inner
        })
        .inner
    })
    .inner
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
        FontId::proportional(crate::theme::font_size() - 1.0),
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
    let font = FontId::proportional(crate::theme::font_size() - 1.0);
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

/// Change bars at the right edge of the line numbers: green added, blue modified, a red marker
/// where lines were deleted. Hover shows the old lines; a click returns the hunk to revert.
/// `branch`: the dim bars of what the branch changed since its PR base; they do not revert.
pub(super) fn paint_changes(
    ui: &mut Ui,
    hunks: &[git::Hunk],
    branch: bool,
    gutter: egui::Rect,
    galley: &Galley,
    galley_pos: Pos2,
) -> Option<usize> {
    let clip = ui.clip_rect();
    let x0 = gutter.right() - 6.0;
    let mut revert = None;
    for (i, h) in hunks.iter().enumerate() {
        let row_top = |line: usize| galley.rows.get(line).map(|r| galley_pos.y + r.pos.y);
        let row_bottom = |line: usize| {
            galley
                .rows
                .get(line)
                .map(|r| galley_pos.y + r.pos.y + r.rect().height())
        };
        let rect = if h.new_len == 0 {
            // a deletion sits between two lines
            let Some(y) =
                row_top(h.new_start).or_else(|| row_bottom(h.new_start.saturating_sub(1)))
            else {
                continue;
            };
            egui::Rect::from_min_max(Pos2::new(x0 - 1.0, y - 3.0), Pos2::new(x0 + 5.0, y + 3.0))
        } else {
            let (Some(top), Some(bottom)) = (
                row_top(h.new_start),
                row_bottom(h.new_start + h.new_len - 1),
            ) else {
                continue;
            };
            egui::Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x0 + 3.0, bottom))
        };
        if rect.bottom() < clip.top() || rect.top() > clip.bottom() {
            continue;
        }
        let color = match h.kind() {
            'A' => Color32::from_rgb(0x5F, 0xB8, 0x65),
            'D' => ERROR,
            _ => Color32::from_rgb(0x58, 0x9D, 0xF6),
        };
        let color = if branch {
            color.gamma_multiply(0.6)
        } else {
            color
        };
        if h.kind() == 'D' {
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(x0 - 1.0, rect.top()),
                    Pos2::new(x0 + 5.0, rect.center().y),
                    Pos2::new(x0 - 1.0, rect.bottom()),
                ],
                color,
                egui::Stroke::NONE,
            ));
        } else {
            ui.painter().rect_filled(rect, 1.0, color);
        }
        let r = ui.interact(
            rect.expand2(egui::vec2(3.0, 0.0)),
            ui.scope_id().with(("hunk", branch, i)),
            if branch {
                egui::Sense::hover()
            } else {
                egui::Sense::click()
            },
        );
        if r.clicked() {
            revert = Some(i);
        }
        let r = if branch {
            r
        } else {
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        };
        r.on_hover_ui(|ui| {
            if branch {
                ui.label(
                    RichText::new("Changed on this branch (since its PR base)")
                        .weak()
                        .small(),
                );
            }
            let what = match h.kind() {
                'A' => format!("{} added lines", h.new_len),
                'D' => format!("{} deleted lines", h.old.len()),
                _ => format!("{} lines changed; before:", h.new_len),
            };
            ui.label(RichText::new(what).strong());
            for l in h.old.iter().take(15) {
                ui.label(
                    RichText::new(format!("- {l}"))
                        .monospace()
                        .small()
                        .color(ERROR),
                );
            }
            if h.old.len() > 15 {
                ui.label(
                    RichText::new(format!("… {} more", h.old.len() - 15))
                        .small()
                        .weak(),
                );
            }
            if !branch {
                ui.label(RichText::new("Click to revert this change").weak().small());
            }
        });
    }
    revert
}

#[cfg(test)]
mod tests {
    use super::{SideRow, side_rows};

    fn pair(old: Option<(usize, &str)>, new: Option<(usize, &str)>, changed: bool) -> SideRow {
        let s = |x: Option<(usize, &str)>| x.map(|(n, t)| (n, t.to_string()));
        SideRow::Pair {
            old: s(old),
            new: s(new),
            changed,
        }
    }

    #[test]
    fn removed_and_added_lines_pair_up_side_by_side() {
        let lines: Vec<String> = [
            "diff --git a/A.kt b/A.kt",
            "--- a/A.kt",
            "+++ b/A.kt",
            "@@ -10,4 +10,4 @@ class A",
            " keep",
            "-old one",
            "-old two",
            "+new one",
            " end",
            "+tail",
        ]
        .map(String::from)
        .to_vec();
        let rows = side_rows(&lines);
        assert_eq!(rows[0], SideRow::Head("diff --git a/A.kt b/A.kt".into()));
        assert_eq!(rows[3], SideRow::Head("@@ -10,4 +10,4 @@ class A".into()));
        assert_eq!(
            rows[4..],
            [
                pair(Some((10, "keep")), Some((10, "keep")), false),
                pair(Some((11, "old one")), Some((11, "new one")), true),
                pair(Some((12, "old two")), None, true),
                pair(Some((13, "end")), Some((12, "end")), false),
                pair(None, Some((13, "tail")), true),
            ]
        );
    }
}
