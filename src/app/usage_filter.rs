//! Filters for Find usages / references, like Android Studio's scope filter: hide tests, import
//! lines, XML, or any path (`-android-test`, `feature/lending`). Saved with the settings.

use super::FoundLine;
use super::comfort::Settings;
use egui::{RichText, Ui};
use std::path::Path;

/// A test source: a source set under `src/` with "test" in its name (`test`, `androidTest`,
/// `testDebug`, `testFixtures`, `commonTest`, `jvmTest`, `iosTest`), a test-only module
/// (`data-test`, `test-runner`), or a `…Test` / `…Tests` / `…Spec` file.
pub(super) fn is_test(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let source_set = parts
        .windows(2)
        .any(|w| w[0] == "src" && w[1].to_ascii_lowercase().contains("test"));
    let module = parts[..parts.len().saturating_sub(1)].iter().any(|p| {
        p.ends_with("-test")
            || p.ends_with("-tests")
            || p.ends_with("-testing")
            || p.starts_with("test-")
    });
    let stem = parts
        .last()
        .and_then(|f| f.rsplit_once('.'))
        .map_or("", |(s, _)| s);
    source_set || module || ["Test", "Tests", "Spec"].iter().any(|s| stem.ends_with(s))
}

/// Whether a found line passes the filters. `rel` is its path from the project root.
pub(super) fn keep(s: &Settings, rel: &str, f: &FoundLine) -> bool {
    if s.hide_tests && is_test(rel) {
        return false;
    }
    if s.hide_imports && f.text.trim_start().starts_with("import ") {
        return false;
    }
    if s.hide_xml && rel.ends_with(".xml") {
        return false;
    }
    let terms: Vec<&str> = s.usage_path.split_whitespace().collect();
    if terms
        .iter()
        .filter_map(|t| t.strip_prefix('-'))
        .any(|t| !t.is_empty() && rel.contains(t))
    {
        return false;
    }
    let wanted: Vec<&&str> = terms.iter().filter(|t| !t.starts_with('-')).collect();
    wanted.is_empty() || wanted.iter().any(|t| rel.contains(**t))
}

/// The lines of `found` that pass, as indices.
pub(super) fn apply(s: &Settings, root: &Path, found: &[FoundLine]) -> Vec<usize> {
    found
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let rel = f
                .path
                .strip_prefix(root)
                .unwrap_or(&f.path)
                .to_string_lossy();
            keep(s, &rel, f)
        })
        .map(|(i, _)| i)
        .collect()
}

/// The filter bar. Returns true when a filter changed (the settings are saved then).
pub(super) fn bar(ui: &mut Ui, s: &mut Settings, hidden: usize) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui
            .toggle_value(&mut s.hide_tests, "Hide tests")
            .on_hover_text(
                "test, androidTest, commonTest… source sets, *-test modules, *Test files",
            )
            .changed();
        changed |= ui
            .toggle_value(&mut s.hide_imports, "Hide imports")
            .changed();
        changed |= ui.toggle_value(&mut s.hide_xml, "Hide XML").changed();
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut s.usage_path)
                    .desired_width(220.0)
                    .hint_text("path: feature/  -android-test"),
            )
            .on_hover_text("Words a path must contain; a word starting with - hides those paths")
            .changed();
        if hidden > 0 {
            ui.label(RichText::new(format!("{hidden} hidden")).color(crate::theme::t().weak));
        }
    });
    if changed {
        s.save();
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_test_sources_in_android_and_kmp_projects() {
        for t in [
            "app/src/test/kotlin/a/LoginViewModelTest.kt",
            "app/src/androidTest/java/a/Flow.kt",
            "app/src/testDebug/kotlin/a/X.kt",
            "core/src/testFixtures/kotlin/a/Fakes.kt",
            "shared/src/commonTest/kotlin/a/X.kt",
            "shared/src/iosTest/kotlin/a/X.kt",
            "feature/card/data-test/src/main/kotlin/a/FakeRepo.kt",
            "android-test/test-runner/src/main/kotlin/a/Runner.kt",
            "app/src/main/kotlin/a/ParserSpec.kt",
        ] {
            assert!(is_test(t), "{t}");
        }
        for t in [
            "app/src/main/kotlin/a/LoginScreen.kt",
            "shared/src/commonMain/kotlin/a/Contest.kt",
            "feature/latest/src/main/kotlin/a/Latest.kt",
            "app/src/main/res/layout/test_mode.xml",
        ] {
            assert!(!is_test(t), "{t}");
        }
    }

    #[test]
    fn filters_tests_imports_xml_and_paths() {
        let line = |text: &str| FoundLine {
            path: "x".into(),
            offset: 0,
            line: 1,
            text: text.into(),
        };
        let mut s = Settings::default();
        let main = "feature/card/src/main/kotlin/a/A.kt";
        let test = "feature/card/src/test/kotlin/a/ATest.kt";
        assert!(keep(&s, test, &line("a()")), "nothing hidden by default");
        s.hide_tests = true;
        assert!(!keep(&s, test, &line("a()")) && keep(&s, main, &line("a()")));
        s.hide_imports = true;
        assert!(!keep(&s, main, &line("import a.b.a")));
        s.hide_xml = true;
        assert!(!keep(&s, "app/src/main/res/layout/a.xml", &line("<a/>")));
        s.usage_path = "-feature/card".into();
        assert!(!keep(&s, main, &line("a()")));
        s.usage_path = "feature/lending".into();
        assert!(!keep(&s, main, &line("a()")));
        assert!(keep(
            &s,
            "feature/lending/src/main/kotlin/B.kt",
            &line("a()")
        ));
    }
}
