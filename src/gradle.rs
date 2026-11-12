//! Runs Gradle tasks in the background, and finds the preview screenshot tool a module uses.

use crate::project::{Module, Project};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

pub enum Line {
    Out(String),
    Done(Option<i32>),
}

pub struct Run {
    child: Child,
    rx: Receiver<Line>,
}

impl Run {
    /// Starts `./gradlew <args> --console=plain` in the project root; output arrives through
    /// `poll`. `serial` makes install tasks target that device (`ANDROID_SERIAL`).
    pub fn start_on(
        project: &Project,
        args: &str,
        serial: Option<&str>,
        ctx: egui::Context,
    ) -> std::io::Result<Run> {
        let gradlew = project.gradlew();
        let mut cmd = if gradlew.exists() {
            Command::new(gradlew)
        } else {
            Command::new("gradle")
        };
        cmd.args(args.split_whitespace())
            .arg("--console=plain")
            .current_dir(&project.root)
            .envs(serial.map(|s| ("ANDROID_SERIAL", s)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let (tx, rx) = channel();
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(Line::Out(line)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
        }
        Ok(Run { child, rx })
    }

    /// New output lines, then `Done` once the process has exited.
    pub fn poll(&mut self) -> Vec<Line> {
        let mut out: Vec<Line> = self.rx.try_iter().collect();
        if let Ok(Some(status)) = self.child.try_wait() {
            out.extend(self.rx.try_iter());
            out.push(Line::Done(status.code()));
        }
        out
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Run {
    /// A dropped run (new project, app quit) must not leave Gradle running or a zombie behind.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewTool {
    /// Google's Compose Preview Screenshot Testing plugin (`com.android.compose.screenshot`).
    Screenshot,
    Paparazzi,
    Roborazzi,
}

impl PreviewTool {
    pub fn detect(module: &Module) -> Option<PreviewTool> {
        let build = ["build.gradle.kts", "build.gradle"]
            .iter()
            .find_map(|f| std::fs::read_to_string(module.dir.join(f)).ok())?;
        let build = build.to_lowercase();
        if build.contains("compose.screenshot")
            || build.contains("compose-screenshot")
            || build.contains("screenshot)")
        {
            Some(PreviewTool::Screenshot)
        } else if build.contains("paparazzi") {
            Some(PreviewTool::Paparazzi)
        } else if build.contains("roborazzi") {
            Some(PreviewTool::Roborazzi)
        } else {
            None
        }
    }

    pub fn task(self, module: &Module) -> String {
        let m = if module.name == ":" {
            String::new()
        } else {
            module.name.clone()
        };
        match self {
            PreviewTool::Screenshot => format!("{m}:updateDebugScreenshotTest"),
            PreviewTool::Paparazzi => format!("{m}:recordPaparazziDebug"),
            PreviewTool::Roborazzi => format!("{m}:recordRoborazziDebug"),
        }
    }

    fn image_dirs(self, module: &Module) -> Vec<PathBuf> {
        let d = &module.dir;
        match self {
            PreviewTool::Screenshot => vec![
                d.join("src/screenshotTestDebug/reference"),
                d.join("src/debug/screenshotTest/reference"),
                d.join("src/screenshotTest/reference"),
            ],
            PreviewTool::Paparazzi => vec![d.join("src/test/snapshots/images")],
            PreviewTool::Roborazzi => vec![
                d.join("build/outputs/roborazzi"),
                d.join("src/test/screenshots"),
                d.join("screenshots"),
            ],
        }
    }

    /// PNGs whose file name contains one of the preview function names, newest first.
    pub fn images(self, module: &Module, previews: &[String]) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf, std::time::SystemTime)> = Vec::new();
        for dir in self.image_dirs(module).into_iter().filter(|d| d.is_dir()) {
            for e in ignore::WalkBuilder::new(&dir)
                .standard_filters(false)
                .build()
                .filter_map(Result::ok)
            {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "png") {
                    continue;
                }
                let file = p
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(name) = previews
                    .iter()
                    .filter(|n| matches_preview(&file, n))
                    .max_by_key(|n| n.len())
                {
                    let time = p
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    out.push((name.clone(), p.to_path_buf(), time));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)));
        out.into_iter().map(|(n, p, _)| (n, p)).collect()
    }
}

/// A preview name matches a file name as a whole word: `UserCardPreview` matches
/// `com.ui.CardKt.UserCardPreview_0.png` but not `UserCardPreviewDark_0.png`.
fn matches_preview(file: &str, name: &str) -> bool {
    file.match_indices(name).any(|(i, _)| {
        let before = file[..i].chars().last();
        let after = file[i + name.len()..].chars().next();
        before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
    })
}

pub const SETUP_HELP: &str = "This module has no preview screenshot tool.\n\n\
Add one of these to the module's build.gradle.kts, then press Render again:\n\
- Google's Compose Preview Screenshot Testing plugin, id \"com.android.compose.screenshot\".\n  \
It renders the @Preview functions in src/screenshotTest. Setup and newest version:\n  \
https://developer.android.com/studio/preview/compose-screenshot-testing\n\
- Paparazzi (app.cash.paparazzi) or Roborazzi (io.github.takahirom.roborazzi).";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_names_match_whole_words() {
        assert!(matches_preview(
            "com.ui.CardKt.UserCardPreview_0.png",
            "UserCardPreview"
        ));
        assert!(matches_preview("UserCardPreview.png", "UserCardPreview"));
        assert!(!matches_preview(
            "UserCardPreviewDark_0.png",
            "UserCardPreview"
        ));
    }

    #[test]
    fn tasks_per_tool() {
        let m = Module {
            name: ":app".into(),
            dir: PathBuf::from("/x/app"),
        };
        assert_eq!(
            PreviewTool::Screenshot.task(&m),
            ":app:updateDebugScreenshotTest"
        );
        let root = Module {
            name: ":".into(),
            dir: PathBuf::from("/x"),
        };
        assert_eq!(PreviewTool::Paparazzi.task(&root), ":recordPaparazziDebug");
    }
}
