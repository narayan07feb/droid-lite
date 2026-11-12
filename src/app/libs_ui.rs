//! Library symbols in the app: indexing in the background, search hits, completion items, and
//! read-only tabs for library sources.

use super::App;
use crate::index::{self, Hit};
use crate::libs::{self, LibSym};
use std::path::PathBuf;
use std::sync::mpsc::channel;

impl App {
    /// Indexes the project's libraries from the Gradle cache on a worker thread.
    pub(super) fn start_lib_index(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let root = p.root.clone();
        let scripts: Vec<PathBuf> = p
            .files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "kts" || e == "gradle"))
            .cloned()
            .collect();
        let (tx, rx) = channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(libs::index(&root, &scripts));
            c.request_repaint();
        });
        self.libs_rx = Some(rx);
    }

    pub(super) fn poll_libs(&mut self) {
        if let Some(rx) = &self.libs_rx
            && let Ok(l) = rx.try_recv()
        {
            self.status = format!(
                "Indexed {} library symbols from {} artifacts",
                l.syms.len(),
                l.artifacts
            );
            self.libs = l;
            self.libs_rx = None;
            self.index.generation += 1; // UI-call detection and annotations may change
        }
    }

    /// Library hits for Go to class / symbol, ranked like project hits.
    pub(super) fn lib_hits(&self, q: &str, types_only: bool, limit: usize) -> Vec<Hit> {
        if q.is_empty() {
            return Vec::new();
        }
        let items = self
            .libs
            .syms
            .iter()
            .filter(|s| !types_only || s.kind.is_type());
        index::rank_by(q, items, |s| &s.name, limit)
            .into_iter()
            .map(|(score, s)| lib_hit(s, score))
            .collect()
    }
}

pub(super) fn lib_hit(s: &LibSym, score: u32) -> Hit {
    let what = if s.composable {
        "@Composable fun"
    } else {
        s.kind.label()
    };
    Hit {
        name: s.name.to_string(),
        detail: format!("{what}  {}  · {}", s.package, s.artifact),
        path: s.source_path().unwrap_or_else(|| s.archive.to_path_buf()),
        offset: 0,
        score,
    }
}

/// A library file opened from a hit: archives without sources are not opened as text.
pub(super) fn is_binary_archive(path: &std::path::Path) -> bool {
    !libs::is_archive_path(path) && path.extension().is_some_and(|e| e == "jar" || e == "aar")
}
