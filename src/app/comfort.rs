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
            let v = json!({ "font_size": self.font_size, "light": self.light, "autosave": self.autosave, "lsp_heap": self.lsp_heap,
                "hide_tests": self.hide_tests, "hide_imports": self.hide_imports, "hide_xml": self.hide_xml, "usage_path": self.usage_path, "pr_bases": self.pr_bases });
            let _ = std::fs::write(
                c.join("settings.json"),
                serde_json::to_string_pretty(&v).unwrap_or_default(),
            );
        }
    }

    /// Applies what can change while running: theme and font size.
    pub fn apply(&self, ctx: &egui::Context) {
        theme::set_light(self.light);
        theme::set_font_size(self.font_size);
        ctx.set_visuals(theme::visuals());
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
/// All of local history (every file, also files deleted since) stays under this size.
const HISTORY_CAP: u64 = 200 * 1024 * 1024;

fn history_root() -> Option<PathBuf> {
    Some(config_dir()?.join("history"))
}

fn history_dir(path: &Path) -> Option<PathBuf> {
    Some(history_root()?.join(format!("{:016x}", hash(&path.to_string_lossy()))))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Keeps the current disk content of `path` before it is overwritten (save, refactor, auto-save).
/// A file that is not there has nothing to keep. The error says why no copy could be kept.
pub(super) fn record_history(path: &Path) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(bytes) => record_text(path, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Keeps `text` as a version of `path` in local history ([`record_history`] for text that is not
/// on disk: an editor buffer that is not saved). Identical content is not stored twice; old and
/// extra versions are dropped. The error says why the copy could not be kept.
pub(super) fn record_text(path: &Path, text: &[u8]) -> Result<(), String> {
    let dir = history_dir(path).ok_or("no settings folder (HOME is not set)")?;
    if history(path)
        .first()
        .is_some_and(|v| std::fs::read(&v.file).is_ok_and(|t| t == *text))
    {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("path.txt"), path.to_string_lossy().as_bytes())
        .map_err(|e| e.to_string())?;
    write_version(&dir, text).map_err(|e| e.to_string())?;
    let cutoff = now_ms().saturating_sub(HISTORY_DAYS * 86_400_000);
    for (i, v) in history(path).iter().enumerate() {
        // a version from a clock set ahead is newer than the cutoff: never dropped for its age
        if i >= HISTORY_KEEP || v.time < cutoff {
            let _ = std::fs::remove_file(&v.file);
        }
    }
    prune_soon();
    Ok(())
}

/// Writes a new version file `<ms>-<pid>-<n>`: two versions in one millisecond never share a name.
fn write_version(dir: &Path, text: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("{}-{}-{n}", now_ms(), std::process::id());
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))
        {
            Ok(mut f) => return f.write_all(text),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// [`prune_all`] at most once a minute (it reads every history folder).
fn prune_soon() {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 || now.abs_diff(last) > 60_000 {
        LAST.store(now, std::sync::atomic::Ordering::Relaxed);
        if let Some(root) = history_root() {
            prune_all(&root, HISTORY_CAP);
        }
    }
}

/// Drops the oldest versions of all files under `root` until they take `cap` bytes at most. A
/// version from a clock set ahead counts as the newest, so it goes last.
pub(super) fn prune_all(root: &Path, cap: u64) {
    let mut all: Vec<(u64, u64, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten())
        .filter_map(Result::ok)
        .filter_map(|e| {
            let time = version_time(e.file_name().to_str()?)?;
            Some((time, e.metadata().ok()?.len(), e.path()))
        })
        .collect();
    let mut total: u64 = all.iter().map(|v| v.1).sum();
    all.sort_by_key(|v| v.0);
    for (_, size, file) in all {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&file).is_ok() {
            total -= size;
        }
    }
}

/// The time of a version file name: `<ms>` or `<ms>-<pid>-<n>`.
fn version_time(name: &str) -> Option<u64> {
    name.split('-').next()?.parse().ok()
}

/// Stored versions of `path`, newest first.
pub(super) fn history(path: &Path) -> Vec<Version> {
    let Some(dir) = history_dir(path) else {
        return Vec::new();
    };
    let mut v: Vec<(Version, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let n = name.rsplit('-').next()?.parse().unwrap_or(0);
            Some((
                Version {
                    time: version_time(name)?,
                    file: e.path(),
                },
                n,
            ))
        })
        .collect();
    v.sort_by_key(|(v, n)| std::cmp::Reverse((v.time, *n)));
    v.into_iter().map(|(v, _)| v).collect()
}

/// "Could not keep a copy of <file>: <error>".
pub(super) fn copy_failed(path: &Path, e: &str) -> String {
    format!("Could not keep a copy of {}: {e}", path.display())
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
        let v = json!({
            "tabs": tabs,
            "active": self.active,
            "tree_view": self.tree_view.name(),
            "variant": self.variant,
            "structure": self.show_structure,
        });
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
        if let Some(var) = v["variant"]
            .as_str()
            .filter(|x| self.variants.iter().any(|y| y == x))
        {
            self.variant = var.to_string();
        }
        self.show_structure = v["structure"].as_bool().unwrap_or(false);
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

    /// Saves every edited tab, as IntelliJ does when the window loses focus (and on quit). A tab
    /// whose file changed on disk meanwhile is not written: its bar asks which version to keep.
    /// While a checkout runs, [`App::save`] refuses, so nothing is written.
    pub(super) fn autosave(&mut self) {
        let active = self.active;
        for i in 0..self.tabs.len() {
            let t = &self.tabs[i];
            if t.dirty() && !t.deleted && !t.disk_changed {
                self.active = i;
                self.save();
            }
        }
        self.active = active;
    }

    /// Tabs with edits that auto-save does not write: the file changed or vanished on disk, or
    /// auto-save is off. Quitting would lose them.
    pub(super) fn unwritten_edits(&self) -> Vec<usize> {
        (0..self.tabs.len())
            .filter(|&i| {
                let t = &self.tabs[i];
                t.dirty() && !t.readonly && (t.deleted || t.disk_changed || !self.settings.autosave)
            })
            .collect()
    }

    /// Tabs with edits that are not saved.
    pub(super) fn unsaved_tabs(&self) -> Vec<usize> {
        (0..self.tabs.len())
            .filter(|&i| self.tabs[i].dirty() && !self.tabs[i].readonly)
            .collect()
    }

    /// Puts the text of tabs `tabs` in local history, so nothing typed is lost. The tabs whose
    /// copy failed, each with "Could not keep a copy of <file>: <error>".
    pub(super) fn keep_copies(&self, tabs: &[usize]) -> Vec<(usize, String)> {
        tabs.iter()
            .filter_map(|&i| {
                let t = &self.tabs[i];
                let e = record_text(&t.path, t.text.as_bytes()).err()?;
                Some((i, copy_failed(&t.path, &e)))
            })
            .collect()
    }

    fn save_tabs(&mut self, tabs: &[usize]) {
        let active = self.active;
        for &i in tabs {
            self.active = i;
            self.save();
        }
        self.active = active;
    }

    /// The "Unsaved edits" window listing tabs `tabs`, with `buttons` (keep mine, keep a copy,
    /// cancel); the one clicked.
    fn unsaved_dialog(
        &self,
        ctx: &egui::Context,
        tabs: &[usize],
        buttons: [&str; 3],
    ) -> Option<usize> {
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        let list = crate::git::file_list(
            "These files have edits that are not saved:",
            &root,
            tabs.iter().map(|&i| self.tabs[i].path.as_path()),
        );
        let mut pick = None;
        egui::Window::new("Unsaved edits")
            .id(egui::Id::unique(buttons[1]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(list);
                if let Some(e) = &self.copy_error {
                    ui.label(RichText::new(e).color(crate::highlight::ERROR));
                }
                ui.label(
                    RichText::new(
                        "Their files changed or were deleted on disk (or auto-save is off).",
                    )
                    .color(theme::t().weak)
                    .small(),
                );
                ui.horizontal(|ui| {
                    for (i, b) in buttons.into_iter().enumerate() {
                        if ui.button(b).clicked() {
                            pick = Some(i);
                        }
                    }
                });
            });
        pick
    }

    /// The window is asked to close while tabs have edits auto-save will not write: the close
    /// waits for a choice, "Keep my version (save)", "Keep a copy and close" (in local history)
    /// or "Cancel".
    pub(super) fn guard_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.close_ok
            && !self.unwritten_edits().is_empty()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_asked = true;
        }
        if !self.close_asked {
            return;
        }
        let tabs = self.unwritten_edits();
        if tabs.is_empty() {
            self.close_asked = false;
            return;
        }
        let buttons = ["Keep my version (save)", "Keep a copy and close", "Cancel"];
        match self.unsaved_dialog(ctx, &tabs, buttons) {
            Some(0) => {
                self.save_tabs(&tabs);
                // a save that failed (its folder is gone) keeps the window open, with the reason
                if self.unwritten_edits().is_empty() {
                    self.close_now(ctx);
                }
            }
            Some(1) => {
                // a copy that cannot be kept keeps the window open, with the reason
                let failed = self.keep_copies(&tabs);
                if failed.is_empty() {
                    self.close_now(ctx);
                } else {
                    self.copy_failed(failed);
                }
            }
            Some(_) => {
                self.close_asked = false;
                self.copy_error = None;
            }
            None => {}
        }
    }

    /// Shows why copies could not be kept, in the status bar and the Unsaved edits window.
    pub(super) fn copy_failed(&mut self, failed: Vec<(usize, String)>) {
        let text = failed
            .into_iter()
            .map(|(_, e)| e)
            .collect::<Vec<_>>()
            .join("\n");
        self.status = text.lines().next().unwrap_or_default().to_string();
        self.copy_error = Some(text);
    }

    /// A project switch ([`App::open_project`]) waits while tabs have unsaved edits: "Save",
    /// "Keep a copy and switch" (in local history) or "Cancel" (the current project stays). Once
    /// nothing is unsaved (saved or reverted meanwhile) it goes on.
    pub(super) fn guard_switch(&mut self, ctx: &egui::Context) {
        let Some(dir) = self.switch_to.clone() else {
            return;
        };
        let tabs = self.unsaved_tabs();
        let pick = if tabs.is_empty() {
            Some(1)
        } else {
            self.unsaved_dialog(ctx, &tabs, ["Save", "Keep a copy and switch", "Cancel"])
        };
        match pick {
            Some(0) => {
                self.save_tabs(&tabs);
                // a save that failed keeps the project open, with the reason
                if self.unsaved_tabs().is_empty() {
                    self.switch_project(&dir, ctx);
                }
            }
            Some(1) => self.switch_project(&dir, ctx), // it keeps the copies, or stays
            Some(_) => {
                self.switch_to = None;
                self.copy_error = None;
            }
            None => {}
        }
    }

    fn close_now(&mut self, ctx: &egui::Context) {
        self.close_asked = false;
        self.copy_error = None;
        self.close_ok = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
            self.settings.apply(ctx);
            for t in self.tabs.iter_mut() {
                t.recolor = true;
            }
        }
        self.show_settings = open;
    }

    /// Puts `old` in the editor for `path` (open or opened now); edits it replaces go to local
    /// history first, else nothing changes. Whether it was restored.
    pub(super) fn restore_version(&mut self, path: &Path, old: String) -> bool {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.open_file(path, None);
        let Some(t) = self.tabs.iter_mut().find(|t| t.path == path) else {
            return false;
        };
        if t.dirty()
            && let Err(e) = record_text(&t.path, t.text.as_bytes())
        {
            self.status = copy_failed(&t.path, &e); // the edits it would replace
            return false;
        }
        t.text = old;
        t.cursor = t.cursor.min(t.text.len());
        t.caret = Some(t.cursor);
        t.mark_edited();
        self.status = format!("Restored an older version of {name}; ⌘S keeps it, Revert drops it");
        true
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
            if restore {
                if self.refuse_during_checkout() {
                    return;
                }
                match std::fs::read(&file).map(String::from_utf8) {
                    Ok(Ok(old)) => {
                        if self.restore_version(&path, old) {
                            open = false;
                        }
                    }
                    Ok(Err(_)) => {
                        self.status = format!(
                            "This version of {name} is not UTF-8 text, so the editor cannot show it. It is kept in {}",
                            file.display()
                        );
                    }
                    Err(e) => self.status = format!("Cannot read that version of {name}: {e}"),
                }
                if !open {
                    self.history_for = None;
                }
                return;
            }
            let current = self
                .tabs
                .iter()
                .find(|t| t.path == path)
                .map(|t| t.text.clone())
                .or_else(|| std::fs::read_to_string(&path).ok())
                .unwrap_or_default();
            self.show_diff_text(
                format!("Local history: {name} (old → current)"),
                diff_texts(&file, &current),
            );
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
        record_history(&file).unwrap();
        record_history(&file).unwrap(); // same content: not stored again
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, "v2").unwrap();
        record_history(&file).unwrap();
        let v = history(&file);
        assert_eq!(v.len(), 2);
        assert_eq!(std::fs::read_to_string(&v[0].file).unwrap(), "v2");
        assert!(diff_texts(&v[1].file, "v3\n").contains("+v3"));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn versions_kept_in_one_millisecond_never_share_a_name() {
        let _guard = super::super::e2e::config_guard();
        let file = Path::new("/p/Fast.kt");
        for i in 0..20 {
            record_text(file, format!("v{i}").as_bytes()).unwrap();
        }
        let v = history(file);
        assert_eq!(v.len(), 20);
        assert_eq!(
            std::fs::read_to_string(&v[0].file).unwrap(),
            "v19",
            "newest first"
        );
    }

    #[test]
    fn a_copy_that_cannot_be_kept_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = super::super::e2e::config_guard();
        let root = history_root().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let e = record_text(Path::new("/p/A.kt"), b"typed");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(e.is_err_and(|e| e.contains("denied")));
        // no settings folder at all
        // SAFETY: the SERIAL lock is held
        unsafe { std::env::remove_var("DROID_LITE_CONFIG_DIR") };
        assert!(record_text(Path::new("/p/A.kt"), b"typed").is_err());
        assert!(
            record_history(Path::new("/nonexistent/droid-lite/A.kt")).is_ok(),
            "nothing to keep"
        );
    }

    #[test]
    fn all_history_stays_under_its_size_oldest_first_and_a_future_version_last() {
        let _guard = super::super::e2e::config_guard();
        let root = history_root().unwrap();
        let now = now_ms();
        // a deleted file's history (no file any more), another file's, a clock set ahead
        for (dir, name, size) in [
            ("a", now - 3000, 100),
            ("b", now - 2000, 100),
            ("a", now + 86_400_000, 100),
            ("b", now - 1000, 100),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(format!("{name}-1-0")), vec![b'x'; size]).unwrap();
        }
        std::fs::write(root.join("a/path.txt"), "/gone/A.kt").unwrap();
        prune_all(&root, 250);
        let left = |d: &str| {
            let mut v: Vec<u64> = std::fs::read_dir(root.join(d))
                .unwrap()
                .filter_map(|e| version_time(e.ok()?.file_name().to_str()?))
                .collect();
            v.sort();
            v
        };
        assert_eq!(left("a"), vec![now + 86_400_000]);
        assert_eq!(left("b"), vec![now - 1000]);
        assert!(root.join("a/path.txt").exists());
    }
}
