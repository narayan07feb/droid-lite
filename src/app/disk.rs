//! Reacts to files that change on disk (git checkout, another editor, Gradle-generated reports).

use super::{App, tree};
use crate::highlight::WARN;
use crate::kotlin::FileSyms;
use crate::project::is_code;
use egui::RichText;
use std::path::PathBuf;
use std::sync::mpsc::channel;

/// What a worker thread found for one changed path.
pub(super) enum DiskChange {
    /// The file exists: its text, and its symbols if it is code.
    File(PathBuf, String, Option<FileSyms>),
    Gone(PathBuf),
}

impl App {
    /// Collects watcher events and hands them to a worker thread, which reads and scans the files.
    /// A `git checkout` of hundreds of files therefore never blocks the window.
    pub(super) fn poll_disk(&mut self, ctx: &egui::Context) {
        if let Some(w) = &self.watcher {
            let changed = w.changed();
            if !changed.is_empty() {
                let (tx, rx) = channel();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let out: Vec<DiskChange> = changed
                        .into_iter()
                        .map(|p| match std::fs::read_to_string(&p) {
                            Ok(text) => {
                                let syms = is_code(&p).then(|| crate::kotlin::scan(&text));
                                DiskChange::File(p, text, syms)
                            }
                            Err(_) if p.exists() => DiskChange::File(p, String::new(), None), // a folder or unreadable file
                            Err(_) => DiskChange::Gone(p),
                        })
                        .collect();
                    let _ = tx.send(out);
                    c.request_repaint();
                });
                self.disk_rx.push(rx);
            }
        }
        let mut ready = Vec::new();
        self.disk_rx.retain(|rx| match rx.try_recv() {
            Ok(v) => {
                ready.extend(v);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        if !ready.is_empty() {
            self.git.changes_stale = true; // the Git tab's lists may have changed
            self.res_stale |= ready.iter().any(|c| match c {
                DiskChange::File(p, ..) | DiskChange::Gone(p) => {
                    p.components().any(|c| c.as_os_str() == "res")
                }
            });
            self.apply_disk_changes(ready, ctx);
        }
    }

    fn apply_disk_changes(&mut self, changes: Vec<DiskChange>, ctx: &egui::Context) {
        let mut tree_dirty = false;
        let mut reports = false;
        for change in changes {
            let Some(project) = self.project.as_mut() else {
                return;
            };
            match change {
                DiskChange::File(path, text, syms) => {
                    let s = path.to_string_lossy();
                    reports |= path.extension().is_some_and(|e| e == "stability")
                        || s.ends_with("-composables.txt");
                    if !path.is_file() {
                        continue;
                    }
                    if project.accepts(&path)
                        && let Err(at) = project.files.binary_search(&path)
                    {
                        project.files.insert(at, path.clone());
                        tree_dirty = true;
                    }
                    if let Some(syms) = syms
                        && project.files.binary_search(&path).is_ok()
                    {
                        self.index.insert_scanned(path.clone(), syms);
                    }
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.deleted = false;
                        if text == t.text {
                            t.saved = text; // our own save, or the same change
                            t.disk_changed = false;
                        } else if !t.dirty() {
                            t.text = text.clone();
                            t.saved = text;
                            t.cursor = t.cursor.min(t.text.len());
                            t.reanalyse();
                            if let Some(l) = self.lsp.as_mut() {
                                l.did_change(&t.path, &t.text);
                            }
                            self.status =
                                format!("Reloaded {} (changed on disk)", t.path.display());
                        } else {
                            t.disk_changed = true;
                        }
                    }
                }
                DiskChange::Gone(path) => {
                    let before = project.files.len();
                    project.files.retain(|f| !f.starts_with(&path)); // a file, or a deleted folder
                    tree_dirty |= project.files.len() != before;
                    self.index.remove_under(&path);
                    for t in self.tabs.iter_mut().filter(|t| t.path.starts_with(&path)) {
                        t.deleted = true;
                    }
                }
            }
        }
        if tree_dirty && let Some(p) = &self.project {
            self.tree = tree::build(p, self.tree_view);
        }
        if reports && let Some(p) = self.project.take() {
            self.reload_stability(&p, ctx);
            self.project = Some(p);
        }
    }

    /// The bar above the editor when the open file changed or vanished on disk while it had edits.
    pub(super) fn disk_banner(&mut self, ui: &mut egui::Ui) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        if t.deleted {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("This file was deleted on disk. ⌘S writes it back.").color(WARN),
                );
            });
        } else if t.disk_changed {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("This file changed on disk, and you have unsaved edits.")
                        .color(WARN),
                );
                if ui.button("Reload from disk").clicked()
                    && let Ok(text) = std::fs::read_to_string(&t.path)
                {
                    t.text = text.clone();
                    t.saved = text;
                    t.cursor = t.cursor.min(t.text.len());
                    t.disk_changed = false;
                    t.reanalyse();
                }
                if ui.button("Keep mine").clicked() {
                    t.disk_changed = false;
                }
            });
        }
    }
}
