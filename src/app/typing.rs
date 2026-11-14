//! Smart typing in the editor (Enter indent, bracket pairs) and ⌘/Ctrl-hover quick info with usages.

use super::{App, FoundLine, found_line, word_bounds};
use crate::edit;
use crate::project::is_code;
use egui::{Event, Key, Modifiers, RichText, Ui};
use std::path::PathBuf;
use std::sync::mpsc::channel;

impl App {
    /// Handles keys the plain text editor does not know, before it sees them:
    /// Enter keeps the indent, brackets and quotes close themselves, Backspace removes an empty pair.
    pub(super) fn editor_keys(&mut self, ui: &mut Ui) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let id = egui::Id::unique(("editor", &t.path));
        if !ui.memory(|m| m.has_focus(id)) || self.completion.is_some() {
            return;
        }
        if self.template_key(ui) {
            return;
        }
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let sel = if t.selection.start != t.selection.end {
            t.selection.clone()
        } else {
            t.cursor..t.cursor
        };
        let mut result = None;
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            result = Some(edit::newline(&t.text, sel.clone()));
        } else {
            // only a lone typed character: with several in one frame their order must stay the editor's
            let typed = ui.input(|i| {
                let mut texts = i.events.iter().filter_map(|e| {
                    if let Event::Text(s) = e {
                        Some(s.as_str())
                    } else {
                        None
                    }
                });
                match (texts.next(), texts.next()) {
                    (Some(s), None) if s.chars().count() == 1 => s.chars().next(),
                    _ => None,
                }
            });
            if let Some(ch) = typed
                && let Some(r) = edit::type_char(&t.text, sel.clone(), ch)
            {
                ui.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
                result = Some(r);
            } else if sel.start == sel.end
                && ui.input(|i| i.key_pressed(Key::Backspace) && i.modifiers.is_none())
                && let Some(r) = edit::backspace_pair(&t.text, sel.start)
            {
                ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace));
                result = Some(r);
            }
        }
        if let Some((text, cursor)) = result {
            t.text = text;
            t.caret = Some(cursor);
            t.cursor = cursor;
            t.selection = cursor..cursor;
            t.mark_edited();
        }
    }

    /// Collects finished usage counts for ⌘-hover.
    pub(super) fn poll_usages(&mut self) {
        let index_gen = self.index.generation;
        let mut done = Vec::new();
        self.usages_rx.retain(|rx| match rx.try_recv() {
            Ok(v) => {
                done.push(v);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for (word, g, found) in done {
            self.usages.insert(word, (g, Some(found)));
        }
        // keep the cache small: only answers for the current index
        self.usages.retain(|_, (g, _)| *g == index_gen);
        if self.usages.len() > 64 {
            self.usages.clear();
        }
    }

    /// Usages of `word` for the hover card; starts counting in the background the first time.
    pub(super) fn usages_of(&mut self, word: &str, at: usize) -> Option<&Vec<FoundLine>> {
        let index_gen = self.index.generation;
        let target = self.hover_target(at);
        // one count per declaration: two `insert`s in different classes are counted apart
        let key = match &target {
            Some(t) => format!("{word}\u{0}{}:{}", t.path.display(), t.offset),
            None => word.to_string(),
        };
        if !self.usages.contains_key(&key) {
            self.usages.insert(key.clone(), (index_gen, None));
            let files: Vec<PathBuf> = self
                .project
                .as_ref()
                .map(|p| p.files.iter().filter(|f| is_code(f)).cloned().collect())
                .unwrap_or_default();
            let (tx, rx) = channel();
            let (w, k, c) = (word.to_string(), key.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                let mut found = Vec::new();
                for f in files {
                    let Ok(text) = std::fs::read_to_string(&f) else {
                        continue;
                    };
                    let hits = crate::refactor::occurrences(&text, &w);
                    let syms =
                        (target.is_some() && !hits.is_empty()).then(|| crate::kotlin::scan(&text));
                    for r in hits {
                        if let (Some(t), Some(s)) = (&target, &syms)
                            && !t.is_use(&f, &text, s, r.start)
                        {
                            continue;
                        }
                        found.push(found_line(&f, &text, r.start));
                    }
                }
                let _ = tx.send((k, index_gen, found));
                if let Some(c) = c {
                    c.request_repaint();
                }
            });
            self.usages_rx.push(rx);
        }
        self.usages.get(&key).and_then(|(_, f)| f.as_ref())
    }

    /// `usage_target` for the hovered name, worked out once per place and text version (it reads
    /// files and walks class hierarchies: too slow for every frame).
    fn hover_target(&mut self, at: usize) -> Option<crate::usage::Target> {
        let t = self.tabs.get(self.active)?;
        let key = (t.path.clone(), at, t.rev, self.index.generation);
        if let Some((k, target)) = &self.hover_target
            && *k == key
        {
            return target.clone();
        }
        let target = self.usage_target(at);
        self.hover_target = Some((key, target.clone()));
        target
    }

    /// The card shown while ⌘ (or Ctrl) is held over a name: where it is declared and its usages.
    pub(super) fn hover_card(&mut self, ui: &mut Ui, word: &str, at: usize) {
        let path = self
            .tabs
            .get(self.active)
            .map(|t| t.path.clone())
            .unwrap_or_default();
        let mut defs = self.index.definitions(word, &path);
        // the one declaration this place means, when that is known
        if let Some(t) = self.hover_target(at) {
            defs.retain(|d| d.path == t.path && d.offset == t.offset);
        }
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        for d in defs.iter().take(3) {
            ui.label(RichText::new(&d.detail).monospace().strong());
            ui.label(
                RichText::new(
                    d.path
                        .strip_prefix(&root)
                        .unwrap_or(&d.path)
                        .display()
                        .to_string(),
                )
                .color(crate::theme::t().weak)
                .small(),
            );
        }
        if defs.is_empty() {
            ui.label(
                RichText::new(format!("`{word}` is not declared in this project"))
                    .color(crate::theme::t().weak),
            );
        }
        ui.separator();
        let decls: Vec<(PathBuf, usize)> =
            defs.iter().map(|d| (d.path.clone(), d.offset)).collect();
        match self.usages_of(word, at) {
            None => {
                ui.label(RichText::new("Counting usages…").color(crate::theme::t().weak));
            }
            Some(found) => {
                // the declaration is not a usage of itself
                let found: Vec<&FoundLine> = found
                    .iter()
                    .filter(|f| !decls.iter().any(|(p, o)| *p == f.path && *o == f.offset))
                    .collect();
                let mut files: Vec<&std::path::Path> =
                    found.iter().map(|f| f.path.as_path()).collect();
                files.dedup();
                ui.label(
                    RichText::new(format!("{} usages in {} files", found.len(), files.len()))
                        .strong(),
                );
                for f in found.iter().take(8) {
                    let name = f
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(
                        RichText::new(format!("{name}:{}  {}", f.line, f.text))
                            .monospace()
                            .small(),
                    );
                }
                if found.len() > 8 {
                    ui.label(
                        RichText::new(format!("… {} more", found.len() - 8))
                            .color(crate::theme::t().weak)
                            .small(),
                    );
                }
            }
        }
        ui.label(
            RichText::new(
                "⌘-click: declaration (on the declaration: all usages)    right-click: more",
            )
            .color(crate::theme::t().weak)
            .small(),
        );
    }
}

/// The identifier under a byte offset, if any.
pub(super) fn word_at(text: &str, at: usize) -> Option<(usize, usize)> {
    let (a, b) = word_bounds(text, at);
    (a < b && !text[a..b].starts_with(|c: char| c.is_ascii_digit())).then_some((a, b))
}
