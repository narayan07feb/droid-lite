//! The translations table: every string resource × every language folder, missing ones marked.
//! A click on a cell edits it in place; Enter writes it to that folder's `strings.xml`.

use super::App;
use crate::android_kit;
use crate::highlight::ERROR;
use crate::theme;
use egui::{Key, RichText, Ui};
use std::path::PathBuf;

/// One cell: the value as plain text, and where it is.
struct Cell {
    value: String,
    path: PathBuf,
    offset: usize,
}

struct Row {
    key: String,
    /// `translatable="false"`: no translation needed.
    fixed: bool,
    /// One per column of `Table::folders`.
    cells: Vec<Option<Cell>>,
}

struct Table {
    /// `values` first, then `values-hi`, `values-fr`…
    folders: Vec<String>,
    rows: Vec<Row>,
}

impl Table {
    fn build(res: &crate::resources::Resources) -> Table {
        let strings = res.strings();
        let mut folders: Vec<String> = strings
            .iter()
            .flat_map(|(_, v)| v.iter().map(|e| e.qualifier.clone()))
            .collect();
        folders.sort();
        folders.dedup();
        let rows = strings
            .into_iter()
            .map(|(key, entries)| Row {
                key: key.to_string(),
                fixed: entries.iter().any(|e| e.fixed),
                cells: folders
                    .iter()
                    .map(|f| {
                        entries.iter().find(|e| e.qualifier == *f).map(|e| Cell {
                            value: android_kit::plain_value(e.value.as_deref().unwrap_or("")),
                            path: e.path.clone(),
                            offset: e.offset,
                        })
                    })
                    .collect(),
            })
            .collect();
        Table { folders, rows }
    }

    fn missing(&self, r: &Row) -> usize {
        if r.fixed {
            0
        } else {
            r.cells.iter().filter(|c| c.is_none()).count()
        }
    }
}

#[derive(Default)]
pub(super) struct Translations {
    pub(super) open: bool,
    filter: String,
    missing_only: bool,
    /// The cell being edited: row key, column, text, focused once.
    edit: Option<(String, usize, String, bool)>,
    /// The table and the resource scan it was built from.
    table: Option<(u64, Table)>,
}

/// `1 string`, `3 strings`.
pub(super) fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// A table cell of a fixed size, its content left-aligned.
fn sized(ui: &mut Ui, size: [f32; 2], w: impl egui::Widget) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(size[0], size[1]),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(size[0], size[1]));
            ui.add(w)
        },
    )
    .inner
}

impl App {
    pub(super) fn translations_window(&mut self, ctx: &egui::Context) {
        if !self.translations.open {
            return;
        }
        let stale = self
            .translations
            .table
            .as_ref()
            .is_none_or(|(g, _)| *g != self.res_gen);
        if stale {
            self.translations.table = Some((self.res_gen, Table::build(&self.res)));
        }
        let mut open = true;
        let mut commit = None;
        egui::Window::new("Translations")
            .open(&mut open)
            .default_size([960.0, 540.0])
            .show(ctx, |ui| commit = self.translations_ui(ui));
        self.translations.open = open;
        if let Some((row, col, text)) = commit {
            self.write_translation(row, col, &text);
        }
    }

    /// Draws the table; returns an edit to write (row key, column, text).
    fn translations_ui(&mut self, ui: &mut Ui) -> Option<(String, usize, String)> {
        let tr = &mut self.translations;
        let (_, table) = tr.table.as_ref()?;
        let missing: usize = table.rows.iter().map(|r| table.missing(r)).sum();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut tr.filter)
                    .hint_text("Filter by name or text")
                    .desired_width(260.0),
            );
            ui.checkbox(&mut tr.missing_only, "Only missing");
            ui.label(
                RichText::new(format!(
                    "{} · {} · {missing} missing",
                    count(table.rows.len(), "string"),
                    count(table.folders.len().saturating_sub(1), "language"),
                ))
                .color(theme::t().weak),
            );
        });
        ui.separator();
        let q = tr.filter.to_lowercase();
        let rows: Vec<&Row> = table
            .rows
            .iter()
            .filter(|r| !tr.missing_only || table.missing(r) > 0)
            .filter(|r| {
                q.is_empty()
                    || r.key.to_lowercase().contains(&q)
                    || r.cells
                        .iter()
                        .flatten()
                        .any(|c| c.value.to_lowercase().contains(&q))
            })
            .collect();
        let key_w = 220.0;
        let col_w = ((ui.available_width() - key_w) / table.folders.len().max(1) as f32).max(140.0);
        let header = |ui: &mut Ui| {
            ui.horizontal(|ui| {
                sized(
                    ui,
                    [key_w, 18.0],
                    egui::Label::new(RichText::new("Name").strong()),
                );
                for f in &table.folders {
                    let name = match f.strip_prefix("values-") {
                        Some(l) => l.to_string(),
                        None => "default".into(),
                    };
                    sized(
                        ui,
                        [col_w, 18.0],
                        egui::Label::new(RichText::new(name).strong()),
                    );
                }
            });
        };
        let mut commit = None;
        let mut start = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                header(ui);
                let row_h = 22.0;
                egui::ScrollArea::vertical()
                    .id_salt("translation-rows")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, rows.len(), |ui, range| {
                        for r in &rows[range] {
                            ui.horizontal(|ui| {
                                ui.set_height(row_h);
                                sized(
                                    ui,
                                    [key_w, row_h],
                                    egui::Label::new(RichText::new(&r.key).monospace().small())
                                        .truncate(),
                                );
                                for (col, cell) in r.cells.iter().enumerate() {
                                    let editing = tr
                                        .edit
                                        .as_mut()
                                        .filter(|(k, c, _, _)| *k == r.key && *c == col);
                                    if let Some((_, _, text, focused)) = editing {
                                        let resp = sized(
                                            ui,
                                            [col_w, row_h],
                                            egui::TextEdit::singleline(text),
                                        );
                                        if !*focused {
                                            resp.request_focus();
                                            *focused = true;
                                        }
                                        if ui.input(|i| i.key_pressed(Key::Escape)) {
                                            start = Some(None);
                                        } else if resp.lost_focus() {
                                            commit = Some((r.key.clone(), col, text.clone()));
                                        }
                                        continue;
                                    }
                                    let (text, color) = match cell {
                                        Some(c) => (c.value.clone(), theme::t().text),
                                        None if r.fixed => {
                                            ("not translated".into(), theme::t().weak)
                                        }
                                        None => ("missing".into(), ERROR),
                                    };
                                    let resp = sized(
                                        ui,
                                        [col_w, row_h],
                                        egui::Label::new(RichText::new(text).color(color).small())
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                    );
                                    if resp.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                                        let v = cell
                                            .as_ref()
                                            .map_or(String::new(), |c| c.value.clone());
                                        start = Some(Some((r.key.clone(), col, v, false)));
                                    }
                                }
                            });
                        }
                    });
            });
        if let Some(s) = start {
            tr.edit = s;
        }
        if commit.is_some() {
            tr.edit = None;
        }
        commit
    }

    /// Writes one cell: in place when the folder has the string, else added to that folder's
    /// `strings.xml` next to the default one.
    fn write_translation(&mut self, key: String, col: usize, text: &str) {
        let Some((_, table)) = &self.translations.table else {
            return;
        };
        let Some(row) = table.rows.iter().find(|r| r.key == key) else {
            return;
        };
        let value = android_kit::xml_value(text);
        let result = match &row.cells[col] {
            Some(c) if c.value == text => return,
            Some(c) => std::fs::read_to_string(&c.path)
                .ok()
                .and_then(|x| android_kit::set_string(&x, c.offset, &value))
                .map(|x| (c.path.clone(), x)),
            None => {
                // the res folder of any existing version of this string
                let res = row
                    .cells
                    .iter()
                    .flatten()
                    .find_map(|c| c.path.parent()?.parent().map(PathBuf::from));
                res.map(|res| {
                    let file = res.join(&table.folders[col]).join("strings.xml");
                    let old = std::fs::read_to_string(&file).ok();
                    (file, android_kit::add_string(old.as_deref(), &key, &value))
                })
            }
        };
        let Some((file, xml)) = result else {
            self.status = format!("Could not change `{key}`");
            return;
        };
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new(".")));
        match std::fs::write(&file, xml) {
            Ok(()) => {
                self.status = format!("Saved `{key}` in {}", file.display());
                self.res_stale = true;
            }
            Err(e) => self.status = format!("Could not write {}: {e}", file.display()),
        }
    }
}
