//! Reacts to files that change on disk (git checkout, another editor, Gradle-generated reports).

use super::{App, build_tree};
use crate::highlight::WARN;
use crate::project::is_code;
use egui::RichText;

impl App {
    pub(super) fn poll_disk(&mut self, ctx: &egui::Context) {
        let Some(w) = &self.watcher else { return };
        let changed = w.changed();
        if changed.is_empty() {
            return;
        }
        let mut tree_dirty = false;
        let mut reports = false;
        for path in changed {
            let s = path.to_string_lossy();
            if path.extension().is_some_and(|e| e == "stability") || s.ends_with("-composables.txt")
            {
                reports = true;
            }
            let Some(project) = self.project.as_mut() else {
                return;
            };
            if path.is_file() {
                if project.accepts(&path) && project.files.binary_search(&path).is_err() {
                    let at = project.files.binary_search(&path).unwrap_err();
                    project.files.insert(at, path.clone());
                    tree_dirty = true;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if is_code(&path) && project.files.binary_search(&path).is_ok() {
                    self.index.update(&path, &text);
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
                        self.status = format!("Reloaded {} (changed on disk)", t.path.display());
                    } else {
                        t.disk_changed = true;
                    }
                }
            } else if !path.exists() {
                if let Ok(i) = project.files.binary_search(&path) {
                    project.files.remove(i);
                    tree_dirty = true;
                } else {
                    // a deleted folder: drop every file under it
                    let before = project.files.len();
                    project.files.retain(|f| !f.starts_with(&path));
                    tree_dirty |= project.files.len() != before;
                }
                self.index.files.retain(|f, _| !f.starts_with(&path));
                for t in self.tabs.iter_mut().filter(|t| t.path.starts_with(&path)) {
                    t.deleted = true;
                }
            }
        }
        if tree_dirty && let Some(p) = &self.project {
            self.tree = build_tree(p);
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
