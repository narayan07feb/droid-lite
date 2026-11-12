//! Watches the project folder (FSEvents on macOS) and reports changed paths.

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: Receiver<PathBuf>,
}

/// Build output, Gradle caches and VCS folders change all the time and are not shown.
pub fn ignored(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("build" | ".gradle" | ".git" | ".idea" | ".cxx" | "node_modules" | ".kotlin")
        )
    })
}

impl Watcher {
    pub fn start(root: &Path, ctx: egui::Context) -> Option<Watcher> {
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if ev.kind.is_access() {
                return;
            }
            let mut any = false;
            for p in ev.paths {
                // stability dumps and compiler reports live under build/ but matter
                let report = p.extension().is_some_and(|e| e == "stability")
                    || p.to_string_lossy().ends_with("-composables.txt");
                if report || !ignored(&p) {
                    any |= tx.send(p).is_ok();
                }
            }
            if any {
                ctx.request_repaint();
            }
        })
        .ok()?;
        inner.watch(root, RecursiveMode::Recursive).ok()?;
        Some(Watcher { _inner: inner, rx })
    }

    /// Paths changed since the last call, without duplicates.
    pub fn changed(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.rx.try_iter().collect();
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_build_and_vcs_folders() {
        assert!(ignored(Path::new("/p/app/build/tmp/a.kt")));
        assert!(ignored(Path::new("/p/.git/index")));
        assert!(!ignored(Path::new("/p/app/src/main/java/Builder.kt")));
    }

    #[test]
    fn reports_a_changed_file() {
        let dir = std::env::temp_dir().join(format!("droid-lite-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let w = Watcher::start(&dir, egui::Context::default()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(dir.join("A.kt"), "class A").unwrap();
        let mut seen = false;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if w.changed().iter().any(|p| p.ends_with("A.kt")) {
                seen = true;
                break;
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(seen, "no event for A.kt");
    }
}
