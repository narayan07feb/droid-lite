//! The sidebar's Pull requests view: your open PRs and the ones waiting for your review (through
//! `gh`, off the UI thread). A PR opens to its changed files; a file shows its diff in the diff panel.
//! "Check out" switches the project to a PR's branch (`gh pr checkout`), never over uncommitted work,
//! unsaved edits, a half-done rebase or merge, or a running build. While it runs, editors are
//! read-only and builds, test runs, previews, saves and Git commands wait ([`App::checkout_busy`]).

use super::App;
use crate::git::{self, PrFilter, PrItem};
use crate::icons as ic;
use egui::{RichText, Sense, Ui};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

/// Why a command waits while a checkout runs.
pub(super) const WAIT_CHECKOUT: &str = "Wait for the check out to finish";

type Files = Result<Vec<(String, String)>, String>;
type List = Result<Vec<PrItem>, String>;
/// A list and the HEAD commit read with it.
type Listed = (List, Option<String>);

/// What a `gh pr checkout` thread sends back.
pub(super) struct Checkout {
    /// The project it ran in: a reply for a project closed since then is dropped.
    pub root: PathBuf,
    pub number: u64,
    /// The new branch and whether a submodule needs `git submodule update`, or the error.
    pub result: Result<(String, bool), String>,
    /// The branch and HEAD commit after it, also after a failure: gh may switch, then fail.
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// A `gh pr checkout` thread: its project, PR, stop switch (with gh's process group) and reply.
pub(super) struct Running {
    root: PathBuf,
    number: u64,
    ctl: Arc<git::Ctl>,
    rx: Receiver<Checkout>,
}

#[derive(Default)]
pub(super) struct PrsState {
    /// The sidebar shows the pull requests instead of the Project tree.
    pub shown: bool,
    pub filter: Option<PrFilter>,
    pub lists: HashMap<PrFilter, List>,
    list_rx: Option<(PrFilter, Receiver<Listed>)>,
    /// The commit HEAD points at: the PR whose last commit it is shows "● current".
    pub head: Option<String>,
    /// The opened PR and its files with their diffs.
    pub open: Option<(u64, Option<Files>)>,
    /// The files of PR number `.0`; a reply for a PR closed since then is dropped.
    files_rx: Option<Receiver<(u64, Files)>>,
    /// "Checking out #7…", then its result or why it was refused; `true` for an error.
    pub note: Option<(String, bool)>,
    /// The running `gh pr checkout` of this project.
    checkout: Option<Running>,
    /// Checkouts of projects closed since they started, being stopped: a new one in the same
    /// project waits until each has ended.
    stopping: Vec<Running>,
}

impl PrsState {
    /// Another project opened: its lists, files, note and replies still on their way are
    /// dropped (a running checkout is stopped); the sidebar choice and the filter stay.
    pub(super) fn reset(&mut self) {
        let mut stopping = std::mem::take(&mut self.stopping);
        if let Some(r) = self.checkout.take() {
            r.ctl.stop();
            stopping.push(r);
        }
        *self = PrsState {
            shown: self.shown,
            filter: self.filter,
            stopping,
            ..Default::default()
        };
    }

    /// A checkout of `root` being stopped, whose reply the test sends.
    #[cfg(test)]
    pub(super) fn add_stopping(
        &mut self,
        root: &Path,
        number: u64,
    ) -> std::sync::mpsc::Sender<Checkout> {
        let (tx, rx) = channel();
        self.stopping.push(Running {
            root: root.to_path_buf(),
            number,
            ctl: Default::default(),
            rx,
        });
        tx
    }

    /// How many checkouts of closed projects are still being stopped.
    #[cfg(test)]
    pub(super) fn stopping_count(&self) -> usize {
        self.stopping.len()
    }

    /// A checkout of `root` runs or is being stopped.
    fn running_in(&self, root: &Path) -> Option<&Running> {
        self.checkout
            .iter()
            .chain(&self.stopping)
            .find(|r| r.root == root)
    }
}

/// Opens a link in Google Chrome, else the default browser.
fn open_url(url: &str) {
    let chrome = std::process::Command::new("open")
        .args(["-a", "Google Chrome", url])
        .status()
        .is_ok_and(|s| s.success());
    if !chrome {
        let _ = std::process::Command::new("open").arg(url).status();
    }
}

const APPROVED: egui::Color32 = egui::Color32::from_rgb(0x5F, 0xB8, 0x65);

fn review_text(r: &str) -> Option<(&'static str, egui::Color32)> {
    match r {
        "APPROVED" => Some(("Approved", APPROVED)),
        "CHANGES_REQUESTED" => Some(("Changes requested", crate::highlight::ERROR)),
        "REVIEW_REQUIRED" => Some(("Review required", crate::highlight::WARN)),
        _ => None,
    }
}

impl App {
    pub(super) fn prs_panel(&mut self, ui: &mut Ui) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            ui.label("Open a project to see its pull requests.");
            return;
        };
        let filter = *self.prs.filter.get_or_insert(PrFilter::Mine);
        let mut refresh = false;
        ui.horizontal(|ui| {
            let mut tabs = crate::theme::Tabs::new(ui, "pr-filter");
            for f in [PrFilter::Mine, PrFilter::Review, PrFilter::Approved] {
                if tabs.tab(ui, filter == f, f.label()).clicked() {
                    self.prs.filter = Some(f);
                }
            }
            tabs.finish(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.prs.list_rx.is_some() {
                    ui.spinner();
                } else if ui
                    .add(egui::Button::new(ic::ARROW_COUNTER_CLOCKWISE).frame(false))
                    .on_hover_text("Refresh")
                    .clicked()
                {
                    refresh = true;
                }
            });
        });
        let filter = self.prs.filter.unwrap_or(PrFilter::Mine);
        if refresh {
            self.prs.lists.remove(&filter);
        }
        // load the list once per filter; gh uses the network, so never on the UI thread
        if !self.prs.lists.contains_key(&filter) && self.prs.list_rx.is_none() {
            let (tx, rx) = channel();
            let (r, c) = (root.clone(), ui.ctx().clone());
            std::thread::spawn(move || {
                let _ = tx.send((git::list_prs(&r, filter), git::head_sha(&r)));
                c.request_repaint();
            });
            self.prs.list_rx = Some((filter, rx));
        }
        ui.separator();
        if let Some((text, error)) = &self.prs.note {
            ui.horizontal(|ui| {
                if self.prs.checkout.is_some() {
                    ui.spinner();
                }
                let t = RichText::new(text);
                ui.label(if *error {
                    t.color(crate::highlight::WARN)
                } else {
                    t
                });
            });
            ui.separator();
        }
        let list = match self.prs.lists.get(&filter) {
            None => {
                ui.label(RichText::new("Loading pull requests…").weak());
                return;
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("gh: {e}")).color(crate::highlight::ERROR));
                return;
            }
            Some(Ok(l)) => l.clone(),
        };
        if list.is_empty() {
            ui.label(
                RichText::new(match filter {
                    PrFilter::Mine => "You have no open pull requests.",
                    PrFilter::Review => "No pull requests wait for your review.",
                    PrFilter::Approved => "None of your open pull requests is approved yet.",
                })
                .weak(),
            );
            return;
        }
        let mut toggle = None;
        let mut diff = None;
        let mut checkout = None;
        let busy = self.prs.checkout.is_some();
        egui::ScrollArea::vertical()
            .id_salt("prs")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for p in &list {
                    let opened = self.prs.open.as_ref().filter(|(n, _)| *n == p.number);
                    let r = ui
                        .horizontal(|ui| {
                            ui.label(
                                RichText::new(if opened.is_some() { "▾" } else { "▸" }).weak(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add(egui::Button::new(ic::LINK_SIMPLE).frame(false))
                                    .on_hover_text(format!("Open on GitHub\n{}", p.url))
                                    .clicked()
                                {
                                    open_url(&p.url);
                                }
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        // approved at a glance: a green check before the title
                                        if p.review == "APPROVED" {
                                            ui.label(RichText::new(ic::CHECK).color(APPROVED))
                                                .on_hover_text("Approved");
                                        }
                                        ui.add(
                                            egui::Label::new(format!("#{} {}", p.number, p.title))
                                                .truncate()
                                                .sense(Sense::click()),
                                        )
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(&p.title)
                                    },
                                )
                                .inner
                            })
                            .inner
                        })
                        .inner;
                    if r.clicked() {
                        toggle = Some(p.number);
                    }
                    // the second line: author, draft, review state; Check out at the right
                    ui.horizontal(|ui| {
                        ui.add_space(18.0);
                        // the commit, not the branch name: a fork's PR may be named main too
                        if !p.head_oid.is_empty()
                            && self.prs.head.as_deref() == Some(p.head_oid.as_str())
                        {
                            ui.label(RichText::new("● current").small().color(APPROVED))
                                .on_hover_text(format!("{} is checked out", p.head));
                        }
                        ui.label(RichText::new(&p.author).weak().small());
                        if p.draft {
                            ui.label(RichText::new("Draft").weak().small());
                        }
                        if let Some((t, c)) = review_text(&p.review) {
                            let t = if p.approvals > 0 && p.review == "APPROVED" {
                                format!("Approved by {}", p.approvals)
                            } else {
                                t.to_string()
                            };
                            ui.label(RichText::new(t).small().color(c));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(RichText::new("Check out").small()),
                                )
                                .on_hover_text(format!("gh pr checkout {}", p.number))
                                .clicked()
                            {
                                checkout = Some(p.number);
                            }
                        });
                    });
                    let Some((_, files)) = opened else { continue };
                    match files {
                        None => {
                            ui.horizontal(|ui| {
                                ui.add_space(18.0);
                                ui.spinner();
                                ui.label(RichText::new("Loading files…").weak());
                            });
                        }
                        Some(Err(e)) => {
                            ui.label(
                                RichText::new(format!("gh: {e}")).color(crate::highlight::ERROR),
                            );
                        }
                        Some(Ok(files)) => {
                            for (path, d) in files {
                                let name = path.rsplit('/').next().unwrap_or(path);
                                ui.horizontal(|ui| {
                                    ui.add_space(18.0);
                                    let link = ui.visuals().hyperlink_color;
                                    if ui
                                        .add(
                                            egui::Label::new(RichText::new(name).color(link))
                                                .truncate()
                                                .sense(Sense::click()),
                                        )
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(format!("{path}\nClick to see the diff"))
                                        .clicked()
                                    {
                                        diff = Some((format!("#{} {path}", p.number), d.clone()));
                                    }
                                });
                            }
                        }
                    }
                    ui.add_space(4.0);
                }
            });
        if let Some(n) = toggle {
            if self.prs.open.as_ref().is_some_and(|(o, _)| *o == n) {
                self.prs.open = None;
            } else {
                self.prs.open = Some((n, None));
                let (tx, rx) = channel();
                let (r, c) = (root.clone(), ui.ctx().clone());
                std::thread::spawn(move || {
                    let _ = tx.send((n, git::pr_diff(&r, n)));
                    c.request_repaint();
                });
                self.prs.files_rx = Some(rx);
            }
        }
        if let Some((title, text)) = diff {
            self.show_diff_text(title, text);
        }
        if let Some(n) = checkout {
            self.check_out(n, root, ui.ctx());
        }
    }

    /// Replies of the PR threads, every frame (also while the sidebar shows the Project tree).
    pub(super) fn poll_prs(&mut self) {
        if let Some((f, rx)) = &self.prs.list_rx
            && let Ok((list, head)) = rx.try_recv()
        {
            self.prs.lists.insert(*f, list);
            self.prs.head = head;
            self.prs.list_rx = None;
        }
        if let Some(r) = &self.prs.checkout
            && let Ok(reply) = r.rx.try_recv()
        {
            self.prs.checkout = None;
            self.checked_out(reply);
        }
        // a stopped checkout reports once gh has ended; only its own project shows it. While a
        // project opens its reply waits (it may be that project's), and the checkout stays busy
        if self.indexing.is_some() {
            return self.poll_pr_files();
        }
        let mut ended = Vec::new();
        self.prs.stopping.retain(|r| match r.rx.try_recv() {
            Ok(reply) => {
                ended.push(reply);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for reply in ended {
            self.checked_out(reply);
        }
        self.poll_pr_files();
    }

    fn poll_pr_files(&mut self) {
        if let Some(rx) = &self.prs.files_rx
            && let Ok((n, files)) = rx.try_recv()
        {
            if let Some((open, f)) = &mut self.prs.open
                && *open == n
            {
                *f = Some(files);
            }
            self.prs.files_rx = None;
        }
    }

    /// Why Check out must wait: a build, test run or preview of this project is running (a
    /// branch switch under it breaks it), or editor tabs have unsaved edits (they would end up on
    /// the other branch, or be lost when the files reload).
    fn checkout_refusal(&self, root: &Path) -> Option<String> {
        if let Some(r) = self.prs.running_in(root) {
            return Some(format!("Wait for the check out of #{} to stop", r.number));
        }
        let running = if self.gradle.is_some() {
            Some("the Gradle build")
        } else if self.tests.running() {
            Some("the test run")
        } else if self.previews.busy {
            Some("the preview draw")
        } else {
            None
        };
        if let Some(r) = running {
            return Some(format!("Wait for {r} to finish"));
        }
        let unsaved: Vec<&Path> = self
            .tabs
            .iter()
            .filter(|t| t.dirty())
            .map(|t| t.path.as_path())
            .collect();
        (!unsaved.is_empty())
            .then(|| git::file_list("Save or discard your unsaved edits first:", root, unsaved))
    }

    /// Runs `gh pr checkout n` on a thread, unless [`App::checkout_refusal`] says no.
    fn check_out(&mut self, n: u64, root: PathBuf, ctx: &egui::Context) {
        if let Some(e) = self.checkout_refusal(&root) {
            self.status = e.lines().next().unwrap_or_default().to_string();
            self.prs.note = Some((e, true));
            return;
        }
        let text = format!("Checking out #{n}…");
        self.status = text.clone();
        self.prs.note = Some((text, false));
        let (tx, rx) = channel();
        let c = ctx.clone();
        let ctl = Arc::new(git::Ctl::default());
        let (r, stop) = (root.clone(), ctl.clone());
        std::thread::spawn(move || {
            let result = git::checkout_pr(&r, n, &git::gh_program(), git::CHECKOUT_TIMEOUT, &stop);
            let (branch, head) = (git::branch(&r), git::head_sha(&r));
            let _ = tx.send(Checkout {
                root: r,
                number: n,
                result,
                branch,
                head,
            });
            c.request_repaint();
        });
        self.prs.checkout = Some(Running {
            root,
            number: n,
            ctl,
            rx,
        });
    }

    /// A `gh pr checkout` of this project runs (or one of it is still being stopped): its files
    /// may change under the editor, so edits, saves, builds, test runs, previews and Git
    /// commands wait.
    pub(super) fn checkout_busy(&self) -> bool {
        self.prs.checkout.is_some()
            || self
                .project
                .as_ref()
                .is_some_and(|p| self.prs.running_in(&p.root).is_some())
    }

    /// [`App::checkout_busy`]; the status bar then says to wait.
    pub(super) fn refuse_during_checkout(&mut self) -> bool {
        let busy = self.checkout_busy();
        if busy {
            self.status = WAIT_CHECKOUT.into();
        }
        busy
    }

    /// On quit: stops every running checkout (SIGTERM, SIGKILL after [`git::STOP_GRACE`]) and
    /// waits for each thread to report. The numbers of the PRs whose checkout was stopped.
    pub(super) fn stop_checkouts(&mut self) -> Vec<u64> {
        let all: Vec<Running> = self
            .prs
            .checkout
            .take()
            .into_iter()
            .chain(std::mem::take(&mut self.prs.stopping))
            .collect();
        for r in &all {
            r.ctl.stop();
        }
        let limit = git::STOP_GRACE + std::time::Duration::from_secs(3);
        all.into_iter()
            .map(|r| {
                let _ = r.rx.recv_timeout(limit);
                r.number
            })
            .collect()
    }

    /// After `gh pr checkout`, success or not (gh may have switched before it failed): the branch
    /// name, the Current changes tab and tree colours refresh here, and the PR lists after a
    /// success; the tree and open files follow the files on disk (the watcher). A reply from a
    /// project closed since then changes nothing.
    pub(super) fn checked_out(&mut self, reply: Checkout) {
        if self.project.as_ref().map(|p| &p.root) != Some(&reply.root) {
            return;
        }
        self.branch = reply.branch;
        self.prs.head = reply.head;
        self.git.changes_stale = true;
        match reply.result {
            Ok((branch, submodules)) => {
                self.close_gone_tabs();
                let mut text = format!("Checked out #{} ({branch})", reply.number);
                if submodules {
                    text.push_str(". Submodules point at other commits: run git submodule update");
                }
                self.prs.lists.clear();
                self.status = text.clone();
                self.prs.note = Some((text, false));
            }
            Err(e) => {
                self.status = e.lines().next().unwrap_or_default().to_string();
                self.prs.note = Some((e, true));
            }
        }
    }

    /// After a checkout: tabs without edits whose file the other branch does not have (renamed or
    /// deleted) close; ⌘S would write the old file back. Tabs with edits keep their bar.
    fn close_gone_tabs(&mut self) {
        for i in (0..self.tabs.len()).rev() {
            let t = &self.tabs[i];
            if !t.dirty() && !t.readonly && !t.path.exists() {
                let t = self.tabs.remove(i);
                self.forget_tab(&t);
                if self.active > i {
                    self.active -= 1;
                }
            }
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }
}
