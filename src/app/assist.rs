//! Kotlin and Android help in the editor: ⌥Enter quick fixes, live templates, New file from a
//! template, the build variant picker, ▶ on JUnit tests and a note when the app crashes.

use super::{App, Bottom, import_position, word_bounds};
use crate::android_kit::{self, Crash, Fix, NewKind, Template, TestTarget};
use crate::icons as ic;
use crate::kotlin::{FileSyms, Kind};
use crate::project::Module;
use crate::theme;
use egui::{Color32, Key, Modifiers, RichText, Ui};
use std::path::{Path, PathBuf};

/// The ⌥Enter list at the caret.
pub(super) struct FixPopup {
    fixes: Vec<Fix>,
    selected: usize,
    pos: egui::Pos2,
    /// The frame it opened in: the key press that opened it is not a click outside.
    frame: u64,
}

/// The New file window.
pub(super) struct NewFile {
    dir: PathBuf,
    name: String,
    kind: NewKind,
    error: Option<String>,
    focused: bool,
}

/// Adds the imports a file does not have yet (exact or `pkg.*`, or the file's own package);
/// moves `cursor` down by what went in above it.
fn add_imports(text: &mut String, imports: &[impl AsRef<str>], cursor: &mut usize) {
    let package = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("package "))
        .map(|p| p.trim().to_string())
        .unwrap_or_default();
    for fq in imports {
        let fq = fq.as_ref();
        let pkg = fq.rsplit_once('.').map_or("", |(p, _)| p);
        let have = text.lines().any(|l| {
            let l = l.trim();
            l == format!("import {fq}") || l == format!("import {pkg}.*")
        });
        if have || pkg == package {
            continue;
        }
        let at = import_position(text);
        let line = format!("import {fq}\n");
        text.insert_str(at, &line);
        if at <= *cursor {
            *cursor += line.len();
        }
    }
}

/// Reads a module's build file (`build.gradle.kts` or `build.gradle`).
fn build_text(m: &Module) -> String {
    ["build.gradle.kts", "build.gradle"]
        .iter()
        .find_map(|f| std::fs::read_to_string(m.dir.join(f)).ok())
        .unwrap_or_default()
}

/// An Android module (application or library), so unit tests run per variant.
fn is_android(build: &str) -> bool {
    [
        "com.android",
        "android.library",
        "android {",
        "androidLibrary",
        "androidApplication",
    ]
    .iter()
    .any(|k| build.contains(k))
}

/// Icon and tint for a Kotlin / Java file by what it declares: Activity, Fragment, ViewModel,
/// composables, tests, interface, object. Other files keep the plain file icon.
pub(super) fn kind_icon(path: &Path, syms: Option<&FileSyms>) -> (&'static str, Color32) {
    let plain = super::file_icon(path);
    let Some(syms) = syms else { return plain };
    let green = Color32::from_rgb(0x5F, 0xB8, 0x65);
    let blue = Color32::from_rgb(0x58, 0x9D, 0xF6);
    let orange = Color32::from_rgb(0xE7, 0x8A, 0x4E);
    let in_test = path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.starts_with("test") || s.ends_with("Test"))
    });
    if in_test {
        return (ic::FLASK, green);
    }
    let main = syms
        .decls
        .iter()
        .filter(|d| d.kind.is_type() && d.container.is_none())
        .find(|d| path.file_stem().is_some_and(|s| *s == *d.name))
        .or_else(|| {
            syms.decls
                .iter()
                .find(|d| d.kind.is_type() && d.container.is_none())
        });
    if let Some(d) = main {
        let is =
            |suffix: &str| d.name.ends_with(suffix) || d.supers.iter().any(|s| s.ends_with(suffix));
        let look = if is("Activity") {
            Some((ic::APP_WINDOW, green))
        } else if is("Fragment") {
            Some((ic::SQUARE_HALF, green))
        } else if is("ViewModel") {
            Some((ic::CIRCUITRY, orange))
        } else if is("Service") {
            Some((ic::GEAR, green))
        } else if is("Receiver") {
            Some((ic::BROADCAST, green))
        } else if is("Application") {
            Some((ic::ROCKET, green))
        } else if is("Dao") || is("Database") {
            Some((ic::DATABASE, orange))
        } else {
            None
        };
        if let Some(l) = look {
            return l;
        }
    }
    if syms.decls.iter().any(|d| d.is_composable()) {
        return (ic::LAYOUT, blue);
    }
    match main.map(|d| d.kind) {
        Some(Kind::Interface) => (ic::DIAMOND, plain.1),
        Some(Kind::Object) => (ic::HEXAGON, plain.1),
        _ => plain,
    }
}

impl App {
    // ---------- quick fixes ----------

    /// ⌥Enter: the fixes for the cursor's line, in a list at the caret.
    pub(super) fn quick_fix(&mut self) {
        self.settle(true);
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        if t.readonly {
            return;
        }
        let xml = t.path.extension().is_some_and(|e| e == "xml");
        let mut fixes = android_kit::quick_fixes(&t.text, &t.syms, &t.lints, t.cursor, xml, &|k| {
            !self.res.get("string", k).is_empty()
        });
        if !xml {
            fixes.extend(self.import_fixes());
        }
        if fixes.is_empty() {
            self.status = "No quick fixes here".into();
            return;
        }
        self.fix_popup = Some(FixPopup {
            fixes,
            selected: 0,
            pos: self.caret_pos.unwrap_or(egui::pos2(200.0, 200.0)),
            frame: self.frames,
        });
    }

    /// "Import com.x.Name" for a capitalised name under the cursor that the file does not know.
    fn import_fixes(&self) -> Vec<Fix> {
        let Some(t) = self.tabs.get(self.active) else {
            return Vec::new();
        };
        let (a, b) = word_bounds(&t.text, t.cursor);
        let word = &t.text[a..b];
        let known = t
            .syms
            .imports
            .iter()
            .any(|i| i.rsplit('.').next() == Some(word) || i.ends_with(&format!(" as {word}")))
            || t.syms.decls.iter().any(|d| d.name == word);
        if word.is_empty()
            || !word.starts_with(|c: char| c.is_ascii_uppercase())
            || known
            || t.text[..a].ends_with('.')
        {
            return Vec::new();
        }
        let mut fqs: Vec<(String, String)> = self
            .index
            .named(word)
            .filter(|(_, _, d)| d.container.is_none() && (d.kind.is_type() || d.kind == Kind::Fun))
            .map(|(_, f, d)| (crate::index::qualified(f, d), f.package.clone()))
            .chain(
                self.libs
                    .named(word)
                    .map(|s| (s.qualified(), s.package.to_string())),
            )
            .collect();
        // a declaration in the same package needs no import, nor does Kotlin's default set
        if fqs.iter().any(|(_, p)| *p == t.syms.package) {
            return Vec::new();
        }
        fqs.retain(|(_, p)| p != "kotlin" && !p.starts_with("kotlin.") && p != "java.lang");
        fqs.sort();
        fqs.dedup();
        fqs.into_iter()
            .take(6)
            .map(|(fq, _)| Fix {
                label: format!("Import {fq}"),
                imports: vec![fq],
                ..Fix::default()
            })
            .collect()
    }

    fn apply_fix(&mut self, f: Fix) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let mut cursor = t.cursor as isize;
        for (r, new) in &f.edits {
            if r.end <= t.cursor {
                cursor += new.len() as isize - (r.end - r.start) as isize;
            }
        }
        let mut cursor = cursor.max(0) as usize;
        let mut text = crate::refactor::apply(&t.text, &f.edits);
        let mut imports = f.imports.clone();
        let path = t.path.clone();
        if let Some((key, value)) = &f.string {
            let Some(module) = self
                .project
                .as_ref()
                .and_then(|p| p.module_of(&path))
                .cloned()
            else {
                self.status = "This file is not in a module".into();
                return;
            };
            let strings = module.dir.join("src/main/res/values/strings.xml");
            let old = std::fs::read_to_string(&strings).ok();
            let _ = std::fs::create_dir_all(strings.parent().unwrap_or(Path::new(".")));
            if let Err(e) = std::fs::write(
                &strings,
                android_kit::add_string(old.as_deref(), key, value),
            ) {
                self.status = format!("Could not write {}: {e}", strings.display());
                return;
            }
            self.res_stale = true;
            let root = self.root();
            let shown = strings.strip_prefix(&root).unwrap_or(&strings);
            self.status = format!("Added `{key}` to {}", shown.display());
            // Kotlin needs the module's R; leave a file that imports some R alone
            let ns = android_kit::namespace("", &build_text(&module));
            let has_r = text.lines().any(|l| {
                let l = l.trim();
                l.starts_with("import ") && l.ends_with(".R")
            });
            let package = &self.tabs[self.active].syms.package;
            if path.extension().is_some_and(|e| e == "kt")
                && !has_r
                && !ns.is_empty()
                && *package != ns
            {
                imports.push(format!("{ns}.R"));
            }
        }
        add_imports(&mut text, &imports, &mut cursor);
        let t = &mut self.tabs[self.active];
        t.text = text;
        t.goto = Some(cursor.min(t.text.len()));
        t.reanalyse();
        self.index.update(&path, &self.tabs[self.active].text);
        if let Some(l) = self.lsp.as_mut() {
            l.did_change(&path, &self.tabs[self.active].text);
        }
    }

    /// ↑ ↓ Enter Esc for the quick-fix list, taken before the editor sees them.
    pub(super) fn fix_popup_keys(&mut self, ctx: &egui::Context) {
        let Some(p) = self.fix_popup.as_mut() else {
            return;
        };
        let key = |k| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if key(Key::ArrowDown) {
            p.selected = (p.selected + 1).min(p.fixes.len() - 1);
        }
        if key(Key::ArrowUp) {
            p.selected = p.selected.saturating_sub(1);
        }
        if key(Key::Escape) {
            self.fix_popup = None;
        } else if key(Key::Enter) {
            let p = self.fix_popup.take().expect("checked above");
            self.apply_fix(p.fixes[p.selected].clone());
        }
    }

    pub(super) fn fix_popup_ui(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.fix_popup else { return };
        let mut chosen = None;
        let area = egui::Area::new(egui::Id::unique("quick_fixes"))
            .fixed_pos(p.pos)
            .order(egui::Order::Foreground)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(280.0);
                    ui.label(
                        RichText::new(format!("{}  Quick fixes", ic::LIGHTBULB))
                            .color(theme::t().weak)
                            .small(),
                    );
                    for (i, f) in p.fixes.iter().enumerate() {
                        if ui.selectable_label(i == p.selected, &f.label).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
            });
        let opened_now = p.frame == self.frames;
        if let Some(i) = chosen {
            let p = self.fix_popup.take().expect("shown above");
            self.apply_fix(p.fixes[i].clone());
        } else if !opened_now && area.response.clicked_elsewhere() {
            self.fix_popup = None;
        }
    }

    // ---------- live templates ----------

    /// Replaces `start..end` with the template, indented like its line, and adds its imports.
    pub(super) fn expand_template(&mut self, start: usize, end: usize, t: &Template) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let line = tab.text[..start].rfind('\n').map_or(0, |i| i + 1);
        let indent: String = tab.text[line..start]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let (body, caret) = android_kit::expand(t, &indent);
        tab.text.replace_range(start..end, &body);
        let mut cursor = start + caret;
        add_imports(&mut tab.text, t.imports, &mut cursor);
        tab.goto = Some(cursor);
        tab.reanalyse();
        let path = tab.path.clone();
        self.index.update(&path, &self.tabs[self.active].text);
        if let Some(l) = self.lsp.as_mut() {
            l.did_change(&path, &self.tabs[self.active].text);
        }
    }

    /// Tab right after a template name (`comp`, `vm`, `rem`…) in a Kotlin file expands it.
    pub(super) fn template_key(&mut self, ui: &mut Ui) -> bool {
        let Some(t) = self.tabs.get(self.active) else {
            return false;
        };
        if !t.path.extension().is_some_and(|e| e == "kt") || t.selection.start != t.selection.end {
            return false;
        }
        let (start, end) = word_bounds(&t.text, t.cursor);
        let Some(tpl) = android_kit::template(&t.text[start..t.cursor.min(t.text.len())])
            .filter(|_| end == t.cursor && !t.text[..start].ends_with('.'))
        else {
            return false;
        };
        if !ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            return false;
        }
        self.expand_template(start, end, tpl);
        true
    }

    // ---------- new file ----------

    /// Opens the New file window for `dir`, else the open file's folder.
    pub(super) fn new_file(&mut self, dir: Option<PathBuf>) {
        let dir = dir.or_else(|| {
            self.tabs
                .get(self.active)
                .and_then(|t| t.path.parent().map(Path::to_path_buf))
        });
        let Some(dir) = dir.filter(|_| self.project.is_some()) else {
            self.status = "Open a file first, or right-click a file in the Project tree".into();
            return;
        };
        self.new_file_window = Some(NewFile {
            dir,
            name: String::new(),
            kind: NewKind::Screen,
            error: None,
            focused: false,
        });
    }

    pub(super) fn new_file_ui(&mut self, ctx: &egui::Context) {
        let Some(nf) = self.new_file_window.as_mut() else {
            return;
        };
        let package = android_kit::package_of(&nf.dir);
        let (mut create, mut cancel) = (false, false);
        egui::Window::new(format!("{}  New Kotlin file", ic::FILE_PLUS))
            .id(egui::Id::unique("new_file"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 90.0])
            .show(ctx, |ui| {
                ui.set_width(380.0);
                let place = if package.is_empty() {
                    nf.dir.display().to_string()
                } else {
                    format!("package {package}")
                };
                ui.label(RichText::new(place).color(theme::t().weak).small());
                let r = ui.add(
                    egui::TextEdit::singleline(&mut nf.name)
                        .hint_text("Name, e.g. Home")
                        .desired_width(f32::INFINITY),
                );
                if !nf.focused {
                    r.request_focus();
                    nf.focused = true;
                }
                ui.add_space(4.0);
                for k in NewKind::ALL {
                    if ui.selectable_label(nf.kind == k, k.label()).clicked() {
                        nf.kind = k;
                    }
                }
                ui.add_space(4.0);
                if android_kit::valid_name(&nf.name) {
                    let file = format!("{}.kt", nf.kind.class_name(&nf.name));
                    ui.label(RichText::new(format!("→ {file}")).monospace().small());
                }
                if let Some(e) = &nf.error {
                    ui.label(RichText::new(e).color(crate::highlight::ERROR).small());
                }
                ui.horizontal(|ui| {
                    create =
                        ui.button("Create").clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                    cancel =
                        ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
                });
            });
        if cancel {
            self.new_file_window = None;
        } else if create {
            self.create_file();
        }
    }

    fn create_file(&mut self) {
        let Some(nf) = self.new_file_window.as_mut() else {
            return;
        };
        if !android_kit::valid_name(&nf.name) {
            nf.error = Some("Use letters, digits and _, starting with a letter".into());
            return;
        }
        let class = nf.kind.class_name(&nf.name);
        let path = nf.dir.join(format!("{class}.kt"));
        if path.exists() {
            nf.error = Some(format!("{class}.kt is there already"));
            return;
        }
        let package = android_kit::package_of(&nf.dir);
        let _ = std::fs::create_dir_all(&nf.dir);
        if let Err(e) = std::fs::write(&path, nf.kind.text(&package, &class)) {
            nf.error = Some(format!("Could not write the file: {e}"));
            return;
        }
        let kind = nf.kind;
        self.new_file_window = None;
        self.status = format!("Created {class}.kt");
        if kind == NewKind::Activity {
            self.add_to_manifest(&path, &package, &class);
        }
        self.open_file(&path, None);
    }

    /// Declares a new Activity in its module's main manifest.
    fn add_to_manifest(&mut self, file: &Path, package: &str, class: &str) {
        let Some(m) = self.project.as_ref().and_then(|p| p.module_of(file)) else {
            return;
        };
        let manifest = m.dir.join("src/main/AndroidManifest.xml");
        let fq = if package.is_empty() {
            class.to_string()
        } else {
            format!("{package}.{class}")
        };
        let added = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|t| android_kit::add_activity(&t, &fq));
        match added.map(|t| std::fs::write(&manifest, t)) {
            Some(Ok(())) => {
                self.status = format!("Created {class}.kt and added it to the manifest")
            }
            _ => {
                self.status = format!("Created {class}.kt; add it to AndroidManifest.xml yourself")
            }
        }
    }

    // ---------- build variants ----------

    /// Reads the app module's build variants (on open and when the picker opens).
    pub(super) fn load_variants(&mut self) {
        let Some(p) = &self.project else { return };
        let Some((m, _)) = crate::device::app_modules(p).into_iter().next() else {
            self.variants.clear();
            return;
        };
        self.variants = android_kit::variants(&build_text(&m));
        if !self.variants.contains(&self.variant) {
            self.variant = self
                .variants
                .first()
                .cloned()
                .unwrap_or_else(|| "debug".into());
        }
    }

    /// The build variant picker, next to the device picker. ▶ App installs this variant, and
    /// unit tests run for it.
    pub(super) fn variant_combo(&mut self, ui: &mut Ui) {
        if self.variants.is_empty() {
            return;
        }
        let r = egui::ComboBox::from_id_salt("build_variant")
            .selected_text(self.variant.clone())
            .width(110.0)
            .show_ui(ui, |ui| {
                for v in self.variants.clone() {
                    if ui.selectable_label(self.variant == v, &v).clicked() {
                        self.variant = v;
                        self.save_session(true);
                    }
                }
            });
        super::run_ui::name_combo(&r.response, "Build variant");
        if r.response.clicked() {
            self.load_variants();
        }
        r.response
            .on_hover_text("Build variant: ▶ App installs it, and unit tests run for it");
    }

    // ---------- JUnit ----------

    /// Runs a test class or one test with Gradle: unit tests on the Mac, `androidTest` on the device.
    pub(super) fn run_junit(&mut self, t: &TestTarget, ctx: &egui::Context) {
        let Some(path) = self.tabs.get(self.active).map(|t| t.path.clone()) else {
            return;
        };
        let Some(m) = self
            .project
            .as_ref()
            .and_then(|p| p.module_of(&path))
            .cloned()
        else {
            self.status = "This file is not in a Gradle module".into();
            return;
        };
        let task = android_kit::test_task(&m.name, is_android(&build_text(&m)), &self.variant, t);
        let serial = self.logcat.device.clone().filter(|_| t.device);
        if t.device && serial.is_none() {
            self.status = "No device: plug one in or run adb connect".into();
            return;
        }
        self.run.pending = None; // a test run never starts the app afterwards
        self.run_gradle_on(task, serial.as_deref(), ctx);
    }

    // ---------- crashes ----------

    /// A crash of the app's process: a note that opens the first frame in the project.
    pub(super) fn crash_note(&mut self, c: Crash) {
        let frame = c
            .frames
            .iter()
            .find_map(|(pkg, file, line)| Some((self.frame_path(pkg, file)?, *line)));
        let text = match &frame {
            Some((p, line)) => format!(
                "App crashed: {} at {}:{line}",
                c.short(),
                p.file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into_owned())
            ),
            None => format!("App crashed: {}", c.short()),
        };
        self.toast_to(text, frame, Bottom::Logcat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_go_in_once_and_move_the_cursor() {
        let mut text =
            "package com.app\n\nimport androidx.compose.ui.*\n\nfun f() {}\n".to_string();
        let mut cursor = text.find("fun").unwrap();
        add_imports(
            &mut text,
            &[
                "androidx.compose.runtime.remember",
                "androidx.compose.ui.Modifier", // covered by the wildcard
                "com.app.Other",                // same package
                "androidx.compose.runtime.remember",
            ],
            &mut cursor,
        );
        assert_eq!(
            text,
            "package com.app\n\nimport androidx.compose.ui.*\nimport androidx.compose.runtime.remember\n\nfun f() {}\n"
        );
        assert_eq!(&text[cursor..cursor + 3], "fun");
    }

    #[test]
    fn file_icons_follow_what_the_file_declares() {
        let icon =
            |path: &str, src: &str| kind_icon(Path::new(path), Some(&crate::kotlin::scan(src))).0;
        assert_eq!(
            icon(
                "/a/src/main/kotlin/Main.kt",
                "class Main : ComponentActivity()"
            ),
            ic::APP_WINDOW
        );
        assert_eq!(
            icon(
                "/a/src/main/kotlin/HomeViewModel.kt",
                "class HomeViewModel : ViewModel()"
            ),
            ic::CIRCUITRY
        );
        assert_eq!(
            icon("/a/src/main/kotlin/Home.kt", "@Composable\nfun Home() {}"),
            ic::LAYOUT
        );
        assert_eq!(
            icon("/a/src/test/kotlin/HomeTest.kt", "class HomeTest"),
            ic::FLASK
        );
        assert_eq!(
            icon("/a/src/main/kotlin/Api.kt", "interface Api"),
            ic::DIAMOND
        );
        assert_eq!(
            icon("/a/src/main/kotlin/Util.kt", "fun util() {}"),
            ic::FILE_CODE
        );
    }
}
