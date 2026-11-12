//! Rename (with a preview), optimize imports, extract variable, and undo of the last refactoring.

use super::{App, build_tree, word_bounds};
use crate::lsp;
use crate::project::is_code;
use crate::refactor;
use egui::{Align2, Key, Modifiers, RichText, TextEdit};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

pub(super) enum Refactor {
    RenameInput {
        path: PathBuf,
        at: usize,
        old: String,
        new: String,
    },
    /// Waiting for kotlin-lsp (`rx` is None) or for the project text search.
    Searching {
        old: String,
        new: String,
        since: Instant,
        rx: Option<Receiver<Vec<FileEdits>>>,
    },
    Preview(Preview),
    Extract {
        sel: Range<usize>,
        name: String,
    },
}

pub(super) struct Preview {
    old: String,
    new: String,
    files: Vec<FileEdits>,
    /// `Old.kt` → `New.kt`, and whether to do it.
    rename_file: Option<(PathBuf, PathBuf, bool)>,
    source: &'static str,
}

pub(super) struct FileEdits {
    path: PathBuf,
    /// The text the edits were computed against; apply refuses if the file changed since.
    text: String,
    edits: Vec<EditLine>,
}

struct EditLine {
    range: Range<usize>,
    new_text: String,
    line: usize,
    preview: String,
    include: bool,
}

/// What the last refactoring changed, to undo it.
pub(super) struct Undo {
    label: String,
    files: Vec<(PathBuf, String)>,
    moved: Option<(PathBuf, PathBuf)>,
}

impl Preview {
    #[cfg(test)]
    pub(super) fn source_name(&self) -> &str {
        self.source
    }
}

fn edit_line(text: &str, range: Range<usize>, new_text: String) -> EditLine {
    let ls = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let le = text[range.end..]
        .find('\n')
        .map_or(text.len(), |i| range.end + i);
    let preview = format!(
        "{}[{}]{}",
        &text[ls..range.start],
        new_text,
        &text[range.end..le]
    )
    .trim()
    .chars()
    .take(160)
    .collect();
    EditLine {
        line: text[..range.start].matches('\n').count() + 1,
        range,
        new_text,
        preview,
        include: true,
    }
}

impl App {
    /// The current text of a file: the open tab if there is one, else the disk.
    fn current_text(&self, path: &Path) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.path == path)
            .map(|t| t.text.clone())
            .or_else(|| std::fs::read_to_string(path).ok())
    }

    pub(super) fn start_rename(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let (a, b) = word_bounds(&tab.text, tab.cursor);
        if a == b {
            self.status = "Put the cursor on a name to rename it".into();
            return;
        }
        let old = tab.text[a..b].to_string();
        self.refactor = Some(Refactor::RenameInput {
            path: tab.path.clone(),
            at: a,
            new: old.clone(),
            old,
        });
    }

    fn start_text_search(&mut self, old: String, new: String) {
        let Some(p) = &self.project else { return };
        let open: HashMap<PathBuf, String> = self
            .tabs
            .iter()
            .map(|t| (t.path.clone(), t.text.clone()))
            .collect();
        let files: Vec<PathBuf> = p.files.iter().filter(|f| is_code(f)).cloned().collect();
        let (tx, rx) = channel();
        let (o, n) = (old.clone(), new.clone());
        std::thread::spawn(move || {
            let out: Vec<FileEdits> = files
                .into_iter()
                .filter_map(|path| {
                    let text = open
                        .get(&path)
                        .cloned()
                        .or_else(|| std::fs::read_to_string(&path).ok())?;
                    let edits: Vec<EditLine> = refactor::occurrences(&text, &o)
                        .into_iter()
                        .map(|r| edit_line(&text, r, n.clone()))
                        .collect();
                    (!edits.is_empty()).then_some(FileEdits { path, text, edits })
                })
                .collect();
            let _ = tx.send(out);
        });
        self.refactor = Some(Refactor::Searching {
            old,
            new,
            since: Instant::now(),
            rx: Some(rx),
        });
    }

    /// Kotlin-lsp answered a rename; an empty answer falls back to the text search.
    pub(super) fn on_lsp_rename(&mut self, edits: lsp::WorkspaceEdits) {
        let Some(Refactor::Searching {
            old, new, rx: None, ..
        }) = &self.refactor
        else {
            return;
        };
        let (old, new) = (old.clone(), new.clone());
        let files: Vec<FileEdits> = edits
            .into_iter()
            .filter_map(|(path, es)| {
                let text = self.current_text(&path)?;
                let edits = es
                    .into_iter()
                    .map(|(l1, c1, l2, c2, t)| {
                        edit_line(
                            &text,
                            lsp::offset(&text, l1, c1)..lsp::offset(&text, l2, c2),
                            t,
                        )
                    })
                    .collect();
                Some(FileEdits { path, text, edits })
            })
            .collect();
        if files.iter().all(|f| f.edits.is_empty()) {
            self.status = "kotlin-lsp could not rename this; using the project text search".into();
            self.start_text_search(old, new);
        } else {
            self.show_preview(old, new, files, "kotlin-lsp");
        }
    }

    fn show_preview(
        &mut self,
        old: String,
        new: String,
        files: Vec<FileEdits>,
        source: &'static str,
    ) {
        // a class in a file of the same name: offer to rename the file too
        let rename_file = self.index.files.iter().find_map(|(path, syms)| {
            let stem = path.file_stem()?.to_str()?;
            let ext = path.extension()?.to_str()?;
            (stem == old && syms.decls.iter().any(|d| d.name == old && d.kind.is_type())).then(
                || {
                    (
                        path.clone(),
                        path.with_file_name(format!("{new}.{ext}")),
                        true,
                    )
                },
            )
        });
        self.refactor = Some(Refactor::Preview(Preview {
            old,
            new,
            files,
            rename_file,
            source,
        }));
    }

    fn apply_preview(&mut self, p: Preview) {
        let mut undo = Undo {
            label: format!("rename {} → {}", p.old, p.new),
            files: Vec::new(),
            moved: None,
        };
        let mut skipped = Vec::new();
        let mut changed = 0;
        for f in &p.files {
            let edits: Vec<(Range<usize>, String)> = f
                .edits
                .iter()
                .filter(|e| e.include)
                .map(|e| (e.range.clone(), e.new_text.clone()))
                .collect();
            if edits.is_empty() {
                continue;
            }
            if self.current_text(&f.path).as_deref() != Some(f.text.as_str()) {
                skipped.push(f.path.display().to_string());
                continue;
            }
            let new_text = refactor::apply(&f.text, &edits);
            if self.write_file(&f.path, &new_text) {
                undo.files.push((f.path.clone(), f.text.clone()));
                changed += 1;
            }
        }
        if let Some((from, to, true)) = &p.rename_file {
            if to.exists() {
                skipped.push(format!("{} (already exists)", to.display()));
            } else if std::fs::rename(from, to).is_ok() {
                self.moved(from, to);
                undo.moved = Some((from.clone(), to.clone()));
            }
        }
        self.status = format!("Renamed {} → {} in {changed} files", p.old, p.new);
        if !skipped.is_empty() {
            self.status.push_str(&format!(
                "; skipped (changed meanwhile): {}",
                skipped.join(", ")
            ));
        }
        self.undo = Some(undo);
    }

    /// Writes a file and keeps the open tab, the index and kotlin-lsp in step.
    fn write_file(&mut self, path: &Path, text: &str) -> bool {
        if let Err(e) = std::fs::write(path, text) {
            self.status = format!("Cannot write {}: {e}", path.display());
            return false;
        }
        self.index.update(path, text);
        if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
            t.text = text.to_string();
            t.saved = text.to_string();
            t.cursor = t.cursor.min(text.len());
            t.reanalyse();
        }
        if let Some(l) = self.lsp.as_mut().filter(|_| is_code(path)) {
            l.did_change(path, text);
            l.did_save(path);
        }
        true
    }

    /// A file moved: update tabs, the file list, the tree and the index.
    pub(super) fn moved(&mut self, from: &Path, to: &Path) {
        for t in self.tabs.iter_mut().filter(|t| t.path == from) {
            t.path = to.to_path_buf();
        }
        if let Some(p) = self.project.as_mut() {
            p.files.retain(|f| f != from);
            p.files.push(to.to_path_buf());
            p.files.sort();
            self.tree = build_tree(p);
        }
        self.index.remove(from);
        if let Ok(text) = std::fs::read_to_string(to) {
            self.index.update(to, &text);
        }
    }

    pub(super) fn undo_refactor(&mut self) {
        let Some(u) = self.undo.take() else { return };
        if let Some((from, to)) = &u.moved
            && std::fs::rename(to, from).is_ok()
        {
            self.moved(to, from);
        }
        for (path, text) in &u.files {
            self.write_file(path, text);
        }
        self.status = format!("Undid {}", u.label);
    }

    pub(super) fn optimize_imports(&mut self) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        match refactor::optimize_imports(&t.text) {
            Some(new) => {
                let removed =
                    t.text.matches("\nimport ").count() - new.matches("\nimport ").count();
                t.text = new;
                t.cursor = t.cursor.min(t.text.len());
                t.goto = Some(t.cursor);
                t.reanalyse();
                let (path, text) = (t.path.clone(), t.text.clone());
                self.index.update(&path, &text);
                if let Some(l) = self.lsp.as_mut() {
                    l.did_change(&path, &text);
                }
                self.status = format!("Removed {removed} unused imports (press ⌘S to save)");
            }
            None => self.status = "Imports are already clean".into(),
        }
    }

    pub(super) fn start_extract(&mut self) {
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        if t.selection.is_empty() {
            self.status = "Select an expression first, then press ⌥⌘V".into();
            return;
        }
        let name = refactor::suggest_name(&t.text[t.selection.clone()]);
        self.refactor = Some(Refactor::Extract {
            sel: t.selection.clone(),
            name,
        });
    }

    pub(super) fn refactor_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.refactor.take() else {
            return;
        };
        let esc = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        if esc {
            return;
        }
        let window = |title: &str| {
            egui::Window::new(title.to_string())
                .collapsible(false)
                .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        };
        match state {
            Refactor::RenameInput {
                path,
                at,
                old,
                mut new,
            } => {
                let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                let valid = refactor::valid_identifier(&new) && new != old;
                window(&format!("Rename `{old}`"))
                    .default_width(420.0)
                    .show(ctx, |ui| {
                        ui.add(TextEdit::singleline(&mut new).desired_width(f32::INFINITY))
                            .request_focus();
                        if !refactor::valid_identifier(&new) {
                            ui.label(
                                RichText::new("Not a valid Kotlin name")
                                    .color(crate::highlight::ERROR),
                            );
                        }
                        ui.label(
                            RichText::new(
                                "Enter to see the changes before they are made. Esc to cancel.",
                            )
                            .weak()
                            .small(),
                        );
                    });
                if enter && valid {
                    let text = self.current_text(&path).unwrap_or_default();
                    match self.lsp.as_mut().filter(|l| l.ready) {
                        Some(l) => {
                            let (line, col) = lsp::position(&text, at);
                            l.rename(&path, line, col, &new);
                            self.refactor = Some(Refactor::Searching {
                                old,
                                new,
                                since: Instant::now(),
                                rx: None,
                            });
                        }
                        None => self.start_text_search(old, new),
                    }
                } else {
                    self.refactor = Some(Refactor::RenameInput { path, at, old, new });
                }
            }
            Refactor::Searching {
                old,
                new,
                since,
                rx,
            } => {
                if let Some(r) = &rx
                    && let Ok(files) = r.try_recv()
                {
                    self.show_preview(old, new, files, "text search (check each line)");
                    return;
                }
                if rx.is_none() && since.elapsed().as_secs() > 8 {
                    self.status = "kotlin-lsp did not answer; using the project text search".into();
                    self.start_text_search(old, new);
                    return;
                }
                window("Rename").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Finding every use of `{old}`…"));
                    });
                });
                ctx.request_repaint();
                self.refactor = Some(Refactor::Searching {
                    old,
                    new,
                    since,
                    rx,
                });
            }
            Refactor::Preview(mut p) => {
                let mut apply = false;
                let mut cancel = false;
                let mut open: Option<(PathBuf, usize)> = None;
                let root = self
                    .project
                    .as_ref()
                    .map(|p| p.root.clone())
                    .unwrap_or_default();
                window(&format!("Rename `{}` → `{}`", p.old, p.new)).default_width(760.0).show(ctx, |ui| {
                    let total: usize = p.files.iter().map(|f| f.edits.iter().filter(|e| e.include).count()).sum();
                    ui.label(format!("{total} changes in {} files. Source: {}.", p.files.len(), p.source));
                    if p.source.starts_with("text") {
                        ui.label(RichText::new("Text search finds every identifier with this name. Untick lines that belong to a different symbol.").color(crate::highlight::WARN).small());
                    }
                    if let Some((from, to, on)) = p.rename_file.as_mut() {
                        ui.checkbox(on, format!("Also rename file {} → {}", from.file_name().unwrap_or_default().to_string_lossy(), to.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    egui::ScrollArea::vertical().max_height(440.0).show(ui, |ui| {
                        let many = p.files.len() > 12;
                        for f in p.files.iter_mut() {
                            let rel = f.path.strip_prefix(&root).unwrap_or(&f.path).display().to_string();
                            egui::CollapsingHeader::new(format!("{rel} ({})", f.edits.len())).default_open(!many).show(ui, |ui| {
                                for e in f.edits.iter_mut() {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut e.include, "");
                                        if ui.link(RichText::new(format!("{:>5}: {}", e.line, e.preview)).monospace().small()).clicked() {
                                            open = Some((f.path.clone(), e.range.start));
                                        }
                                    });
                                }
                            });
                        }
                    });
                    ui.horizontal(|ui| {
                        apply = ui.button(RichText::new("Apply").strong()).clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
                if let Some((path, at)) = open {
                    self.open_file(&path, Some(at));
                }
                if apply {
                    self.apply_preview(p);
                } else if !cancel {
                    self.refactor = Some(Refactor::Preview(p));
                }
            }
            Refactor::Extract { sel, mut name } => {
                let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                window("Extract variable")
                    .default_width(360.0)
                    .show(ctx, |ui| {
                        ui.add(TextEdit::singleline(&mut name).desired_width(f32::INFINITY))
                            .request_focus();
                        ui.label(
                            RichText::new("Enter to extract. Esc to cancel.")
                                .weak()
                                .small(),
                        );
                    });
                let done = enter
                    && self.tabs.get_mut(self.active).is_some_and(|t| {
                        match refactor::extract_variable(&t.text, sel.clone(), &name) {
                            Some((text, at)) => {
                                t.text = text;
                                t.goto = Some(at);
                                t.reanalyse();
                                true
                            }
                            None => false,
                        }
                    });
                if done {
                    let t = &self.tabs[self.active];
                    let (path, text) = (t.path.clone(), t.text.clone());
                    self.index.update(&path, &text);
                    if let Some(l) = self.lsp.as_mut() {
                        l.did_change(&path, &text);
                    }
                } else {
                    self.refactor = Some(Refactor::Extract { sel, name });
                }
            }
        }
    }

    pub(super) fn refactor_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Refactor", |ui| {
            if ui.button("Rename…  ⇧F6").clicked() {
                self.start_rename();
                ui.close();
            }
            if ui.button("Extract variable…  ⌥⌘V").clicked() {
                self.start_extract();
                ui.close();
            }
            if ui.button("Optimize imports  ⌃⌥O").clicked() {
                self.optimize_imports();
                ui.close();
            }
            ui.separator();
            let label = self
                .undo
                .as_ref()
                .map_or("Undo last refactoring".to_string(), |u| {
                    format!("Undo {}", u.label)
                });
            if ui
                .add_enabled(self.undo.is_some(), egui::Button::new(label))
                .clicked()
            {
                self.undo_refactor();
                ui.close();
            }
        });
    }
}
