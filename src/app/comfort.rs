//! Daily comfort: settings, session restore, local history, auto-save.

use super::{App, config_dir, hash};
use crate::theme;
use egui::RichText;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// User settings, stored as `settings.json` in the config folder.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Settings {
    pub font_size: f32,
    pub light: bool,
    pub glass: bool,
    pub autosave: bool,
    pub lsp_heap: String,
    /// Find usages filters (see `usage_filter`).
    pub hide_tests: bool,
    pub hide_imports: bool,
    pub hide_xml: bool,
    pub usage_path: String,
    /// The base PR or branch picked for a branch, keyed by `<project root>\t<branch>`.
    pub pr_bases: std::collections::BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_size: 13.0,
            light: false,
            glass: true,
            autosave: true,
            lsp_heap: "2g".into(),
            hide_tests: false,
            hide_imports: false,
            hide_xml: false,
            usage_path: String::new(),
            pr_bases: Default::default(),
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let d = Settings::default();
        let Some(v) = config_dir()
            .and_then(|c| std::fs::read_to_string(c.join("settings.json")).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return d;
        };
        Settings {
            font_size: v["font_size"].as_f64().map_or(d.font_size, |f| f as f32),
            light: v["light"].as_bool().unwrap_or(d.light),
            glass: v["glass"].as_bool().unwrap_or(d.glass),
            autosave: v["autosave"].as_bool().unwrap_or(d.autosave),
            lsp_heap: v["lsp_heap"].as_str().map_or(d.lsp_heap, String::from),
            hide_tests: v["hide_tests"].as_bool().unwrap_or(d.hide_tests),
            hide_imports: v["hide_imports"].as_bool().unwrap_or(d.hide_imports),
            hide_xml: v["hide_xml"].as_bool().unwrap_or(d.hide_xml),
            usage_path: v["usage_path"].as_str().map_or(d.usage_path, String::from),
            pr_bases: v["pr_bases"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn save(&self) {
        if let Some(c) = config_dir() {
            let _ = std::fs::create_dir_all(&c);
            let v = json!({ "font_size": self.font_size, "light": self.light, "glass": self.glass, "autosave": self.autosave, "lsp_heap": self.lsp_heap,
                "hide_tests": self.hide_tests, "hide_imports": self.hide_imports, "hide_xml": self.hide_xml, "usage_path": self.usage_path, "pr_bases": self.pr_bases });
            let _ = std::fs::write(
                c.join("settings.json"),
                serde_json::to_string_pretty(&v).unwrap_or_default(),
            );
        }
    }

    /// Applies what can change while running: theme and font size.
    pub fn apply(&self, ctx: &egui::Context, glass: bool) {
        theme::set_light(self.light);
        theme::set_font_size(self.font_size);
        ctx.set_visuals(theme::visuals(glass && !self.light));
    }
}

fn session_file(root: &Path) -> Option<PathBuf> {
    Some(
        config_dir()?
            .join("sessions")
            .join(format!("{:016x}.json", hash(&root.to_string_lossy()))),
    )
}

/// One history version on disk: when it was saved, and its file.
pub(super) struct Version {
    pub time: u64,
    pub file: PathBuf,
}

const HISTORY_KEEP: usize = 40;
const HISTORY_DAYS: u64 = 14;

fn history_dir(path: &Path) -> Option<PathBuf> {
    Some(
        config_dir()?
            .join("history")
            .join(format!("{:016x}", hash(&path.to_string_lossy()))),
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Keeps the current disk content of `path` before it is overwritten (save, refactor, auto-save).
/// Identical content is not stored twice; old and extra versions are dropped.
pub(super) fn record_history(path: &Path) {
    let (Some(dir), Ok(text)) = (history_dir(path), std::fs::read(path)) else {
        return;
    };
    let mut versions = history(path);
    if versions
        .first()
        .is_some_and(|v| std::fs::read(&v.file).is_ok_and(|t| t == text))
    {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("path.txt"), path.to_string_lossy().as_bytes());
    let _ = std::fs::write(dir.join(now_ms().to_string()), &text);
    versions = history(path);
    let cutoff = now_ms().saturating_sub(HISTORY_DAYS * 86_400_000);
    for (i, v) in versions.iter().enumerate() {
        if i >= HISTORY_KEEP || v.time < cutoff {
            let _ = std::fs::remove_file(&v.file);
        }
    }
}

/// Stored versions of `path`, newest first.
pub(super) fn history(path: &Path) -> Vec<Version> {
    let Some(dir) = history_dir(path) else {
        return Vec::new();
    };
    let mut v: Vec<Version> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            Some(Version {
                time: e.file_name().to_str()?.parse().ok()?,
                file: e.path(),
            })
        })
        .collect();
    v.sort_by_key(|v| std::cmp::Reverse(v.time));
    v
}

/// A unified diff of two texts, through `git diff --no-index` (git is there for blame anyway).
pub(super) fn diff_texts(old: &Path, new_text: &str) -> String {
    let tmp = std::env::temp_dir().join(format!("droid-lite-current-{}", std::process::id()));
    if std::fs::write(&tmp, new_text).is_err() {
        return String::new();
    }
    let out = std::process::Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--"])
        .arg(old)
        .arg(&tmp)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    out
}

impl App {
    /// Writes open tabs and cursors for this project when they changed (cheap, at most once a second).
    pub(super) fn save_session(&mut self, force: bool) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .map(|t| json!({ "path": t.path.to_string_lossy(), "cursor": t.cursor }))
            .collect();
        let v = json!({ "tabs": tabs, "active": self.active, "tree_view": self.tree_view.name() });
        let text = v.to_string();
        let h = hash(&text);
        if h == self.session_saved.0 || (!force && self.session_saved.1.elapsed().as_secs() < 1) {
            return;
        }
        if let Some(f) = session_file(&root) {
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(f, text);
        }
        self.session_saved = (h, Instant::now());
    }

    /// Reopens the tabs of the last session of this project, where they were.
    pub(super) fn restore_session(&mut self) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        let Some(v) = session_file(&root)
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return;
        };
        let view = match v["tree_view"].as_str() {
            Some("Project") => super::tree::View::Project,
            _ => super::tree::View::Android,
        };
        if view != self.tree_view {
            self.tree_view = view;
            if let Some(p) = &self.project {
                self.tree = super::tree::build(p, view);
            }
        }
        for t in v["tabs"].as_array().into_iter().flatten() {
            let (Some(p), cursor) = (
                t["path"].as_str(),
                t["cursor"].as_u64().unwrap_or(0) as usize,
            ) else {
                continue;
            };
            let p = PathBuf::from(p);
            if p.is_file() && !self.tabs.iter().any(|t| t.path == p) {
                self.show_file(&p, Some(cursor));
            }
        }
        if let Some(a) = v["active"]
            .as_u64()
            .map(|a| a as usize)
            .filter(|a| *a < self.tabs.len())
        {
            self.active = a;
        }
        self.history.clear();
    }

    /// Saves every edited tab, as IntelliJ does when the window loses focus.
    pub(super) fn autosave(&mut self) {
        let active = self.active;
        for i in 0..self.tabs.len() {
            if self.tabs[i].dirty() && !self.tabs[i].deleted {
                self.active = i;
                self.save();
            }
        }
        self.active = active;
    }

    /// Detects the window losing focus and auto-saves then.
    pub(super) fn watch_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);
        if self.window_focused && !focused && self.settings.autosave {
            self.autosave();
        }
        self.window_focused = focused;
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let before = self.settings.clone();
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
                ui.label("Editor font size");
                ui.add(egui::Slider::new(&mut self.settings.font_size, 10.0..=24.0).step_by(1.0));
                ui.end_row();
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.settings.light, false, "Dark");
                    ui.radio_value(&mut self.settings.light, true, "Light");
                });
                ui.end_row();
                ui.label("Liquid Glass");
                ui.checkbox(&mut self.settings.glass, "On (dark theme, applies after restart)");
                ui.end_row();
                ui.label("Auto-save");
                ui.checkbox(&mut self.settings.autosave, "Save edited files when Droid Lite loses focus");
                ui.end_row();
                ui.label("kotlin-lsp memory");
                ui.add(egui::TextEdit::singleline(&mut self.settings.lsp_heap).desired_width(60.0));
                ui.end_row();
                ui.label("Keymap");
                ui.label(RichText::new(self.keymap.name()).color(theme::t().weak));
                ui.end_row();
            });
            ui.label(RichText::new("Help → Keymap switches keymaps. kotlin-lsp memory applies the next time it starts.").color(theme::t().weak).small());
        });
        if self.settings != before {
            self.settings.save();
            self.settings.apply(ctx, self.glass);
            for t in self.tabs.iter_mut() {
                t.recolor = true;
            }
        }
        self.show_settings = open;
    }

    pub(super) fn history_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.history_for.clone() else {
            return;
        };
        let mut open = !ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let versions = history(&path);
        let mut pick: Option<(PathBuf, bool)> = None;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        egui::Window::new(format!("Local history: {name}")).open(&mut open).default_size([520.0, 360.0]).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            if versions.is_empty() {
                ui.label(RichText::new("No saved versions yet. A copy is kept before every save, refactoring and auto-save.").color(theme::t().weak));
            }
            let now = now_ms() / 1000;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for v in &versions {
                    ui.horizontal(|ui| {
                        let size = std::fs::metadata(&v.file).map_or(0, |m| m.len());
                        ui.label(format!("{}   ({} bytes)", crate::git::ago((v.time / 1000) as i64, now as i64), size));
                        if ui.button("Show diff").clicked() {
                            pick = Some((v.file.clone(), false));
                        }
                        if ui.button("Restore").on_hover_text("Puts this version in the editor; save to keep it").clicked() {
                            pick = Some((v.file.clone(), true));
                        }
                    });
                }
            });
        });
        if let Some((file, restore)) = pick {
            let current = self
                .tabs
                .iter()
                .find(|t| t.path == path)
                .map(|t| t.text.clone())
                .or_else(|| std::fs::read_to_string(&path).ok())
                .unwrap_or_default();
            if restore {
                if let Ok(old) = std::fs::read_to_string(&file) {
                    self.open_file(&path, None);
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) {
                        t.text = old;
                        t.cursor = t.cursor.min(t.text.len());
                        t.caret = Some(t.cursor);
                        t.mark_edited();
                    }
                    self.status = format!(
                        "Restored an older version of {name}; ⌘S keeps it, Revert drops it"
                    );
                    open = false;
                }
            } else {
                self.show_diff_text(
                    format!("Local history: {name} (old → current)"),
                    diff_texts(&file, &current),
                );
            }
        }
        if !open {
            self.history_for = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_distinct_versions_newest_first() {
        let _guard = super::super::e2e::config_guard();
        let file = std::env::temp_dir().join(format!("droid-lite-hist-{}.kt", std::process::id()));
        std::fs::write(&file, "v1").unwrap();
        record_history(&file);
        record_history(&file); // same content: not stored again
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, "v2").unwrap();
        record_history(&file);
        let v = history(&file);
        assert_eq!(v.len(), 2);
        assert_eq!(std::fs::read_to_string(&v[0].file).unwrap(), "v2");
        assert!(diff_texts(&v[1].file, "v3\n").contains("+v3"));
        std::fs::remove_file(&file).ok();
    }
}
