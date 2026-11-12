//! Find / replace in the current file: a bar above the editor, all matches highlighted.

use super::App;
use crate::edit;
use crate::icons as ic;
use crate::keymap::{self, Action};
use crate::theme::DARK;
use egui::{Key, Modifiers, RichText, TextEdit, Ui};
use std::ops::Range;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct FindBar {
    pub query: String,
    pub replace: String,
    pub case: bool,
    pub whole: bool,
    pub matches: Vec<Range<usize>>,
    /// Index into `matches` of the selected match.
    pub current: usize,
    key: Option<(PathBuf, u64, String, bool, bool)>,
    focus: bool,
}

impl App {
    pub(super) fn open_find(&mut self) {
        let selected = self
            .tabs
            .get(self.active)
            .map(|t| t.text[t.selection.clone()].to_string())
            .filter(|s| !s.is_empty() && !s.contains('\n'));
        let bar = self.find.get_or_insert_with(FindBar::default);
        if let Some(s) = selected {
            bar.query = s;
        }
        bar.focus = true;
        bar.key = None;
    }

    /// Recomputes matches when the text, the query or the options changed.
    pub(super) fn refresh_find(&mut self) {
        let (Some(bar), Some(t)) = (self.find.as_mut(), self.tabs.get(self.active)) else {
            return;
        };
        let key = (
            t.path.clone(),
            t.rev,
            bar.query.clone(),
            bar.case,
            bar.whole,
        );
        if bar.key.as_ref() != Some(&key) {
            bar.matches = edit::find_all(&t.text, &bar.query, bar.case, bar.whole);
            bar.current = bar
                .matches
                .iter()
                .position(|m| m.start >= t.cursor)
                .unwrap_or(0);
            bar.key = Some(key);
        }
    }

    /// Selects the next (or previous) match from the cursor, wrapping around.
    pub(super) fn find_step(&mut self, forward: bool) {
        self.refresh_find();
        let (Some(bar), Some(t)) = (self.find.as_mut(), self.tabs.get_mut(self.active)) else {
            return;
        };
        if bar.matches.is_empty() {
            return;
        }
        let n = bar.matches.len();
        bar.current = if forward {
            bar.matches
                .iter()
                .position(|m| m.start > t.selection.start.max(t.cursor.saturating_sub(1)))
                .unwrap_or(0)
        } else {
            bar.matches
                .iter()
                .rposition(|m| m.start < t.selection.start.min(t.cursor))
                .unwrap_or(n - 1)
        };
        let m = bar.matches[bar.current].clone();
        t.goto = Some(m.end);
        t.select = Some(m);
    }

    pub(super) fn replace_current(&mut self) {
        self.refresh_find();
        let (Some(bar), Some(t)) = (self.find.as_ref(), self.tabs.get_mut(self.active)) else {
            return;
        };
        let Some(m) = bar.matches.get(bar.current).cloned() else {
            return;
        };
        t.text.replace_range(m.clone(), &bar.replace);
        t.goto = Some(m.start + bar.replace.len());
        t.mark_edited();
        self.find_step(true);
    }

    pub(super) fn replace_all(&mut self) {
        self.refresh_find();
        let (Some(bar), Some(t)) = (self.find.as_ref(), self.tabs.get_mut(self.active)) else {
            return;
        };
        let n = bar.matches.len();
        if n == 0 {
            return;
        }
        let edits: Vec<(Range<usize>, String)> = bar
            .matches
            .iter()
            .map(|m| (m.clone(), bar.replace.clone()))
            .collect();
        t.text = crate::refactor::apply(&t.text, &edits);
        t.cursor = t.cursor.min(t.text.len());
        t.mark_edited();
        self.status = format!("Replaced {n} matches (⌘Z undoes, or Revert)");
    }

    /// The bar above the editor. Enter / ⇧Enter step through matches, Esc closes it.
    pub(super) fn find_bar(&mut self, ui: &mut Ui) {
        if self.find.is_none() {
            return;
        }
        self.refresh_find();
        let km = self.keymap;
        let (mut next, mut prev, mut close, mut one, mut all) = (false, false, false, false, false);
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        ui.horizontal(|ui| {
            let find_label =
                ui.label(RichText::new(format!("{}  Find", ic::MAGNIFYING_GLASS)).color(DARK.weak));
            let q = ui
                .add(
                    TextEdit::singleline(&mut bar.query)
                        .desired_width(220.0)
                        .hint_text("Find"),
                )
                .labelled_by(find_label.id);
            if bar.focus {
                q.request_focus();
                bar.focus = false;
            }
            if q.has_focus() || q.lost_focus() {
                let (enter, shift) = ui.input(|i| (i.key_pressed(Key::Enter), i.modifiers.shift));
                if enter {
                    if shift {
                        prev = true
                    } else {
                        next = true
                    }
                    q.request_focus();
                }
                close |= ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
            }
            // the toggles hand focus back to the field, so Enter keeps stepping through matches
            bar.focus |= ui
                .toggle_value(&mut bar.case, "Aa")
                .on_hover_text("Match case")
                .clicked();
            bar.focus |= ui
                .toggle_value(&mut bar.whole, "W")
                .on_hover_text("Whole words")
                .clicked();
            let count = if bar.query.is_empty() {
                String::new()
            } else if bar.matches.is_empty() {
                "No matches".into()
            } else {
                format!("{}/{}", bar.current + 1, bar.matches.len())
            };
            ui.label(RichText::new(count).color(
                if bar.matches.is_empty() && !bar.query.is_empty() {
                    crate::highlight::ERROR
                } else {
                    DARK.weak
                },
            ));
            prev |= ui
                .button("↑")
                .on_hover_text(format!(
                    "Previous  ⇧Enter  {}",
                    keymap::text(km, Action::FindPrev)
                ))
                .clicked();
            next |= ui
                .button("↓")
                .on_hover_text(format!(
                    "Next  Enter  {}",
                    keymap::text(km, Action::FindNext)
                ))
                .clicked();
            ui.separator();
            let with = ui.label(RichText::new("Replace with").color(DARK.weak));
            ui.add(
                TextEdit::singleline(&mut bar.replace)
                    .desired_width(180.0)
                    .hint_text("text"),
            )
            .labelled_by(with.id);
            one = ui.button("Replace").clicked();
            all = ui.button("Replace all").clicked();
            close |= ui.button(ic::X).on_hover_text("Close  Esc").clicked();
        });
        if close {
            self.find = None;
        } else if one {
            self.replace_current();
        } else if all {
            self.replace_all();
        } else if next || prev {
            self.find_step(next);
        }
    }
}
