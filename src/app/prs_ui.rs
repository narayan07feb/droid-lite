//! The sidebar's Pull requests view: your open PRs and the ones waiting for your review (through
//! `gh`, off the UI thread). A PR opens to its changed files; a file shows its diff in the diff panel.

use super::App;
use crate::git::{self, PrFilter, PrItem};
use crate::icons as ic;
use egui::{RichText, Sense, Ui};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};

type Files = Result<Vec<(String, String)>, String>;
type List = Result<Vec<PrItem>, String>;

#[derive(Default)]
pub(super) struct PrsState {
    /// The sidebar shows the pull requests instead of the Project tree.
    pub shown: bool,
    pub filter: Option<PrFilter>,
    pub lists: HashMap<PrFilter, List>,
    list_rx: Option<(PrFilter, Receiver<List>)>,
    /// The opened PR and its files with their diffs.
    pub open: Option<(u64, Option<Files>)>,
    /// The files of PR number `.0`; a reply for a PR closed since then is dropped.
    files_rx: Option<Receiver<(u64, Files)>>,
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

fn review_text(r: &str) -> Option<(&'static str, egui::Color32)> {
    match r {
        "APPROVED" => Some(("Approved", egui::Color32::from_rgb(0x5F, 0xB8, 0x65))),
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
            for f in [PrFilter::Mine, PrFilter::Review] {
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
                let _ = tx.send(git::list_prs(&r, filter));
                c.request_repaint();
            });
            self.prs.list_rx = Some((filter, rx));
        }
        if let Some((f, rx)) = &self.prs.list_rx
            && let Ok(list) = rx.try_recv()
        {
            self.prs.lists.insert(*f, list);
            self.prs.list_rx = None;
        }
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
        ui.separator();
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
                })
                .weak(),
            );
            return;
        }
        let mut toggle = None;
        let mut diff = None;
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
                    // the second line: author, draft, review state
                    ui.horizontal(|ui| {
                        ui.add_space(18.0);
                        ui.label(RichText::new(&p.author).weak().small());
                        if p.draft {
                            ui.label(RichText::new("Draft").weak().small());
                        }
                        if let Some((t, c)) = review_text(&p.review) {
                            ui.label(RichText::new(t).small().color(c));
                        }
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
                let c = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send((n, git::pr_diff(&root, n)));
                    c.request_repaint();
                });
                self.prs.files_rx = Some(rx);
            }
        }
        if let Some((title, text)) = diff {
            self.show_diff_text(title, text);
        }
    }
}
