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
    /// It exists but cannot be read as text (not UTF-8, no permission, a folder): why.
    Unreadable(PathBuf, String),
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
                                let syms = is_code(&p).then(|| crate::kotlin::scan_file(&p, &text));
                                DiskChange::File(p, text, syms)
                            }
                            Err(_) if !p.exists() => DiskChange::Gone(p),
                            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                                DiskChange::Unreadable(p, "it is not UTF-8 text".into())
                            }
                            Err(e) => DiskChange::Unreadable(p, e.to_string()),
                        })
                        .collect();
                    let _ = tx.send(out);
                    c.request_repaint();
                });
                self.disk_rx.push(rx);
            }
        }
        // batches apply in the order they were sent: a newer one that is read first waits
        let mut ready = Vec::new();
        while let Some(rx) = self.disk_rx.first() {
            match rx.try_recv() {
                Ok(v) => ready.extend(v),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
            self.disk_rx.remove(0);
        }
        if !ready.is_empty() {
            self.git.changes_stale = true; // the Current changes tab's lists may have changed
            self.res_stale |= ready.iter().any(|c| match c {
                DiskChange::File(p, ..) | DiskChange::Unreadable(p, _) | DiskChange::Gone(p) => {
                    p.components().any(|c| c.as_os_str() == "res")
                        || p.file_name().is_some_and(|n| n == "AndroidManifest.xml")
                }
            });
            self.apply_disk_changes(ready, ctx);
        }
    }

    fn apply_disk_changes(&mut self, changes: Vec<DiskChange>, ctx: &egui::Context) {
        if let Some(l) = self.lsp.as_mut() {
            let watched: Vec<(PathBuf, u8)> = changes
                .iter()
                .filter_map(|c| match c {
                    DiskChange::File(p, _, Some(_)) => Some((p.clone(), 2)),
                    DiskChange::Gone(p) => Some((p.clone(), 3)),
                    DiskChange::File(..) | DiskChange::Unreadable(..) => None,
                })
                .collect();
            l.files_changed(&watched);
        }
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
                        let p = path.clone();
                        self.index.change(move |i| i.insert_scanned(p, syms));
                    }
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.deleted = false;
                        t.unreadable = None;
                        if text == t.text || text == t.saved {
                            // our own save (also one that arrives after more typing), or the same
                            // change
                            t.saved = text;
                            t.disk_changed = false;
                        } else if !t.dirty() {
                            t.text = text.clone();
                            t.saved = text;
                            t.cursor = t.cursor.min(t.text.len());
                            t.disk_changed = false;
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
                DiskChange::Unreadable(path, why) => {
                    if path.is_file()
                        && project.accepts(&path)
                        && let Err(at) = project.files.binary_search(&path)
                    {
                        project.files.insert(at, path.clone());
                        tree_dirty = true;
                    }
                    // the tab keeps its text: an empty or garbled read never replaces it
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.deleted = false;
                        t.disk_changed = true;
                        t.unreadable = Some(why);
                    }
                }
                DiskChange::Gone(path) => {
                    let before = project.files.len();
                    project.files.retain(|f| !f.starts_with(&path)); // a file, or a deleted folder
                    tree_dirty |= project.files.len() != before;
                    let p = path.clone();
                    self.index.change(move |i| i.remove_under(&p));
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
            let text = match (&t.unreadable, t.dirty()) {
                (Some(why), _) => format!(
                    "This file changed on disk and cannot be read as text now ({why}). The editor keeps your version."
                ),
                (None, true) => "This file changed on disk, and you have unsaved edits.".into(),
                (None, false) => "File changed on disk; reload or keep yours.".into(),
            };
            let mut status = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).color(WARN));
                if ui.button("Reload from disk").clicked() {
                    status = reload(t).err();
                }
                if ui.button("Keep mine").clicked() {
                    // what is on disk counts as loaded: ⌘S and auto-save write this version over it
                    if let Ok(disk) = std::fs::read(&t.path) {
                        t.saved = String::from_utf8_lossy(&disk).into_owned();
                    }
                    t.disk_changed = false;
                    t.unreadable = None;
                }
            });
            if let Some(s) = status {
                self.status = s;
            }
        }
    }
}

/// Puts the file on disk in tab `t`; its edits go to local history first. Else why not.
fn reload(t: &mut super::Tab) -> Result<(), String> {
    let text = match std::fs::read(&t.path).map(String::from_utf8) {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => {
            return Err(format!(
                "Cannot reload {}: it is not UTF-8 text",
                t.path.display()
            ));
        }
        Err(e) => return Err(format!("Cannot reload {}: {e}", t.path.display())),
    };
    if t.dirty() {
        // the edits it drops
        super::comfort::record_text(&t.path, t.text.as_bytes())
            .map_err(|e| super::comfort::copy_failed(&t.path, &e))?;
    }
    t.text = text.clone();
    t.saved = text;
    t.cursor = t.cursor.min(t.text.len());
    t.disk_changed = false;
    t.unreadable = None;
    t.reanalyse();
    Ok(())
}
